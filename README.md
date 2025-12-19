# Xremap Socket Manager

`xremap-socket` allows xremap to be configured and run as a system user with
limited permissions while maintaining the ability to query the active window in
GNOME. No running xremap as root or adding users to the input group.

User sessions are monitored for changes, and the socket used to query the active
application switches automatically as users log in, log out, switch users, etc.

## System configuration to run xremap as a dedicated system user

For each user account that will use xremap mappings, install the
[Xremap GNOME extension](https://extensions.gnome.org/extension/5060/xremap/)
(v12 or later). It can be installed system-wide (for all users) in
`/usr/share/gnome-shell/extensions/xremap@k0kubun.com`.

### Install `xremap-socket`

Currently it must be built from source.

```sh
git clone https://github.com/millerdev/xremap-socket.git
cargo build --release
sudo cp target/release/xremap-socket /usr/local/bin/
sudo chown root:root /usr/local/bin/xremap-socket
```

Alternately, the Python version ([xremap-socket.py](https://github.com/millerdev/xremap-socket/blob/main/xremap-socket.py))
is a drop-in replacement. It requires `dbus-next`, which may need to be
installed if your system does not already have it.

### Install the GNOME variant of xremap v0.14.6 or later

Use the best [installation procedure](https://github.com/xremap/xremap?tab=readme-ov-file#Installation)
for your system.

### Create xremap configuration file: _/etc/xremap/config.yml_

Once created, ensure the config file has proper ownership and permissions:

```sh
sudo chown root:root /etc/xremap/config.yml
sudo chmod 644 /etc/xremap/config.yml
```

### Create _xremap_ user and groups

```sh
# Add system xremap user
sudo useradd --system --no-create-home --shell=/bin/false xremap

# Add a xremap-{username} group for each user that will use xremap.
# Replace {usernameN} with the username.
sudo groupadd --system xremap-{username1}
sudo groupadd --system xremap-{username2}
sudo groupadd --system xremap-{username3}

# Add each user to its respective xremap-{username} group
sudo usermod --append --groups xremap-{username1} {username1}
sudo usermod --append --groups xremap-{username2} {username2}
sudo usermod --append --groups xremap-{username3} {username3}
```

Note: xremap will be enabled for any user that does not have a corresponding
`xremap-{username}` group, but application- and window-specific mappings
will not function.

### Create systemd unit files

_/etc/systemd/system/xremap.service_
```ini
[Unit]
Description=Xremap keyboard mapper
Wants=xremap-socket.service
After=xremap-socket.service

[Service]
ExecStart=/usr/bin/xremap --watch=config,device /etc/xremap/config.yml
Environment=GNOME_SOCKET=/run/xremap/gnome.sock
User=xremap
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

_/etc/systemd/system/xremap-socket.service_
```ini
[Unit]
Description=Xremap socket manager

[Service]
ExecStart=/usr/local/bin/xremap-socket
User=xremap
Restart=always
RuntimeDirectory=xremap
RuntimeDirectoryMode=0755
RuntimeDirectoryPreserve=yes

# Add two lines for each xremap user, replacing {username} and {uid} for each
#ExecStartPre=install --directory --mode 2770 --owner xremap --group xremap-{username} /run/xremap/{uid}
#SupplementaryGroups=xremap-{username}

# Hardening
NoExecPaths=/
ExecPaths=/usr
ReadWritePaths=/run/xremap
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ProtectKernelModules=yes
ProtectKernelTunables=yes
ProtectControlGroups=yes
```

### Enable systemd units

```sh
sudo systemctl enable --now xremap.service
```

Restart your user session (logout) to reload the GNOME extension.

### Troubleshooting

Verify that `/run/xremap/$UID` exists and permissions are correct.

Xremap logs can be viewed with `journalctl`

```sh
journalctl -fu xremap.service --since="1 hour ago"
journalctl -fu xremap-socket.service --since="1 hour ago"
journalctl -f /usr/bin/gnome-shell --since="1 hour ago" | grep Xremap
```

For more detailed logging, do any or all of the following:

Stop `xremap.service` and run it in a shell with debug logging enabled. Log
output will indicate if GNOME support is enabled.

```sh
sudo systemctl stop xremap.service
sudo systemd-run -t -p User=xremap -p SupplementaryGroups=input \
    -p Environment=GNOME_SOCKET=/run/xremap/gnome.sock \
    -p Environment=RUST_LOG=debug  \
    xremap --watch=config,device /etc/xremap/config.yml
```

Stop `xremap-socket` and run it in a shell with verbose logging. Log output will
indicate when key events cause xremap to query the active application or window.

```sh
sudo systemctl stop xremap-socket.service
sudo systemd-run -t -p User=xremap -p Group=xremap-$USER xremap-socket -vv
```

Disable and re-enable the xremap-gnome extension to restart the socket server.
This should produce a line of `journalctl` output like

```log
... [Xremap] Socket server listening on /run/xremap/1000/gnome.sock
```

----

Tested on Fedora 42 (GNOME 48) and Fedora 43 (GNOME 49).
