#!/usr/bin/env bash
# Installs net_backend_server as a systemd service on Ubuntu 24.04 (run as root, from a checkout of the
# repository). Safe to run again: existing configuration, secrets and databases are kept.
#
#   sudo bash deploy/systemd/install.sh --db mysql|postgres|sqlite --binary target/release/examples/server \
#        [--domain api.example.com [--email admin@example.com]] [--no-backup]
#
# What it does:
#   - the system user `nbs`; /etc/net-backend (config.toml, database_url, migrations/); /var/lib/net-backend
#   - /usr/local/bin/net-backend-server (the binary) and /usr/local/bin/net-backend-cli (runs commands as `nbs`)
#   - the database and its account when /etc/net-backend/database_url does not exist yet (MySQL / PostgreSQL
#     on this machine, through the local admin account; SQLite: a file in /var/lib/net-backend)
#   - the service net-backend (migrations before every start), checked with `config check --connect`
#   - with --domain: the site /etc/caddy/sites/net-backend.caddy (HTTPS + WSS), imported once from the main
#     /etc/caddy/Caddyfile (your own sites and options there are kept), and Caddy's open-file limit
#   - the daily backup timer (deploy/backup/install.sh), unless --no-backup
set -euo pipefail

usage() {
	echo "usage: $0 --db mysql|postgres|sqlite --binary <path> [--domain <host name> [--email <address>]] [--no-backup]" >&2
	exit 64
}

db="" binary="" domain="" email="" backup=1
while [ "$#" -gt 0 ]; do
	case "$1" in
	--db | --binary | --domain | --email)
		[ "$#" -ge 2 ] || usage
		case "$1" in --db) db="$2" ;; --binary) binary="$2" ;; --domain) domain="$2" ;; --email) email="$2" ;; esac
		shift 2
		;;
	--no-backup) backup=0; shift ;;
	*) usage ;;
	esac
done
case "$db" in mysql | postgres | sqlite) ;; *) usage ;; esac
[ -n "$binary" ] || usage
[ -x "$binary" ] || { echo "$binary is not an executable file (build it first, see deploy/README.md)" >&2; exit 66; }
[ -z "$email" ] || [ -n "$domain" ] || usage
if [ -n "$domain" ]; then
	case "$domain" in *[!A-Za-z0-9.-]*) echo "invalid host name: $domain" >&2; exit 64 ;; esac
	if [ -n "$email" ]; then
		case "$email" in *[!A-Za-z0-9.@+_-]* | *@*@* | @* | *@) echo "invalid email: $email" >&2; exit 64 ;; *@*) ;; *) echo "invalid email: $email" >&2; exit 64 ;; esac
	fi
fi
[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 77; }
if [ -n "$domain" ]; then
	# Before anything is installed: Caddy 2.8 or newer (the site uses `stream_close_delay` and the
	# log filter's field syntax).
	command -v caddy >/dev/null || { echo "Caddy is not installed (see deploy/README.md, 'Path B')" >&2; exit 69; }
	caddy_version="$(caddy version | grep -oE '[0-9]+\.[0-9]+' | head -n 1 || true)"
	caddy_major="${caddy_version%%.*}" caddy_minor="${caddy_version#*.}"
	[ -n "$caddy_version" ] || { caddy_major=0; caddy_minor=0; }
	if [ "$caddy_major" -lt 2 ] || { [ "$caddy_major" -eq 2 ] && [ "$caddy_minor" -lt 8 ]; }; then
		echo "Caddy $(caddy version | head -n 1) is too old: 2.8 or newer is needed (install it from Caddy's repository, deploy/README.md 'Path B'); nothing was installed" >&2
		exit 69
	fi
fi
here="$(cd "$(dirname "$0")" && pwd)"
step() { echo "==> $*"; }

random_hex() { head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

step "user and folders"
if ! id nbs >/dev/null 2>&1; then
	useradd --system --home-dir /var/lib/net-backend --no-create-home --shell /usr/sbin/nologin nbs
fi
install -d -o root -g nbs -m 0750 /etc/net-backend
install -d -o nbs -g nbs -m 0750 /etc/net-backend/migrations
install -d -o nbs -g nbs -m 0750 /var/lib/net-backend

step "binary"
install -m 0755 "$binary" /usr/local/bin/net-backend-server
install -m 0755 "$here/net-backend-cli" /usr/local/bin/net-backend-cli

step "configuration"
if [ -e /etc/net-backend/config.toml ]; then
	echo "kept     /etc/net-backend/config.toml"
else
	install -o root -g nbs -m 0640 "$here/../config/config.toml" /etc/net-backend/config.toml
	echo "wrote    /etc/net-backend/config.toml (edit [modules.auth] app_name, mail and the chat rooms)"
fi

step "database"
url_file=/etc/net-backend/database_url
if [ -s "$url_file" ]; then
	echo "kept     $url_file (the database is not touched)"
else
	password="$(random_hex)"
	case "$db" in
	mysql)
		command -v mysql >/dev/null || { echo "MySQL is not installed (apt install mysql-server)" >&2; exit 69; }
		# Ubuntu's MySQL root account logs in through the local socket (auth_socket).
		mysql --protocol=socket -u root <<SQL
CREATE DATABASE IF NOT EXISTS nbs CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER IF NOT EXISTS 'nbs'@'localhost' IDENTIFIED BY '${password}';
CREATE USER IF NOT EXISTS 'nbs'@'127.0.0.1' IDENTIFIED BY '${password}';
ALTER USER 'nbs'@'localhost' IDENTIFIED BY '${password}';
ALTER USER 'nbs'@'127.0.0.1' IDENTIFIED BY '${password}';
GRANT ALL PRIVILEGES ON nbs.* TO 'nbs'@'localhost';
GRANT ALL PRIVILEGES ON nbs.* TO 'nbs'@'127.0.0.1';
SQL
		url="mysql://nbs:${password}@127.0.0.1:3306/nbs"
		;;
	postgres)
		command -v psql >/dev/null || { echo "PostgreSQL is not installed (apt install postgresql)" >&2; exit 69; }
		(cd / && sudo -u postgres psql -v ON_ERROR_STOP=1 -q) <<SQL
DO \$\$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'nbs') THEN CREATE ROLE nbs LOGIN; END IF;
END \$\$;
ALTER ROLE nbs WITH LOGIN PASSWORD '${password}';
SQL
		if ! (cd / && sudo -u postgres psql -tAc "SELECT 1 FROM pg_database WHERE datname = 'nbs'") | grep -q 1; then
			(cd / && sudo -u postgres createdb --owner=nbs --encoding=UTF8 nbs)
		fi
		url="postgres://nbs:${password}@127.0.0.1:5432/nbs"
		;;
	sqlite)
		url="sqlite:/var/lib/net-backend/nbs.db"
		;;
	esac
	umask 027
	printf '%s' "$url" >"$url_file"
	umask 022
	chown root:nbs "$url_file"
	chmod 0640 "$url_file"
	echo "wrote    $url_file"
fi

step "check the configuration and the database"
/usr/local/bin/net-backend-cli config check --connect

step "service"
install -m 0644 "$here/net-backend.service" /etc/systemd/system/net-backend.service
systemctl daemon-reload
systemctl enable net-backend
# A restart picks up a new binary; ExecStartPre applies pending migrations first.
systemctl restart net-backend
ready=0
for _ in $(seq 1 30); do
	if /usr/local/bin/net-backend-cli healthcheck 2>/dev/null; then ready=1; break; fi
	sleep 2
done
[ "$ready" -eq 1 ] || { echo "the server did not become ready; see: journalctl -u net-backend -n 50" >&2; exit 1; }
echo "ready    net-backend (127.0.0.1:8080)"

if [ -n "$domain" ]; then
	step "Caddy (HTTPS + WSS for $domain)"
	stamp="$(date -u +%Y%m%dT%H%M%SZ)"
	install -d -m 0755 /var/log/caddy /etc/caddy/sites
	# The site file is ours: rewritten on every run (a changed one is kept with a time stamp first).
	site=/etc/caddy/sites/net-backend.caddy
	# Without --email the `tls <email>` line goes (certificates work without a contact address).
	if [ -n "$email" ]; then
		sed -e "s|API_DOMAIN|${domain}|" -e "s|ACME_EMAIL|${email}|" "$here/net-backend.caddy" >"$site.new"
	else
		sed -e "s|API_DOMAIN|${domain}|" -e '/^[[:space:]]*tls ACME_EMAIL$/d' -e '/The address for certificate expiry notices/d' "$here/net-backend.caddy" >"$site.new"
	fi
	caddy_changed=0
	if [ ! -e "$site" ] || ! cmp -s "$site" "$site.new"; then
		caddy_changed=1
	fi
	if [ -e "$site" ] && ! cmp -s "$site" "$site.new"; then
		cp -p "$site" "$site.before-$stamp"
		echo "kept     $site.before-$stamp"
	fi
	# The main Caddyfile is the operator's: only Caddy's package default (or a missing file) is
	# replaced by ours; otherwise the import line is added once and everything else stays.
	main=/etc/caddy/Caddyfile
	main_new=""
	if [ ! -e "$main" ] || { grep -q 'root \* /usr/share/caddy' "$main" && ! grep -q '^import /etc/caddy/sites/' "$main"; }; then
		if [ -e "$main" ] && ! cmp -s "$main" "$here/Caddyfile"; then
			cp -p "$main" "$main.before-$stamp"
			echo "kept     $main.before-$stamp"
		fi
		main_new="$here/Caddyfile"
	elif ! grep -q '^import /etc/caddy/sites/\*\.caddy' "$main"; then
		cp -p "$main" "$main.before-$stamp"
		echo "kept     $main.before-$stamp"
		{
			cat "$main"
			echo
			echo "# net_backend_server (deploy/systemd/install.sh)"
			echo "import /etc/caddy/sites/*.caddy"
		} >"$main.new"
		main_new="$main.new"
		echo "note     your Caddyfile keeps its own global options; add read_header / idle timeouts there if wanted (deploy/systemd/Caddyfile)"
	fi
	mv "$site.new" "$site"
	if [ -n "$main_new" ]; then
		[ "$main_new" = "$main.new" ] || cp "$main_new" "$main.new"
		if ! caddy validate --adapter caddyfile --config "$main.new"; then
			echo "the new Caddy configuration is invalid; nothing was changed in $main ($site is written)" >&2
			exit 1
		fi
		mv "$main.new" "$main"
		caddy_changed=1
	else
		caddy validate --adapter caddyfile --config "$main"
	fi
	# The check above may have created the log file as root; Caddy runs as `caddy`.
	chown -R caddy:caddy /var/log/caddy
	# The open-file limit needs a restart (closes every proxied WebSocket of every site), so only when
	# the drop-in is new or changed; a changed site or Caddyfile only needs a reload (open WebSockets
	# stay, `stream_close_delay`); nothing changed: Caddy is left alone.
	dropin=/etc/systemd/system/caddy.service.d/net-backend-limits.conf
	install -d -m 0755 /etc/systemd/system/caddy.service.d
	if [ ! -e "$dropin" ] || ! cmp -s "$dropin" "$here/caddy-limits.conf"; then
		install -m 0644 "$here/caddy-limits.conf" "$dropin"
		systemctl daemon-reload
		systemctl restart caddy
		echo "restart  caddy (new open-file limit)"
	elif [ "$caddy_changed" -eq 1 ]; then
		systemctl reload caddy
		echo "reload   caddy"
	else
		echo "kept     caddy (nothing changed)"
	fi
	echo "ready    https://${domain}/v1/info (the certificate may take a minute)"
fi

if [ "$backup" -eq 1 ]; then
	step "daily backup"
	bash "$here/../backup/install.sh" --mode systemd
fi

echo
echo "Done. Useful commands:"
echo "  systemctl status net-backend            journalctl -u net-backend -f"
echo "  sudo net-backend-cli migrate status      sudo net-backend-cli user:create admin@example.com --admin"
