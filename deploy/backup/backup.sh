#!/usr/bin/env bash
# net-backend-backup: one consistent database backup of a net_backend_server install, checked, then
# old backups past the retention are deleted. Runs as root (the systemd timer does it daily).
#
#   MySQL / MariaDB: mysqldump --single-transaction (a consistent snapshot without locking writers)
#   PostgreSQL:      pg_dump -Fc (custom format, compressed; restore with pg_restore)
#   SQLite:          the online backup API (sqlite3 .backup), integrity-checked, gzip
#
# Settings (environment, else /etc/net-backend/backup.env or the file named by NBS_BACKUP_ENV):
#   NBS_BACKUP_MODE        systemd (the server runs as a systemd service; default) or docker (Compose)
#   NBS_BACKUP_DIR         where backups go (default /var/backups/net-backend)
#   NBS_BACKUP_KEEP_DAYS   delete backups older than this many days (default 14; 0 = keep all)
#   NBS_DATABASE_URL_FILE  systemd mode: the file holding the database URL (default /etc/net-backend/database_url)
#   NBS_COMPOSE_DIR        docker mode: the folder with compose.yaml and .env (default /opt/net_backend/deploy/docker)
#
# The database password never appears on a command line: a temporary option file (MySQL), a temporary
# password file (PostgreSQL), or inside the database container (docker mode).
set -euo pipefail

# Settings from the environment win; the rest come from the settings file, if any.
env_file="${NBS_BACKUP_ENV:-/etc/net-backend/backup.env}"
if [ -r "$env_file" ]; then
	while IFS='=' read -r key value; do
		case "$key" in NBS_[A-Z_]*) [ -n "${!key:-}" ] || export "$key=$value" ;; esac
	done <"$env_file"
fi

mode="${NBS_BACKUP_MODE:-systemd}"
dir="${NBS_BACKUP_DIR:-/var/backups/net-backend}"
keep_days="${NBS_BACKUP_KEEP_DAYS:-14}"
url_file="${NBS_DATABASE_URL_FILE:-/etc/net-backend/database_url}"
compose_dir="${NBS_COMPOSE_DIR:-/opt/net_backend/deploy/docker}"

log() { echo "net-backend-backup: $*"; }
fail() { echo "net-backend-backup: ERROR: $*" >&2; exit 1; }

case "$keep_days" in '' | *[!0-9]*) fail "NBS_BACKUP_KEEP_DAYS must be a number" ;; esac
umask 077
mkdir -p "$dir"
chmod 0700 "$dir"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
work="$(mktemp -d "$dir/.work.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# Splits a database URL into shell assignments (scheme, user, password, host, port, database, path),
# percent-decoded. Read with `eval` from a trusted file only.
parse_url() {
	python3 - "$1" <<'PY'
import shlex, sys
from urllib.parse import urlsplit, unquote
url = sys.argv[1].strip()
parts = urlsplit(url)
scheme = parts.scheme
if scheme == "sqlite":
    path = url[len("sqlite:"):]
    path = path.split("?", 1)[0]
    if path.startswith("//"):
        path = path[2:]
    print("scheme=sqlite")
    print("path=" + shlex.quote(path))
    sys.exit(0)
print("scheme=" + shlex.quote(scheme))
print("user=" + shlex.quote(unquote(parts.username or "")))
print("password=" + shlex.quote(unquote(parts.password or "")))
print("host=" + shlex.quote(parts.hostname or "127.0.0.1"))
print("port=" + shlex.quote(str(parts.port or "")))
print("database=" + shlex.quote(unquote(parts.path.lstrip("/"))))
PY
}

if [ "$mode" = docker ]; then
	[ -f "$compose_dir/compose.yaml" ] || fail "no compose.yaml in $compose_dir (set NBS_COMPOSE_DIR)"
	url="$(cat "$compose_dir/secrets/database_url")"
else
	[ -r "$url_file" ] || fail "cannot read $url_file (set NBS_DATABASE_URL_FILE)"
	url="$(cat "$url_file")"
fi
scheme="" user="" password="" host="" port="" database="" path=""
eval "$(parse_url "$url")"

case "$scheme" in
mysql | mariadb)
	out="$dir/net-backend-mysql-$stamp.sql.gz"
	dump_args="--single-transaction --quick --no-tablespaces --hex-blob --triggers --default-character-set=utf8mb4"
	if [ "$mode" = docker ]; then
		# Inside the database container, as the app's own account; the password stays in the container.
		(cd "$compose_dir" && docker compose exec -T db sh -c \
			"MYSQL_PWD=\"\$(cat /run/secrets/db_password)\" exec mysqldump -h 127.0.0.1 -u nbs $dump_args nbs") |
			gzip -c >"$work/dump.sql.gz"
	else
		command -v mysqldump >/dev/null || fail "mysqldump is not installed (apt install mysql-client)"
		cnf="$work/client.cnf"
		{
			echo "[client]"
			echo "user=$user"
			echo "password=\"$password\""
			echo "host=$host"
			[ -n "$port" ] && echo "port=$port"
		} >"$cnf"
		# shellcheck disable=SC2086
		mysqldump --defaults-extra-file="$cnf" $dump_args "$database" | gzip -c >"$work/dump.sql.gz"
	fi
	gzip -t "$work/dump.sql.gz" || fail "the dump is not a valid gzip file"
	# mysqldump writes this line last: a dump without it was cut off.
	tail_lines="$(zcat "$work/dump.sql.gz" | tail -n 3)"
	case "$tail_lines" in *"Dump completed"*) ;; *) fail "the dump is incomplete (no 'Dump completed' line)" ;; esac
	;;
postgres | postgresql)
	out="$dir/net-backend-postgres-$stamp.dump"
	if [ "$mode" = docker ]; then
		(cd "$compose_dir" && docker compose exec -T db sh -c \
			'PGPASSWORD="$(cat /run/secrets/db_password)" exec pg_dump -h 127.0.0.1 -U nbs -d nbs -Fc') >"$work/dump"
		(cd "$compose_dir" && docker compose exec -T db pg_restore --list) <"$work/dump" >/dev/null ||
			fail "pg_restore cannot read the dump"
	else
		command -v pg_dump >/dev/null || fail "pg_dump is not installed (apt install postgresql-client)"
		pass="$work/pgpass"
		printf '%s:%s:%s:%s:%s\n' "$host" "${port:-5432}" "$database" "$user" "$password" >"$pass"
		PGPASSFILE="$pass" pg_dump -h "$host" -p "${port:-5432}" -U "$user" -d "$database" -Fc -f "$work/dump"
		pg_restore --list "$work/dump" >/dev/null || fail "pg_restore cannot read the dump"
	fi
	mv "$work/dump" "$work/dump.final"
	;;
sqlite)
	[ "$mode" = docker ] && fail "SQLite is a systemd-install database; Compose uses MySQL or PostgreSQL"
	command -v sqlite3 >/dev/null || fail "sqlite3 is not installed (apt install sqlite3)"
	# A relative path is relative to the service's working directory.
	case "$path" in /*) ;; *) path="/var/lib/net-backend/$path" ;; esac
	[ -f "$path" ] || fail "the SQLite database $path does not exist"
	out="$dir/net-backend-sqlite-$stamp.db.gz"
	# The online backup API: consistent while the server keeps writing (WAL).
	sqlite3 "$path" ".timeout 10000" ".backup '$work/copy.db'"
	[ "$(sqlite3 "$work/copy.db" 'PRAGMA integrity_check;')" = ok ] || fail "the copy fails PRAGMA integrity_check"
	gzip -c "$work/copy.db" >"$work/dump.db.gz"
	;;
*)
	fail "unknown database URL scheme '$scheme'"
	;;
esac

final="$(find "$work" -maxdepth 1 -type f \( -name 'dump.sql.gz' -o -name 'dump.final' -o -name 'dump.db.gz' \) | head -n 1)"
[ -s "$final" ] || fail "the backup is empty"
mv "$final" "$out"
log "wrote $out ($(du -h "$out" | cut -f1))"

if [ "$keep_days" -gt 0 ]; then
	find "$dir" -maxdepth 1 -type f -name 'net-backend-*' -mtime +"$keep_days" -print -delete |
		sed 's/^/net-backend-backup: deleted (older than '"$keep_days"' days) /'
fi
