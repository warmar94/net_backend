# Deploying net_backend_server

Everything needed to run a server built with
[`net_backend_server`](https://github.com/warmar94/net_backend/tree/main/crates/net_backend_server) on one
Linux machine (written for Ubuntu 24.04): HTTPS and WSS through Caddy, migrations on every deploy, daily
backups with a tested restore, and a hardened SSH login. You choose the install path once:

| | Docker Compose | systemd |
|---|---|---|
| The server runs | in a container (non-root, read-only, health-checked) | as a service of a dedicated user, with systemd's sandboxing |
| Database | MySQL 8.4 or PostgreSQL 16 in a container | MySQL, PostgreSQL or SQLite on the machine |
| Caddy | in a container | Caddy's own package |
| Migrations | the `migrate` service runs before the server starts | `ExecStartPre=… migrate` before every start |
| Backups | `net-backend-backup` (systemd timer) dumps inside the database container | the same timer, dumping directly |
| Updates | `git pull` + `docker compose up -d --build` | build, then `install.sh` again (or copy the binary + restart) |

Both paths install the **reference server**: the framework with the `Auth`, `Storage` and `Chat`
modules, configured entirely from `config.toml`
([`examples/server.rs`](https://github.com/warmar94/net_backend/blob/main/crates/net_backend_server/examples/server.rs)).
Your own game server is a binary built the same way (your routes, hooks and WebSocket handlers added); it
has the same command line (`serve`, `migrate`, `config check`, `user:create`, …), so every file here works
for it unchanged: point the Dockerfile at your binary, or pass it to `install.sh --binary`.

## Contents

- [The files](#the-files)
- [Before you start](#before-you-start)
- [Path A: Docker Compose](#path-a-docker-compose)
- [Path B: systemd](#path-b-systemd)
- [Caddy](#caddy)
- [Configuration](#configuration)
- [Migrations on deploy](#migrations-on-deploy)
- [Backups and restore](#backups-and-restore)
- [Capacity, limits and load testing](#capacity-limits-and-load-testing)
- [Security checklist](#security-checklist)
- [Troubleshooting](#troubleshooting)

## The files

| Path | What |
|---|---|
| `config/config.toml` | the production configuration (both paths start from it) |
| `docker/Dockerfile` | multi-stage build of the reference server into a distroless, non-root image with a health check |
| `docker/compose.yaml` | the server, a one-shot `migrate` service and Caddy; private networks (the Caddy network with IPv4 + IPv6) |
| `docker/compose.mysql.yaml`, `docker/compose.postgres.yaml`, `docker/postgres-init.sql` | the database, chosen at install time (PostgreSQL: the app's non-superuser role) |
| `docker/Caddyfile` | HTTPS + WSS for `$DOMAIN` |
| `docker/setup.sh` | writes `.env`, the generated database secrets, `config.toml` and `migrations/` |
| `systemd/install.sh` | the systemd install (user, folders, database account, service, Caddy, backups) |
| `systemd/net-backend.service` | the hardened unit |
| `systemd/net-backend.caddy`, `systemd/Caddyfile`, `systemd/caddy-limits.conf` | Caddy's site (an imported file), the main Caddyfile for a machine without one, Caddy's open-file limit |
| `systemd/net-backend-cli` | runs a server command as the service user (`sudo net-backend-cli user:create …`) |
| `backup/backup.sh`, `backup/restore.sh` | backup and restore for MySQL, PostgreSQL and SQLite, both paths |
| `backup/install.sh`, `backup/net-backend-backup.{service,timer}`, `backup/backup.env.example` | the daily backup job |
| [`ssh-hardening.md`](ssh-hardening.md) | key-only SSH, no root login, firewall, fail2ban |
| [`../crates/load_test`](https://github.com/warmar94/net_backend/tree/main/crates/load_test) | the load generator used to size a machine |

## Before you start

- **A machine** with Ubuntu 24.04, a public address, and SSH access by key ([`ssh-hardening.md`](ssh-hardening.md)
  first). 2 vCPU and 4 GB of memory are a comfortable start; see [Capacity](#capacity-limits-and-load-testing).
- **A host name** for the API (for example `api.example.com`) whose DNS A / AAAA record points at the machine.
- **Open ports** 80 and 443 (TCP) and 443 (UDP, HTTP/3) towards the machine; Caddy needs port 80 for its
  certificate challenge and redirects it to HTTPS. Nothing else is opened: the server and the database
  listen on loopback (systemd) or on private container networks (Docker).
- **A checkout** of the repository on the machine: `sudo git clone https://github.com/warmar94/net_backend /opt/net_backend`.

## Path A: Docker Compose

1. **Docker Engine 27 or newer with the Compose plugin**, from Docker's own apt repository
   ([docs.docker.com/engine/install/ubuntu](https://docs.docker.com/engine/install/ubuntu/)). Check:
   `docker version` (Engine 27+) and `docker compose version`. From version 27 Docker forwards IPv6 clients
   to Caddy with their own address (`ip6tables` is on by default); with an older Engine set
   `"ip6tables": true` in `/etc/docker/daemon.json`, or every IPv6 player shares one address.
2. **Prepare** (in `/opt/net_backend/deploy/docker`):

   ```text
   sudo bash setup.sh --db mysql --domain api.example.com --email admin@example.com     # or --db postgres
   ```

   It writes `.env` (the database overlay, your host name and email, the Caddy network's addresses),
   `secrets/` (a random password for the app's database account `nbs`, one for the database administrator,
   and `database_url`, the server's connection URL), `config.toml` (a copy of `../config/config.toml`) and an
   empty `migrations/`. Existing secrets are never replaced (the database volume was created with them).
   The app's account is never a database administrator: MySQL's `nbs` owns only the database `nbs`; on
   PostgreSQL `postgres-init.sql` creates `nbs` as a plain login role owning the database `nbs`.
3. **Review `config.toml`**: `[modules.auth] app_name`, mail (SMTP) and the chat rooms. The address, the
   database URL file and the trusted proxy are set by `compose.yaml` and need no change.
4. **Start**:

   ```text
   sudo docker compose up -d --build        # builds the image, starts the database, runs `migrate`, starts the server and Caddy
   sudo docker compose ps -a                # server: healthy; migrate: exited (0)
   curl https://api.example.com/v1/info     # {"protocol":1,"min_protocol":1,"modules":["auth","chat","storage"]}
   ```

5. **The first administrator**:

   ```text
   sudo docker compose exec server net-backend-server user:create admin@example.com --admin
   ```

   The password is generated and printed once. Every server command works this way
   (`… exec server net-backend-server migrate status`, `config check --connect`, `user:role`, …).
6. **Backups**: `sudo bash ../backup/install.sh --mode docker --compose-dir /opt/net_backend/deploy/docker`
   (see [Backups](#backups-and-restore)).

**Updates:** `git pull`, then `sudo docker compose up -d --build`: the new image is built, `migrate` runs
again (pending migrations only), then the server is replaced. During the switch open WebSockets get close
1001 and clients reconnect. **Logs:** `sudo docker compose logs -f server` (JSON lines), `… logs caddy`.

What the Compose setup does for you: the server is not published on the host (only Caddy's ports are); the
database sits on an internal network without outside access; the server and `migrate` containers are
read-only, run as uid 65532, drop every capability and cannot gain privileges; Caddy (pinned to `caddy:2.11`)
keeps only the capability to bind ports 80 / 443; secrets are files mounted at `/run/secrets/`;
`stop_grace_period` (90 s) covers the server's shutdown grace (20 s), up to 10 s per module shutdown and the
pool close; the open-file limits are raised for the server (262 144) and Caddy (1 048 576); logs rotate at
5 × 50 MB per container; the Caddy network has IPv4 and IPv6, and Caddy has fixed addresses in it (`.env`),
which are the only proxies the server trusts.

## Path B: systemd

1. **The database** (pick one):

   ```text
   sudo apt install mysql-server           # MySQL 8.0 (Ubuntu 24.04); or mariadb-server
   sudo apt install postgresql             # PostgreSQL 16
   sudo apt install sqlite3                # SQLite: only the command-line tool, for backups
   ```

2. **Caddy 2.8 or newer** from Caddy's apt repository ([caddyserver.com/docs/install](https://caddyserver.com/docs/install#debian-ubuntu-raspbian));
   Ubuntu's own `caddy` package is older and does not know every directive the Caddyfile uses. Check: `caddy version`.
3. **Build the server** as a normal user (Rust from [rustup.rs](https://rustup.rs)):

   ```text
   cd /opt/net_backend
   cargo build --release --locked -p net_backend_server --example server \
       --no-default-features --features mysql,storage,chat,smtp,steam      # postgres / sqlite instead of mysql
   ```

   The binary is `target/release/examples/server`. Building on another machine works too (same CPU
   architecture, glibc 2.39 or older, e.g. Ubuntu 24.04 or Debian 12).
4. **Install**:

   ```text
   sudo bash deploy/systemd/install.sh --db mysql --binary target/release/examples/server \
       --domain api.example.com --email admin@example.com        # --email is optional
   ```

   It creates the system user `nbs`, `/etc/net-backend/` (`config.toml`, `database_url`, `migrations/`),
   `/var/lib/net-backend/`, the database `nbs` and its account `nbs` with a random password (MySQL /
   PostgreSQL through the local administrator login; SQLite: `/var/lib/net-backend/nbs.db`), the binary
   `/usr/local/bin/net-backend-server`, the helper `/usr/local/bin/net-backend-cli`, the service
   `net-backend` (checked with `config check --connect`, then started: migrations first), Caddy's site and
   the daily backup. Caddy's site is its own file, `/etc/caddy/sites/net-backend.caddy`: the main
   `/etc/caddy/Caddyfile` gets one line, `import /etc/caddy/sites/*.caddy`, and keeps everything else (your
   own sites and options). Only Caddy's package default (or a missing file) is replaced by
   `systemd/Caddyfile` (global timeouts + the import). Every file changed is kept first as
   `….before-<time>`. Running it again keeps the configuration, the secrets and the database, installs the
   new binary, rewrites only the site file and restarts the server. Caddy is reloaded only when its site or
   main file changed (open WebSockets of every site stay) and restarted only when its open-file limit is
   new. A Caddy older than 2.8 is refused before anything is installed. `--email` (certificate notices) is
   optional.
5. **Review `/etc/net-backend/config.toml`** (app name, mail, chat rooms), then `sudo systemctl restart net-backend`.
6. **The first administrator**: `sudo net-backend-cli user:create admin@example.com --admin`.

**Updates:** build the new binary, then `sudo bash deploy/systemd/install.sh --db mysql --binary …` again, or
`sudo install -m 0755 target/release/examples/server /usr/local/bin/net-backend-server && sudo systemctl restart net-backend`.
**Logs:** `journalctl -u net-backend -f`. **State:** `systemctl status net-backend`, `sudo net-backend-cli migrate status`.

The unit (`systemd/net-backend.service`): user `nbs`, `NoNewPrivileges`, `ProtectSystem=strict` (only
`/var/lib/net-backend` is writable), `ProtectHome`, `PrivateTmp`, `PrivateDevices`, no capabilities, a
system-call filter, `LimitNOFILE=262144`, `Restart=on-failure`, `TimeoutStopSec=90s` (the 20 s shutdown
grace, up to 10 s per module shutdown and the pool close, with room), `ExecStartPre=… migrate`.
`/etc/net-backend` is `root:nbs 0750`; the database URL file and the configuration are `root:nbs 0640`.

## Caddy

Both Caddyfiles do the same:

- **HTTPS and WSS** with automatic certificates; HTTP redirects to HTTPS; HTTP/3 on UDP 443.
- **One `reverse_proxy`** for the API and the WebSocket hub (`/v1/ws`): Caddy passes WebSocket upgrades
  through by itself. `stream_close_delay 5m` keeps open WebSockets through a configuration reload
  instead of closing all of them at once.
- **`request_body { max_size 5MB }` for the whole site.** The storage module's batch put carries up to
  ~4.2 MB (4 MiB of values plus JSON). In Caddy a site-wide limit applies before a route's own, so it must
  allow the largest route (64 KB here would answer the batch with 413 from Caddy).
- **Token-safe access logs:** the `Authorization` and `Cookie` headers are deleted from log lines, and a
  `token` query parameter is removed from the logged URL. The server keeps `ws.query_token = false`, so
  clients send the token in the `Authorization` header (or as the first-message `auth`), never in the URL.
- **Trusted proxy:** Caddy adds `X-Forwarded-For` with the client's address. The server believes it only
  from the addresses in `http.trusted_proxies`: `127.0.0.1` / `::1` for the systemd install, Caddy's two
  fixed addresses in the Compose `edge` network for Docker (`CADDY_IPV4`, `CADDY_IPV6` in `.env`; not the
  network's gateway). Per-address rate limits and WebSocket caps then count real clients,
  not Caddy. If another proxy or a CDN sits in front of Caddy, configure Caddy's own `trusted_proxies` for
  it as well; otherwise every client looks like that proxy.
- **Not proxied:** Prometheus metrics (`metrics.enabled`) listen on `127.0.0.1:9100`, never through Caddy.

**Memory:** Caddy needs about **120 KiB per proxied WebSocket** (measured: 10 000 sockets over HTTPS raised its
memory by ~1.2 GB), eight times what the server needs per socket. On a small machine Caddy, not the server,
bounds the number of sockets: plan ~1.2 GB of Caddy memory per 10 000 connected players. Caddy holds two file
descriptors per socket; both installs raise its limit to 1 048 576.

## Configuration

`config/config.toml` is the reference server's production configuration; every key is described in the
server README's "Configuration" section. What deployments change most:

| Key | Why |
|---|---|
| `[modules.auth] app_name`, `mailer = "smtp"`, `smtp_*`, `mail_from`, `verify_url`, `reset_url` | real mail for verification and password resets (the default log mailer only logs the recipient and subject) |
| `[modules.auth] steam_*` | Steam login |
| `[[modules.chat.rooms]]` | the public chat rooms (key, name, member cap) |
| `[ws] max_connections` | the socket cap (keep it below the open-file limit; see Capacity) |
| `[metrics] enabled` | Prometheus metrics on loopback |
| `[openapi] enabled` | `/v1/openapi.json` + `/v1/asyncapi.json`; set false to hide the API description |

Secrets are files: `database.url_file`, `modules.auth.smtp_password_file`, `modules.auth.steam_web_api_key_file`
(systemd: under `/etc/net-backend/`, `root:nbs 0640`; Docker: add them to `secrets:` in `compose.yaml`).
Every key can also be set as `NBS__SECTION__KEY` (module keys: `NBS__MODULES__CHAT__HISTORY_RETENTION_DAYS`).
Check a change before restarting: `sudo net-backend-cli config check --connect` (Docker:
`sudo docker compose run --rm --no-deps server config check --connect`; `config check` also reads every
module's own section, so a typo under `[modules.chat]` fails here, not at the next start).

## Migrations on deploy

- **systemd:** `ExecStartPre=/usr/local/bin/net-backend-server migrate` runs before every start; a failed
  migration stops the start (the old schema untouched except as the migration error describes). systemd
  retries every 5 s (`activating (auto-restart)` in `systemctl status`) and marks the service `failed`
  after 5 failed starts within 10 minutes; fix the cause, then `systemctl reset-failed net-backend` and
  start it.
- **Docker Compose:** the `migrate` service runs `net-backend-server migrate` and exits; the server starts
  only after it succeeded (`service_completed_successfully`). `docker compose up -d` runs it on every deploy.
- **Alternative:** `database.migrate_on_start = true` makes `serve` migrate first. Several instances
  starting together are safe (MySQL `GET_LOCK`, a PostgreSQL advisory lock, SQLite's write lock; each
  migration re-checks its tracking row).
- `migrate status` lists every migration (`applied`, `pending`, `MODIFIED`, `missing`).
  **MySQL commits DDL statement by statement:** a failing MySQL migration names exactly which of its
  statements already took effect and how to recover. There is no "down" migration: to go back, restore the
  backup taken before the deploy.
- **Owning a module's tables:** `sudo net-backend-cli migrations publish chat` copies the module's SQL into
  `/etc/net-backend/migrations/chat/<dialect>/`; from then on those files are used. Edit them only before
  their first `migrate`. For Docker, publish with the same binary on your development machine
  (`server migrations publish chat`, they belong in your project anyway) and copy the folder into
  `deploy/docker/migrations/` (mounted read-only at `/etc/net-backend/migrations`).

**A deploy, step by step:** back up (`sudo systemctl start net-backend-backup`), deploy (the migrations
run), check (`/readyz`, `migrate status`, the logs).

## Backups and restore

`backup/install.sh` installs `net-backend-backup`, `net-backend-restore`, the settings file
`/etc/net-backend/backup.env` and a timer that runs a backup every day at about 03:30. There is one settings
file per machine, for the install path the machine runs (systemd or Docker); a second install path on the
same machine keeps the first one's settings.

| Database | How | File |
|---|---|---|
| MySQL / MariaDB | `mysqldump --single-transaction` (consistent, writers keep going), checked for its final "Dump completed" line | `net-backend-mysql-<UTC time>.sql.gz` |
| PostgreSQL | `pg_dump -Fc` (compressed), checked with `pg_restore --list` | `net-backend-postgres-<UTC time>.dump` |
| SQLite | the online backup API (`sqlite3 .backup`, consistent while the server writes), `PRAGMA integrity_check`, gzip | `net-backend-sqlite-<UTC time>.db.gz` |

Backups go to `/var/backups/net-backend/` (mode 0700) and are kept for 14 days (`NBS_BACKUP_KEEP_DAYS`).
The database password never appears on a command line. Run one at once:
`sudo systemctl start net-backend-backup && journalctl -u net-backend-backup -n 20`. A failed nightly run
leaves the unit `failed`: check `systemctl status net-backend-backup` (or watch it with your monitoring),
or add `OnFailure=` to the unit with a service that alerts you.

**Off the machine:** a backup on the same disk does not survive the loss of the machine. Copy
`/var/backups/net-backend/` elsewhere every day, for example with `restic` (encrypted, deduplicated) or
`rclone` to object storage, from a second timer or the provider's snapshot feature.

**Restore** (the drill: practise it once on a fresh machine before you need it):

```text
sudo net-backend-restore /var/backups/net-backend/net-backend-mysql-20261002T033012Z.sql.gz --yes
```

It first checks the backup completely, before anything is stopped or dropped (MySQL: a valid gzip ending
with mysqldump's "Dump completed" line; PostgreSQL: the whole dump read through `pg_restore`; SQLite:
unpacked and `PRAGMA integrity_check`), from a private copy of the file. Then it takes a safety backup of
the current state under its own name (`net-backend-<db>-before-restore-<time>…`; backups never overwrite each
other) and prints its path, stops the server (Docker: the server and Caddy), replaces the database with the
backup (MySQL: the database is dropped and created again; PostgreSQL: the `public` schema dropped, created
and loaded in ONE transaction, so a failed load leaves the old data; SQLite: the old files are moved aside
as `….before-restore-<time>`), runs `migrate` (migrations newer than the backup),
starts everything and waits until `/readyz` answers. If a step after the stop fails, it prints the
command that puts the safety backup back (`net-backend-restore <safety file> --yes --no-safety-backup`).

A restore drill, step by step:

1. Create data: an account (`user:create`), a storage object, a chat message.
2. `sudo systemctl start net-backend-backup`; note the file name.
3. Change data: another account, delete the object, another message.
4. `sudo net-backend-restore <that file> --yes`.
5. Check: the second account and the new message are gone, the object is back, logins with the first
   account work, `migrate status` shows every migration `applied`, the clients reconnect.

## Capacity, limits and load testing

Measured on a 2 vCPU / 7.8 GiB virtual machine (Ubuntu 24.04, MySQL 8 or PostgreSQL 16 on the same machine,
Caddy in front). The HTTPS rows ran `load_test` on that same machine, so it shared the 2 vCPU with the
server, Caddy and the database:

| What | Measured |
|---|---|
| 10 000 authenticated WebSockets over HTTPS through Caddy | all connected in 56 s (TLS handshakes), none closed during the hold; server ~15 KiB per socket, Caddy ~121 KiB |
| A redeploy: 5000 sockets closed with 1001 by a restart | all 5000 reconnected (p99 17 s, bound by TLS handshakes on the shared CPUs) |
| Chat over HTTPS: 200 members, 50 senders at 1 message/s, every message stored | 600 000 of 600 000 deliveries, p99 294 ms (MySQL), 241 ms (PostgreSQL) |
| Room fan-out on loopback (one sender, 64-byte pushes) | 105 000 deliveries/s into 200 members; 149 000/s into 1000 members |
| 1000 players saving for the first time at the same moment | all 1000 stored in 8.6 s (MySQL), 7.4 s (PostgreSQL) |
| 4 MiB batch saves through Caddy | 10 of 10, median 0.97 s (MySQL), 0.47 s (PostgreSQL) |
| HTTP through Caddy, 64 connections | `/v1/info` ~4 500 requests/s; `GET /v1/account` 770 (MySQL) / 890 (PostgreSQL); a storage read 940 / 1 050 |

The limits to set together:

- **Open files:** one per socket in the server (`LimitNOFILE=262144` / Compose `ulimits`), two in Caddy
  (1 048 576). `ws.max_connections` (default 10 000) stays below the server's limit; above it new sockets
  get 503 with `Retry-After`.
- **Memory:** ~15 KiB per socket in the server plus ~120 KiB in Caddy, so roughly 1.4 GB per 10 000
  connected players, on top of the database's buffer pool and the system (the default
  `ws.max_connections = 10000` wants a 4 GB machine). Raise `ws.max_connections` only
  after a load test on the machine itself.
- **Per player:** at most 5 sockets per account (`ws.max_connections_per_user`), 100 per client address
  (`ws.max_connections_per_ip`; IPv6 counted by /64), 60 WebSocket handshakes per address per minute.

**Per-instance state:** one server process holds the WebSocket rooms, chat presence, and every rate limit
and login counter in memory. Run one instance per database; several instances need a shared pub/sub
`Broadcaster` and accept that rooms, presence and rates stay per instance.

**Load testing your machine** with [`load_test`](https://github.com/warmar94/net_backend/tree/main/crates/load_test)
(from a second machine, or from the same one for a first look): it creates accounts, opens sockets, runs
chat fan-out, DM bursts, simultaneous first saves, 4 MiB batch puts and HTTP request rates through
Caddy, and prints latency percentiles. One load machine is one client address, so a load-test server
turns the per-address limits off (never on a public server):

```text
NBS__MODULES__AUTH__RATE_LIMITS=false            # registration / login per address
NBS__WS__HANDSHAKES_PER_IP_PER_MINUTE=0
NBS__WS__MAX_CONNECTIONS_PER_IP=100000
NBS__MODULES__CHAT__RATE_MESSAGES=1000           # chat sends per player
NBS__MODULES__CHAT__RATE_WINDOW_SECS=1
NBS__MODULES__STORAGE__WRITE_RATE=1000
```

## Security checklist

- SSH by key only, no root login, a firewall that opens 22 / 80 / 443 only, fail2ban: [`ssh-hardening.md`](ssh-hardening.md).
- Docker publishes ports past `ufw` (it writes its own iptables rules): publish nothing but Caddy's 80 / 443,
  as `compose.yaml` does.
- Secrets: `secrets/` (Docker, folder 0700) and `/etc/net-backend/` (`root:nbs 0640`); never in
  `config.toml`, never in the repository (`.gitignore` covers `deploy/docker/secrets/` and `.env`).
- `ws.query_token = false` (the default), tokens only in headers.
- Metrics on loopback only; `openapi.enabled = false` if the API description should not be public.
- The admin routes (`/v1/admin/...`) need the `admin` role; give it to as few accounts as possible
  (`user:role`), every admin action is in the audit log.
- Backups leave the machine (encrypted), and a restore was practised.
- Ubuntu's unattended security upgrades stay on (`systemctl status unattended-upgrades`).

## Troubleshooting

| Symptom | Look at |
|---|---|
| `install.sh` stops at `config check` | the message names the key; `database reachable` needs the database running and the URL in `/etc/net-backend/database_url` |
| the service is `activating (auto-restart)` or `failed` right after a deploy | `journalctl -u net-backend -n 50`: a failing migration prints which statements ran and how to recover |
| Docker: `server` never becomes healthy | `docker compose logs migrate server`; `docker compose exec server net-backend-server healthcheck` |
| HTTPS does not come up | DNS points at the machine, ports 80 / 443 reachable; `journalctl -u caddy` (Docker: `docker compose logs caddy`) |
| 413 from Caddy on a batch save | the site's `request_body max_size` is below 5 MB |
| every player shares one rate limit | `http.trusted_proxies` does not list the proxy in front of the server |
| sockets refused with 503 above some thousands | `ws.max_connections`, the open-file limits of the server and Caddy, Caddy's memory |
| `caddy validate` reports an unknown directive or field | Caddy older than 2.8 (`caddy version`); install it from Caddy's repository |
