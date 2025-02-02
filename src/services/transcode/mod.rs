use self::codecs::*;
use super::subs::ChunkStream;
use super::types::AudioFormat;
use crate::config::get_config;
use crate::error::Error;
use futures::future::Either;
use futures::Future;
use mime::Mime;
use std::ffi::OsStr;
use std::fmt::Debug;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::timer::Delay;
use tokio_process::{ChildStdout, CommandExt};

#[cfg(feature = "transcoding-cache")]
pub mod cache;
pub mod codecs;

pub trait AudioCodec {
    fn quality_args(&self) -> Vec<String>;
    fn codec_args(&self) -> &'static [&'static str];
    /// in kbps
    fn bitrate(&self) -> u32;
    fn transcode_from(&self) -> u32 {
        (f64::from(self.bitrate()) * 1.2) as u32
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum TranscodingFormat {
    OpusInOgg(Opus),
    OpusInWebm(Opus),
    Mp3(Mp3),
    AacInAdts(Aac),
    Remux,
}

pub struct TranscodingArgs {
    format: &'static str,
    codec_args: &'static [&'static str],
    quality_args: Vec<String>,
}

macro_rules! targs {
    ($n:ident, $f:expr) => {
        TranscodingArgs {
            format: $f,
            codec_args: $n.codec_args(),
            quality_args: $n.quality_args(),
        }
    };
}

impl TranscodingFormat {
    pub fn args(&self) -> TranscodingArgs {
        match self {
            TranscodingFormat::OpusInOgg(args) => targs!(args, "opus"),
            TranscodingFormat::OpusInWebm(args) => targs!(args, "webm"),
            TranscodingFormat::Mp3(args) => targs!(args, "mp3"),
            TranscodingFormat::AacInAdts(args) => targs!(args, "adts"),
            TranscodingFormat::Remux => TranscodingArgs {
                format: "",
                codec_args: &[],
                quality_args: vec![],
            },
        }
    }

    pub fn bitrate(&self) -> u32 {
        match self {
            TranscodingFormat::OpusInOgg(args) => args.bitrate(),
            TranscodingFormat::OpusInWebm(args) => args.bitrate(),
            TranscodingFormat::Mp3(args) => args.bitrate(),
            TranscodingFormat::AacInAdts(args) => args.bitrate(),
            TranscodingFormat::Remux => 0,
        }
    }

    pub fn format_name(&self) -> &'static str {
        match self {
            TranscodingFormat::OpusInOgg(_) => "opus-in-ogg",
            TranscodingFormat::OpusInWebm(_) => "opus-in-webm",
            TranscodingFormat::Mp3(_) => "mp3",
            TranscodingFormat::AacInAdts(_) => "aac-in-adts",
            TranscodingFormat::Remux => "remux",
        }
    }

    pub fn mime(&self) -> Mime {
        let m = match self {
            TranscodingFormat::OpusInOgg(_) => "audio/ogg",
            TranscodingFormat::OpusInWebm(_) => "audio/webm",
            TranscodingFormat::Mp3(_) => "audio/mpeg",
            TranscodingFormat::AacInAdts(_) => "audio/aac",
            TranscodingFormat::Remux => unreachable!("mime for Remux should never be used!"),
        };
        m.parse().unwrap()
    }
}

impl TranscodingFormat {
    pub fn default_level(l: QualityLevel) -> Self {
        match l {
            QualityLevel::Low => {
                TranscodingFormat::OpusInOgg(Opus::new(32, 5, Bandwidth::SuperWideBand, true))
            }
            QualityLevel::Medium => {
                TranscodingFormat::OpusInOgg(Opus::new(48, 8, Bandwidth::SuperWideBand, false))
            }
            QualityLevel::High => {
                TranscodingFormat::OpusInOgg(Opus::new(64, 10, Bandwidth::FullBand, false))
            }
            QualityLevel::Passthrough => TranscodingFormat::Remux,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum QualityLevel {
    Low,
    Medium,
    High,
    Passthrough,
}

impl QualityLevel {
    pub fn from_letter<T: AsRef<str>>(l: &T) -> Option<Self> {
        use self::QualityLevel::*;
        let s: &str = l.as_ref();
        match s {
            "l" => Some(Low),
            "m" => Some(Medium),
            "h" => Some(High),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn to_letter(self) -> &'static str {
        use self::QualityLevel::*;
        match self {
            Low => "l",
            Medium => "m",
            High => "h",
            Passthrough => "p",
        }
    }
}

#[derive(Clone, Debug)]
pub enum AudioFilePath<S> {
    Original(S),
    #[allow(dead_code)]
    Transcoded(S),
}

impl<S> std::convert::AsRef<S> for AudioFilePath<S> {
    fn as_ref(&self) -> &S {
        use self::AudioFilePath::*;
        match self {
            Original(ref f) => f,
            Transcoded(ref f) => f,
        }
    }
}

// part of audio file - from start to start+duration (in ms)
#[derive(Clone, Copy, Debug)]
pub struct TimeSpan {
    pub start: u64,
    pub duration: Option<u64>,
}

impl std::fmt::Display for TimeSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        match self.duration {
            Some(d) => write!(f, "{}-{}", self.start, d),
            None => write!(f, "{}", self.start),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Transcoder {
    quality: TranscodingFormat,
}

#[cfg(feature = "transcoding-cache")]
type TranscodedStream =
    Box<dyn futures::Stream<Item = Vec<u8>, Error = std::io::Error> + Send + 'static>;
#[cfg(feature = "transcoding-cache")]
type TranscodedFuture = Box<dyn Future<Item = TranscodedStream, Error = Error> + Send>;

impl Transcoder {
    pub fn new(quality: TranscodingFormat) -> Self {
        Transcoder { quality }
    }

    fn base_ffmpeg(&self, seek: Option<f32>, span: Option<TimeSpan>) -> Command {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(&["-nostdin", "-v", "error"]);
        let offset = span.as_ref().map(|s| s.start).unwrap_or(0) as f32;
        let time = span.and_then(|s| s.duration).unwrap_or(0);
        let seek = seek.unwrap_or(0f32);
        let start = offset as f32 / 1000.0 + seek;

        if start > 0.0 {
            cmd.args(&["-accurate_seek", "-ss"]);
            let time_spec = format!("{:3}", start);
            cmd.arg(time_spec);
        }

        if time > 0 {
            cmd.arg("-t");
            let mut t = time as f32 / 1000.0 - seek;
            if t < 0.0 {
                t = 0.0
            };
            cmd.arg(format!("{:3}", t));
        }

        cmd
    }

    fn input_file_args<S: AsRef<OsStr>>(&self, cmd: &mut Command, file: S) {
        cmd.arg("-i").arg(file).args(&[
            "-y",
            "-map_metadata",
            "-1", // removing metadata as we do not need them
            "-map",
            "a", // and we need only audio stream
        ]);
    }

    // ffmpeg -nostdin -v error -i 01-file.mp3 -y -map_metadata 0 -map a -acodec libopus \
    // -b:a 48k -vbr on -compression_level 10 -application audio -cutoff 12000 -f opus pipe:1
    fn build_command<S: AsRef<OsStr>>(
        &self,
        file: S,
        seek: Option<f32>,
        span: Option<TimeSpan>,
    ) -> Command {
        let mut cmd = self.base_ffmpeg(seek, span);
        let targs = self.quality.args();
        self.input_file_args(&mut cmd, file);
        cmd.args(targs.codec_args)
            .args(targs.quality_args)
            .arg("-f")
            .arg(targs.format)
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd
    }

    // should not transcode, just copy audio stream
    #[allow(dead_code)]
    fn build_remux_command<S: AsRef<OsStr>>(
        &self,
        file: S,
        seek: Option<f32>,
        span: Option<TimeSpan>,
        use_transcoding_format: bool,
    ) -> Command {
        let mut cmd = self.base_ffmpeg(seek, span);
        let fmt = if !use_transcoding_format {
            guess_format(file.as_ref()).ffmpeg
        } else {
            self.quality.args().format
        };
        self.input_file_args(&mut cmd, file);
        cmd.args(&["-acodec", "copy"])
            .arg("-f")
            .arg(fmt)
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd
    }

    pub fn transcoding_params(&self) -> String {
        format!(
            "codec={}; bitrate={}",
            self.quality.format_name(),
            self.quality.bitrate(),
        )
    }

    pub fn transcoded_mime(&self) -> Mime {
        self.quality.mime()
    }

    #[cfg(not(feature = "transcoding-cache"))]
    pub fn transcode<S: AsRef<OsStr> + Send + Debug + 'static>(
        self,
        file: AudioFilePath<S>,
        seek: Option<f32>,
        span: Option<TimeSpan>,
        counter: super::Counter,
        _quality: QualityLevel,
    ) -> impl Future<Item = ChunkStream<ChildStdout>, Error = Error> {
        futures::future::result(self.transcode_inner(file, seek, span, counter).map(
            |(stream, f)| {
                tokio::spawn(f);
                stream
            },
        ))
    }

    #[cfg(feature = "transcoding-cache")]
    pub fn transcode<S: AsRef<OsStr> + Debug + Send + 'static>(
        self,
        file: AudioFilePath<S>,
        seek: Option<f32>,
        span: Option<TimeSpan>,
        counter: super::Counter,
        quality: QualityLevel,
    ) -> TranscodedFuture {
        use self::cache::{cache_key, get_cache};
        use futures::future;
        use futures::sync::mpsc;
        use futures::{Sink, Stream};
        use std::io;

        let is_transcoded = if let AudioFilePath::Transcoded(_) = file {
            true
        } else {
            false
        };
        if is_transcoded || seek.is_some() || get_config().transcoding.cache.disabled {
            debug!("Shoud not add to cache as is already transcoded, seeking or cache is disabled");
            return Box::new(future::result(
                self.transcode_inner(file, seek, span, counter)
                    .map(|(stream, f)| {
                        tokio::spawn(f);
                        Box::new(stream) as TranscodedStream
                    }),
            ));
        }

        let cache = get_cache();
        //TODO: this is ugly -  unify either we will use Path or OsStr!
        let key = cache_key(file.as_ref().as_ref(), quality, span);
        let fut = cache.add_async(key).then(move |res| match res {
            Err(e) => {
                warn!("Cannot create cache entry: {}", e);
                self.transcode_inner(file, seek, span, counter)
                    .map(|(stream, f)| {
                        tokio::spawn(f);

                        Box::new(stream) as TranscodedStream
                    })
            }
            Ok((cache_file, cache_finish)) => {
                self.transcode_inner(file, seek, span, counter)
                    .map(|(stream, f)| {
                        tokio::spawn(f.then(|res| {
                            fn box_me<I, E, F: Future<Item = I, Error = E> + 'static + Send>(
                                f: F,
                            ) -> Box<Future<Item = I, Error = E> + 'static + Send>
                            {
                                Box::new(f)
                            };

                            match res {
                                Ok(()) => box_me(
                                    cache_finish
                                        .commit()
                                        .map_err(|e| error!("Error in cache: {}", e))
                                        .and_then(|_| {
                                            debug!("Added to cache");
                                            if false {
                                                box_me(get_cache().save_index_async().map_err(
                                                    |e| {
                                                        error!(
                                                            "Error when saving cache index: {}",
                                                            e
                                                        )
                                                    },
                                                ))
                                            } else {
                                                box_me(future::ok(()))
                                            }
                                        }),
                                ),
                                Err(()) => box_me(
                                    cache_finish
                                        .roll_back()
                                        .map_err(|e| error!("Error in cache: {}", e)),
                                ),
                            }
                        }));
                        let cache_sink =
                            tokio::codec::FramedWrite::new(cache_file, self::vec_codec::VecEncoder);
                        let (tx, rx) = mpsc::channel(64);
                        let tx = cache_sink
                            .fanout(tx.sink_map_err(|e| io::Error::new(io::ErrorKind::Other, e)));
                        tokio::spawn(tx.send_all(stream).then(|res| {
                            if let Err(e) = res {
                                warn!("Error in channel: {}", e)
                            }
                            Ok(())
                        }));
                        Box::new(rx.map_err(|_| {
                            error!("Error in chanel");
                            io::Error::new(io::ErrorKind::Other, "Error in channel")
                        })) as TranscodedStream
                    })
            }
        });
        Box::new(fut)
    }

    fn transcode_inner<S: AsRef<OsStr> + Debug + Send + 'static>(
        &self,
        file: AudioFilePath<S>,
        seek: Option<f32>,
        span: Option<TimeSpan>,
        counter: super::Counter,
    ) -> Result<(ChunkStream<ChildStdout>, impl Future<Item = (), Error = ()>), Error> {
        let mut cmd = match (&file, &self.quality) {
            (_, TranscodingFormat::Remux) => {
                self.build_remux_command(file.as_ref(), seek, span, false)
            }
            (AudioFilePath::Transcoded(_), _) => {
                self.build_remux_command(file.as_ref(), seek, span, true)
            }
            _ => self.build_command(file.as_ref(), seek, span),
        };
        let counter2 = counter.clone();
        match cmd.spawn_async() {
            Ok(mut child) => {
                if child.stdout().is_some() {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let start = Instant::now();
                    let out = child.stdout().take().unwrap();
                    let stream = ChunkStream::new(out);
                    let pid = child.id();
                    debug!("waiting for transcode process to end");
                    let fut =
                        child
                        .select2(Delay::new(
                            Instant::now()
                                + Duration::from_secs(
                                    u64::from(get_config().transcoding.max_runtime_hours * 3600),
                                ),
                        ))
                        .then(move |res| {
                            counter2.fetch_sub(1, Ordering::SeqCst);
                            match res {
                                Ok(Either::A((res, _d))) => {
                                    if res.success() {
                                        debug!("Finished transcoding process of {:?} normally after {:?}",
                                    file.as_ref(),
                                    Instant::now() - start);
                                    Ok(())
                                    } else {
                                        warn!(
                                            "Transconding of file {:?} failed with code {:?}",
                                            file.as_ref(),
                                            res.code()
                                        );
                                        Err(())
                                    }
                                }
                                Ok(Either::B((_d, mut child))) => {
                                    eprintln!(
                                        "Transcoding of file {:?} took longer then deadline",
                                        file.as_ref()
                                    );
                                    child.kill().unwrap_or_else(|e| {
                                        eprintln!("Failed to kill process pid {} error {}", pid, e)
                                    });
                                    Err(())
                                }
                                Err(Either::A((e, _))) => {
                                    eprintln!(
                                        "Error running transcoding process for file {:?} error {}",
                                        file.as_ref(),
                                        e
                                    );
                                    Err(())
                                }
                                Err(Either::B((e, _))) => {
                                    eprintln!("Timer error on process pid {} error {}", pid, e);
                                    Err(())
                                }
                            }
                        });
                    Ok((stream, fut))
                } else {
                    error!("Cannot get stdout");
                    Err(Error::new_with_cause("Cannot get stdout"))
                }
            }
            Err(e) => {
                error!("Cannot spawn child process: {:?}", e);
                Err(Error::new_with_cause("Cannot spawn child"))
            }
        }
    }
}

pub fn guess_format<P: AsRef<std::path::Path>>(p: P) -> AudioFormat {
    const DEFAULT_FORMAT: (&str, &str) = ("matroska", "audio/x-matroska"); // matroska is fairly universal, so it's good chance that audio stream will fit in
    let t = match p.as_ref().extension() {
        Some(e) => {
            let e = e.to_string_lossy().to_lowercase();
            match e.as_str() {
                "opus" => ("opus", "audio/ogg"),
                "mp3" => ("mp3", "audio/mpeg"),
                "m4b" => ("adts", "audio/aac"), // we cannot create mp4 container in pipe
                "m4a" => ("adts", "audion/aac"),
                _ => DEFAULT_FORMAT,
            }
        }
        None => DEFAULT_FORMAT,
    };
    AudioFormat {
        ffmpeg: t.0,
        mime: <Mime as std::str::FromStr>::from_str(t.1).unwrap(),
    }
}

#[cfg(feature = "transcoding-cache")]
mod vec_codec {
    use bytes::BufMut;
    use std::io;
    use tokio::codec::Encoder;

    pub struct VecEncoder;

    impl Encoder for VecEncoder {
        type Item = Vec<u8>;
        type Error = io::Error;

        fn encode(
            &mut self,
            data: Self::Item,
            buf: &mut bytes::BytesMut,
        ) -> Result<(), Self::Error> {
            buf.reserve(data.len());
            buf.put(data);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::audio_meta::{get_audio_properties, MediaInfo};
    use super::*;
    use std::env::temp_dir;
    use std::fs::{remove_file, File};
    use std::io::{Read, Write};
    use std::path::Path;

    fn dummy_transcode<P: AsRef<Path>, R: AsRef<Path>>(
        output_file: P,
        seek: Option<f32>,
        copy_file: Option<R>,
        remove: bool,
        span: Option<TimeSpan>,
    ) {
        pretty_env_logger::try_init().ok();
        let t = Transcoder::new(TranscodingFormat::default_level(QualityLevel::Low));
        let out_file = temp_dir().join(output_file);
        let mut cmd = match copy_file {
            None => t.build_command("./test_data/01-file.mp3", seek, span),
            Some(ref p) => t.build_remux_command(p.as_ref(), seek, span, false),
        };
        println!("Command is {:?}", cmd);
        let mut child = cmd.spawn().expect("Cannot spawn subprocess");

        if child.stdout.is_some() {
            let mut file = File::create(&out_file).expect("Cannot create output file");
            let mut buf = [0u8; 1024];
            let mut out = child.stdout.take().unwrap();
            loop {
                match out.read(&mut buf) {
                    Ok(n) => {
                        if n == 0 {
                            break;
                        } else {
                            file.write_all(&mut buf).expect("Write to file error")
                        }
                    }
                    Err(e) => panic!("stdout read error {:?}", e),
                }
            }
        }
        let status = child.wait().expect("cannot get status");
        assert!(status.success());
        assert!(out_file.exists());
        //TODO: for some reasons sometimes cannot get meta - but file is OK
        let meta = get_audio_properties(&out_file).expect("Cannot get audio file meta");
        let audio_len = if copy_file.is_some() { 1 } else { 2 };
        let dur = audio_len - seek.map(|s| s.round() as u32).unwrap_or(0);

        match meta.get_audio_info() {
            Some(ai) => assert_eq!(ai.duration, dur),
            None => panic!("Cannot get audio info"),
        }

        if remove {
            remove_file(&out_file).expect("error deleting tmp file");
        }
    }

    #[test]
    fn test_transcode() {
        dummy_transcode(
            "audioserve_transcoded.opus",
            None,
            None as Option<&str>,
            true,
            None,
        )
    }

    #[test]
    fn test_transcode_seek() {
        dummy_transcode(
            "audioserve_transcoded2.opus",
            Some(0.8),
            None as Option<&str>,
            false,
            None,
        );
        let out_file = temp_dir().join("audioserve_transcoded2.opus");
        dummy_transcode(
            "audioserve_transcoded3.opus",
            Some(0.8),
            Some(&out_file),
            true,
            None,
        );
        remove_file(out_file).unwrap();
    }

    #[test]
    fn test_transcode_span() {
        dummy_transcode(
            "audioserve_transcoded5.opus",
            Some(0.1),
            None as Option<&str>,
            false,
            Some(TimeSpan {
                start: 100,
                duration: Some(1800),
            }),
        );
    }

}
