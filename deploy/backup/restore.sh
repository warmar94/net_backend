#!/usr/bin/env bash
# net-backend-restore: puts a backup made by net-backend-backup back. Runs as root.
#
#   net-backend-restore /var/backups/net-backend/net-backend-mysql-20261002T033000Z.sql.gz --yes
#
# Steps: check the backup COMPLETELY (nothing is stopped or dropped before that), take a safety backup of
# the current state (unless --no-safety-backup), stop the server, replace the database with the backup's
# content (MySQL: drop + create the database; PostgreSQL: drop + create the `public` schema, the load in
# one transaction; SQLite: the old files are moved aside), run `migrate` (applies migrations newer than the
# backup), start the server, wait until it is ready. If anything fails after the stop, the message names
# the safety backup and the command that puts it back.
# Same settings as backup.sh (NBS_BACKUP_MODE, NBS_DATABASE_URL_FILE, NBS_COMPOSE_DIR, NBS_BACKUP_DIR).
set -euo pipefail

# Settings from the environment win; the rest come from the settings file, if any.
env_file="${NBS_BACKUP_ENV:-/etc/net-backend/backup.env}"
if [ -r "$env_file" ]; then
	while IFS='=' read -r key value; do
		case "$key" in NBS_[A-Z_]*) [ -n "${!key:-}" ] || export "$key=$value" ;; esac
	done <"$env_file"
fi

mode="${NBS_BACKUP_MODE:-systemd}"
url_file="${NBS_DATABASE_URL_FILE:-/etc/net-backend/database_url}"
compose_dir="${NBS_COMPOSE_DIR:-/opt/net_backend/deploy/docker}"
here="$(cd "$(dirname "$0")" && pwd)"

log() { echo "net-backend-restore: $*"; }
fail() { echo "net-backend-restore: ERROR: $*" >&2; exit 1; }

file="" yes=0 safety=1
while [ "$#" -gt 0 ]; do
	case "$1" in
	--yes) yes=1; shift ;;
	--no-safety-backup) safety=0; shift ;;
	-*) fail "unknown option $1" ;;
	*) file="$1"; shift ;;
	esac
done
[ -n "$file" ] || fail "usage: $0 <backup file> --yes [--no-safety-backup]"
[ -s "$file" ] || fail "$file does not exist or is empty"
[ "$yes" -eq 1 ] || fail "this REPLACES the live database with $file; run again with --yes"
file="$(cd "$(dirname "$file")" && pwd)/$(basename "$file")"

if [ "$mode" = docker ]; then
	url="$(cat "$compose_dir/secrets/database_url")"
else
	url="$(cat "$url_file")"
fi
scheme="${url%%:*}"

umask 077
work="$(mktemp -d)"
cleanup() { rm -rf "$work"; }
trap cleanup EXIT

dc() { (cd "$compose_dir" && docker compose "$@"); }

# ---- 1. check the backup completely, before anything is touched ---------------------------------
log "checking $file"
case "$file" in
*.sql.gz)
	case "$scheme" in mysql | mariadb) ;; *) fail "a MySQL backup, but the server uses $scheme" ;; esac
	gzip -t "$file" || fail "not a valid gzip file"
	# mysqldump writes this line last: a dump without it was cut off.
	tail_lines="$(zcat "$file" | tail -n 3)"
	case "$tail_lines" in *"Dump completed"*) ;; *) fail "the dump is incomplete (no 'Dump completed' line); nothing was changed" ;; esac
	;;
*.dump)
	case "$scheme" in postgres | postgresql) ;; *) fail "a PostgreSQL backup, but the server uses $scheme" ;; esac
	if [ "$mode" = docker ]; then
		dc exec -T db pg_restore --list <"$file" >/dev/null || fail "pg_restore cannot read the dump; nothing was changed"
	else
		pg_restore --list "$file" >/dev/null || fail "pg_restore cannot read the dump; nothing was changed"
	fi
	;;
*.db.gz)
	[ "$scheme" = sqlite ] || fail "a SQLite backup, but the server uses $scheme"
	command -v sqlite3 >/dev/null || fail "sqlite3 is not installed (apt install sqlite3)"
	gzip -t "$file" || fail "not a valid gzip file"
	zcat "$file" >"$work/restore.db"
	[ "$(sqlite3 "$work/restore.db" 'PRAGMA integrity_check;')" = ok ] || fail "the backup fails PRAGMA integrity_check; nothing was changed"
	;;
*) fail "unknown backup type: $file" ;;
esac

# ---- 2. safety backup of the current state ------------------------------------------------------
safety_file=""
if [ "$safety" -eq 1 ]; then
	if [ -f "$here/backup.sh" ]; then
		backup_cmd=(bash "$here/backup.sh")
	elif command -v net-backend-backup >/dev/null; then
		backup_cmd=(net-backend-backup)
	else
		fail "backup.sh / net-backend-backup not found for the safety backup"
	fi
	log "safety backup of the current state first"
	NBS_BACKUP_KEEP_DAYS=0 "${backup_cmd[@]}" | tee "$work/safety.log"
	safety_file="$(sed -n 's/^net-backend-backup: wrote \(.*\) ([^)]*)$/\1/p' "$work/safety.log" | tail -n 1)"
	[ -n "$safety_file" ] && [ -s "$safety_file" ] || fail "the safety backup did not report its file; nothing was changed"
	log "safety backup: $safety_file"
fi

stop_server() {
	if [ "$mode" = docker ]; then
		dc stop server caddy
	else
		systemctl stop net-backend
	fi
}
start_server() {
	if [ "$mode" = docker ]; then
		dc run --rm migrate && dc up -d
	else
		# ExecStartPre runs `migrate`.
		systemctl start net-backend
	fi
}

# From here on a failure leaves the database partly replaced: say how to get back.
on_failure() {
	status=$?
	cleanup
	if [ "$status" -ne 0 ]; then
		echo "net-backend-restore: FAILED after the server was stopped; the database may be partly replaced." >&2
		if [ -n "$safety_file" ]; then
			echo "net-backend-restore: put the previous state back with:" >&2
			echo "  net-backend-restore $safety_file --yes --no-safety-backup" >&2
		else
			echo "net-backend-restore: no safety backup was taken (--no-safety-backup)." >&2
		fi
	fi
}
trap on_failure EXIT

# ---- 3. replace -----------------------------------------------------------------------------------
log "stopping the server"
stop_server

case "$scheme" in
mysql | mariadb)
	if [ "$mode" = docker ]; then
		dc exec -T db sh -c \
			'MYSQL_PWD="$(cat /run/secrets/db_password)" exec mysql -h 127.0.0.1 -u nbs -e "DROP DATABASE IF EXISTS nbs; CREATE DATABASE nbs CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;"'
		zcat "$file" | dc exec -T db sh -c \
			'MYSQL_PWD="$(cat /run/secrets/db_password)" exec mysql -h 127.0.0.1 -u nbs nbs'
	else
		eval "$(python3 - "$url" <<'PY'
import shlex, sys
from urllib.parse import urlsplit, unquote
p = urlsplit(sys.argv[1].strip())
for k, v in (("user", unquote(p.username or "")), ("password", unquote(p.password or "")),
             ("host", p.hostname or "127.0.0.1"), ("port", str(p.port or "")), ("database", unquote(p.path.lstrip("/")))):
    print(k + "=" + shlex.quote(v))
PY
)"
		{
			echo "[client]"
			echo "user=$user"
			echo "password=\"$password\""
			echo "host=$host"
			[ -n "$port" ] && echo "port=$port"
		} >"$work/client.cnf"
		mysql --defaults-extra-file="$work/client.cnf" -e "DROP DATABASE IF EXISTS \`$database\`; CREATE DATABASE \`$database\` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;"
		zcat "$file" | mysql --defaults-extra-file="$work/client.cnf" "$database"
	fi
	;;
postgres | postgresql)
	if [ "$mode" = docker ]; then
		dc exec -T db sh -c \
			'PGPASSWORD="$(cat /run/secrets/db_password)" exec psql -h 127.0.0.1 -U nbs -d nbs -v ON_ERROR_STOP=1 -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;"'
		dc exec -T db sh -c \
			'PGPASSWORD="$(cat /run/secrets/db_password)" exec pg_restore -h 127.0.0.1 -U nbs -d nbs --no-owner --no-privileges --exit-on-error --single-transaction' <"$file"
	else
		eval "$(python3 - "$url" <<'PY'
import shlex, sys
from urllib.parse import urlsplit, unquote
p = urlsplit(sys.argv[1].strip())
for k, v in (("user", unquote(p.username or "")), ("password", unquote(p.password or "")),
             ("host", p.hostname or "127.0.0.1"), ("port", str(p.port or "5432")), ("database", unquote(p.path.lstrip("/")))):
    print(k + "=" + shlex.quote(v))
PY
)"
		printf '%s:%s:%s:%s:%s\n' "$host" "$port" "$database" "$user" "$password" >"$work/pgpass"
		export PGPASSFILE="$work/pgpass"
		psql -h "$host" -p "$port" -U "$user" -d "$database" -v ON_ERROR_STOP=1 -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;"
		pg_restore -h "$host" -p "$port" -U "$user" -d "$database" --no-owner --no-privileges --exit-on-error --single-transaction "$file"
	fi
	;;
sqlite)
	path="${url#sqlite:}"
	path="${path%%\?*}"
	case "$path" in //*) path="${path#//}" ;; esac
	case "$path" in /*) ;; *) path="/var/lib/net-backend/$path" ;; esac
	aside="$path.before-restore-$(date -u +%Y%m%dT%H%M%SZ)"
	for suffix in "" -wal -shm; do
		if [ -e "$path$suffix" ]; then mv "$path$suffix" "$aside$suffix"; fi
	done
	install -o nbs -g nbs -m 0600 "$work/restore.db" "$path"
	log "the old database files are kept as $aside*"
	;;
*) fail "unknown database URL scheme '$scheme'" ;;
esac

# ---- 4. migrate, start, wait ----------------------------------------------------------------------
log "starting the server (migrate first)"
start_server
for _ in $(seq 1 60); do
	if [ "$mode" = docker ]; then
		state="$(dc ps --format '{{.Health}}' server 2>/dev/null || true)"
		if [ "$state" = healthy ]; then
			log "done: the server is healthy"
			exit 0
		fi
	else
		if setpriv --reuid=nbs --regid=nbs --init-groups env NBS_CONFIG=/etc/net-backend/config.toml /usr/local/bin/net-backend-server healthcheck 2>/dev/null; then
			log "done: the server is ready"
			exit 0
		fi
	fi
	sleep 2
done
fail "the server did not become ready within 2 minutes; check its log"
