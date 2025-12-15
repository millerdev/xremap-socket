# xremap-socket relay

Relay messages from xremap system service to xremap-gnome GNOME extension.

Allow xremap to be configured and run as a system user with limited permissions
while maintaining the ability to query the active window of a user with an
active seated session.

Behavior is undefined when there are multiple active seated sessions.

## System configuration to run xremap as a dedicated system user

First, for each user account that will use xremap mappings, install the
[xremap-gnome extension](https://extensions.gnome.org/extension/5060/xremap/)
with socket support (v11 or later). This can be done system-wide by installing
the extension in `/usr/share/gnome-shell/extensions/xremap@k0kubun.com`.

### Create xremap configuration file: _/etc/xremap/config.yml_

Once created, ensure the file has proper ownership and permissions:

```sh
sudo chown root:root /etc/xremap/config.yml
sudo chmod 644 /etc/xremap/config.yml
```

### Create _xremap_ user and group

```sh
sudo groupadd xremap
sudo useradd --system --no-create-home --shell=/bin/false -g xremap --groups=input xremap
```

### Install the GNOME variant of xremap v0.14.6 or later

Use the best [installation procedure](https://github.com/xremap/xremap?tab=readme-ov-file#Installation)
for your system.

### Create systemd unit files

_/etc/systemd/system/xremap-socket.service_
```ini
[Unit]
Description=Xremap socket manager

[Service]
ExecStart=/usr/bin/xremap-socket
Restart=always
```

_/etc/systemd/system/xremap.service_
```ini
[Unit]
Description=Xremap keyboard mapper
Wants=xremap-socket.service

[Service]
ExecStart=/usr/bin/xremap --watch=config,device /etc/xremap/config.yml
Environment=GNOME_SOCKET=/run/xremap/gnome.sock
User=xremap
Group=xremap
RuntimeDirectory=xremap
RuntimeDirectoryMode=0700
RuntimeDirectoryPreserve=restart
SupplementaryGroups=input
Restart=always

# Hardening
NoExecPaths=/
ExecPaths=/usr
ReadWritePaths=/run/xremap /dev/input /dev/uinput
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ProtectKernelModules=yes
ProtectKernelTunables=yes
ProtectControlGroups=yes

[Install]
WantedBy=multi-user.target
```

### Enable systemd units

```sh
sudo systemctl enable --now xremap.service
```

Restart your user session (logout) to reload the GNOME extension.

----

Tested on Fedora 42 (GNOME 48) and Fedora 43 (GNOME 49).

Use this command to monitor logs for xremap-related issues:
```sh
journalctl -f --since="10 min ago" | grep -i xremap
```