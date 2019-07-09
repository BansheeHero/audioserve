Installation notes for RED HAT based linux: CentOS 7
==============

It seems nobody bothered to set-up a RPM build scripts for this project.
For now I am just going to compile notes on installation.
Once I am satisfied with the steps and structure I will try to build a proper RPM scripts.

The document assumes you have working FFmpeg if you want to transcode files.
I am not going to include the steps here.

Things to contemplate:
 - Fail2Ban integration
 - FirewallD
 - SELinux
 - Proper directory structure (leaving /opt)

Preparing the environment
-------------

```bash
useradd audioserve 
passwd -d audioserve 
mkdir -p /opt/audioserve 
chown root:audioserve /opt/audioserve 
#chmod g+w /opt/audioserve 
#mkdir -p /opt/audioserve/data/ 
touch /etc/audioserve && chmod o-rwx /etc/audioserve && chgrp audioserve /etc/audioserve 
```
Firewall
-------------

Creating the service and rules:

```bash
firewall-cmd --permanent --new-service=audioserve
firewall-cmd --permanent --service=audioserve --set-short=as
firewall-cmd --permanent --service=audioserve --set-description="audioserve streaming service for books"
firewall-cmd --permanent --service=audioserve --add-port=3000/tcp
```

Enabling the rules:

```bash
firewall-cmd --add-service=audioserve --permanent
firewall-cmd --reload
firewall-cmd --list-all
```

Downloading and extracting the static release
-------------

```bash
wget https://github.com/izderadicka/audioserve/releases/download/v0.11.1/audioserve_static.tar.gz 
tar -xzf audioserve_static.tar.gz -C /opt/audioserve --strip-components=1 
```

Dummy HTTPS
-------------

I do not recommend using this as Android and iOS will block the downloads.
The HTTPS setup requires a bit mroe research into options and prices.

```bash
openssl genrsa -out audioserve.pem 
openssl req -new -key audioserve.pem -out audioserve.csr 
openssl x509 -req -in audioserve.csr -signkey audioserve.pem -out audioserve.crt 
openssl pkcs12 -export -in audioserve.crt -inkey audioserve.pem -out /etc/audioserve.p12 
```

Creating default config file: 
-------------
 - Configuration is to be printed, not executed. Cache and Symlinks are enabled for me.
 - Shared secret (password)
 - Adding 3 collections: /mnt/ab/cz /mnt/ab/en and /mnt/ab/digest

```bash
su audioserve -c "/opt/audioserve/audioserve --print-config --allow-symlinks --search-cache \
  --shared-secret password \
  --client-dir /opt/audioserve/client/dist/ \
  /mnt/ab/cz /mnt/ab/en /mnt/ab/digest" > /etc/audioserve  

```

For SSL you have to add 2 options line before the last line:

```bash
su audioserve -c "/opt/audioserve/audioserve --print-config --allow-symlinks --search-cache \
  --shared-secret password \
  --ssl-key /etc/audioserve.p12 --ssl-key-password password \
  --client-dir /opt/audioserve/client/dist/ \
  /mnt/ab/cz /mnt/ab/en /mnt/ab/digest" > /etc/audioserve  
```
 
Testrun
-------------

Now is the time to test the configuration and if you are satisfied press ctrl+C ti abort.

```bash
su audioserve -c "/opt/audioserve/audioserve -d --config /etc/audioserve" 
```

If you have enabled HTTPS but did not enter HTTPS into the browser, audioserve will not redirect you.
Instead it will display the following error:

```ERROR audioserve                   > TLS error: error:1408F09C:SSL routines:ssl3_get_record:http request:ssl/record/ssl3_record.c:322```

Creating the service: 
-------------
 
```bash
echo "[Unit]
Description=Audioserve Service
After=network.target
[Service]
Type=simple
User=audioserve
ExecStart=/opt/audioserve/audioserve -d --config /etc/audioserve 
Restart=on-abort 
[Install]
WantedBy=multi-user.target" > /etc/systemd/system/audioserve.service 
sudo systemctl daemon-reload 
sudo systemctl start audioserve
sudo systemctl enable audioserve
sudo systemctl status audioserve
```

Update and downgrade
-------------

since the app is a single binary it is likely ok to just replace it.
If there are any issues check the release history for clues (Config, cache changes for example)

```bash
sudo systemctl stop audioserve
wget https://github.com/izderadicka/audioserve/releases/download/v0.11.1/audioserve_static.tar.gz 
tar -xzf audioserve_static.tar.gz -C /opt/audioserve --strip-components=1 
sudo systemctl start audioserve
```
