# Hardening SSH on the server machine

The game server never speaks SSH: administration of the machine goes through OpenSSH, game administration
through the server's HTTPS `/v1/admin` routes. This guide locks OpenSSH down on Ubuntu 24.04: keys only, no
root login, a firewall, fail2ban. Do it before the server goes public.

**Never lock yourself out.** Keep the session you are working in open, and test every change from a
**second** terminal before closing the first. Most providers also offer a web console as a way back in.

## 1. An administrator with a key

On your own computer, create a key once (`ssh-keygen -t ed25519`), then on the machine (as root, or the
provider's first user):

```text
adduser --disabled-password --gecos "" admin          # a name of your choice
usermod -aG sudo admin
install -d -m 0700 -o admin -g admin /home/admin/.ssh
nano /home/admin/.ssh/authorized_keys                 # paste the PUBLIC key (id_ed25519.pub), one line
chown admin:admin /home/admin/.ssh/authorized_keys && chmod 0600 /home/admin/.ssh/authorized_keys
passwd admin                                          # a password for sudo only (SSH refuses passwords)
```

From a second terminal: `ssh -i ~/.ssh/id_ed25519 -o IdentitiesOnly=yes admin@api.example.com`, then
`sudo -v`. Continue only when both work. (`IdentitiesOnly` offers just that key: with several keys in an
ssh-agent, `MaxAuthTries 3` below would otherwise end with "Too many authentication failures" before the
right key is tried.)

## 2. Keys only, no root

Create `/etc/ssh/sshd_config.d/10-hardening.conf`:

```text
# Keys only.
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
AuthenticationMethods publickey
PermitEmptyPasswords no
# No direct root login (use sudo).
PermitRootLogin no
# Only these accounts may log in at all.
AllowUsers admin
# Fewer guesses per connection, a short login window, dead sessions dropped.
MaxAuthTries 3
LoginGraceTime 30
ClientAliveInterval 300
ClientAliveCountMax 2
# Nothing the server administration needs.
X11Forwarding no
AllowAgentForwarding no
AllowTcpForwarding no
PermitTunnel no
```

**Why `10-`:** sshd reads the files in `/etc/ssh/sshd_config.d/` in name order and the **first** value of a
setting wins. Cloud images often ship `50-cloud-init.conf` with `PasswordAuthentication yes`; a file named
`10-…` comes first and wins. Check and apply:

```text
sudo sshd -t                                              # syntax; no output = fine
sudo sshd -T | grep -Ei '^(passwordauthentication|kbdinteractiveauthentication|permitrootlogin|allowusers|authenticationmethods)'
sudo systemctl restart ssh
```

`sshd -T` must show `passwordauthentication no`, `kbdinteractiveauthentication no`, `permitrootlogin no`.
Test from a second terminal: your key works; `ssh -o PubkeyAuthentication=no admin@api.example.com` is
refused with `Permission denied (publickey)`.

`AllowTcpForwarding no` also blocks the tunnel an administrator would use to read the metrics on
`127.0.0.1:9100` from their own computer (`ssh -L 9100:127.0.0.1:9100 …`). If you need it, allow it for
the administrator only, at the end of the file: `Match User admin` + `AllowTcpForwarding local`.

Ubuntu 24.04 starts sshd through `ssh.socket`. Changing the **port** therefore also needs
`sudo systemctl daemon-reload && sudo systemctl restart ssh.socket` (and the new port in the firewall
first). A non-standard port reduces log noise, not risk; keys are what protect the login.

## 3. Firewall (ufw)

```text
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow OpenSSH            # 22/tcp (or your SSH port)
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw allow 443/udp            # HTTP/3
sudo ufw enable
sudo ufw status verbose
```

The server (127.0.0.1:8080), the metrics (127.0.0.1:9100) and the database listen on loopback or on
private container networks and need no rule. **Docker:** ports a container publishes bypass ufw (Docker
writes its own iptables rules); the Compose setup publishes only Caddy's 80 and 443, never the server or
the database.

## 4. fail2ban

```text
sudo apt install fail2ban
```

`/etc/fail2ban/jail.d/sshd.local`:

```text
[sshd]
enabled  = true
backend  = systemd
maxretry = 5
findtime = 10m
bantime  = 1h
```

```text
sudo systemctl restart fail2ban
sudo fail2ban-client status sshd
```

With keys only, guessing cannot succeed; fail2ban keeps the logs quiet and blocks scanners early. Add
your own fixed address to `ignoreip` in the same section if you have one.

## 5. Updates

Ubuntu installs security updates automatically (`unattended-upgrades`); check it is active with
`systemctl status unattended-upgrades`. Reboot when `/var/run/reboot-required` exists (the game server
restarts with the machine; clients reconnect).

## 6. Optional: restricted keys for tools

**A deploy key** that may only run one command, from one address, without a terminal (in that user's
`authorized_keys`, one line):

```text
from="203.0.113.10",command="/usr/local/bin/deploy-game",no-pty,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAA… deploy
```

**An SFTP-only account** for uploads (no shell; locked into its folder), at the end of a file in
`/etc/ssh/sshd_config.d/` (and the account added to `AllowUsers`):

```text
Match User uploads
    ForceCommand internal-sftp
    ChrootDirectory /srv/uploads
    AllowTcpForwarding no
    X11Forwarding no
```

The chroot folder (`/srv/uploads`) must be owned by root and not writable by others; give the account a
writable folder inside it (`/srv/uploads/incoming`, owned by `uploads`).

## 7. Connecting from a client crate

[`net_backend_client`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_client) (Rust) and
[`bevy_net_backend`](https://github.com/warmar94/bevy_net_backend) (Bevy) have `ssh` / `sftp` features for
admin and development tools; any other SSH client works the same way. They connect to this OpenSSH with a
key and check the host key strictly. Give them the machine's host key out of band (never by trusting the
first connection over an unknown network): on the machine,
`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` prints its fingerprint; compare it with what
`ssh-keyscan -t ed25519 api.example.com` returns on your side before you put that line into the tool's
`known_hosts` file.
