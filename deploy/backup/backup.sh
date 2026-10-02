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
#   NBS_BACKUP_TAG         an optional word in the file name (a-z, 0-9, -)
#   NBS_DATABASE_URL_FILE  systemd mode: the file holding the database URL (default /etc/net-backend/database_url)
#   NBS_COMPOSE_DIR        docker mode: the folder with compose.yaml and .env (default /opt/net-backend)
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
compose_dir="${NBS_COMPOSE_DIR:-/opt/net-backend}"

# One field of a PostgreSQL password file: `\` and `:` escaped with a backslash.
pgpass_field() {
	local v="${1//\\/\\\\}"
	printf '%s' "${v//:/\\:}"
}
log() { echo "net-backend-backup: $*"; }
fail() { echo "net-backend-backup: ERROR: $*" >&2; exit 1; }

case "$keep_days" in '' | *[!0-9]*) fail "NBS_BACKUP_KEEP_DAYS must be a number" ;; esac
umask 077
mkdir -p "$dir"
chmod 0700 "$dir"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
# An optional word in the name (restore.sh uses `before-restore` for its safety backup).
tag="${NBS_BACKUP_TAG:-}"
case "$tag" in *[!a-z0-9-]*) fail "NBS_BACKUP_TAG may hold only a-z, 0-9 and -" ;; esac
# The backup's name is claimed at the very end (see "place the backup" below).
kind="" ext=""
# docker mode: the Compose file of the install (compose.yaml, or one of the other names Compose reads).
has_compose_file() {
	local f
	for f in compose.yaml compose.yml docker-compose.yaml docker-compose.yml; do
		[ -f "$1/$f" ] && return 0
	done
	return 1
}
# docker mode: the database the `db` service runs (its secrets stay inside Docker volumes).
docker_scheme() {
	(cd "$compose_dir" && docker compose exec -T db sh -c \
		'if command -v pg_dump >/dev/null 2>&1; then echo postgres; elif command -v mysqldump >/dev/null 2>&1; then echo mysql; fi')
}
work="$(mktemp -d "$dir/.work.XXXXXX")"
# `claimed`: a name taken for this backup but not filled yet (removed if the run ends before).
claimed=""
trap 'rm -rf "$work"; [ -z "$claimed" ] || rm -f "$claimed"' EXIT

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

scheme="" user="" password="" host="" port="" database="" path=""
if [ "$mode" = docker ]; then
	has_compose_file "$compose_dir" || fail "no compose.yaml in $compose_dir (set NBS_COMPOSE_DIR)"
	scheme="$(docker_scheme)" || fail "the db service is not running (docker compose ps in $compose_dir)"
	[ -n "$scheme" ] || fail "the db service runs neither PostgreSQL nor MySQL (docker compose ps in $compose_dir)"
else
	[ -r "$url_file" ] || fail "cannot read $url_file (set NBS_DATABASE_URL_FILE)"
	eval "$(parse_url "$(cat "$url_file")")"
fi

case "$scheme" in
mysql | mariadb)
	kind=mysql ext=.sql.gz
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
	kind=postgres ext=.dump
	if [ "$mode" = docker ]; then
		(cd "$compose_dir" && docker compose exec -T db sh -c \
			'PGPASSWORD="$(cat /run/secrets/db_password)" exec pg_dump -h 127.0.0.1 -U nbs -d nbs -Fc') >"$work/dump"
		(cd "$compose_dir" && docker compose exec -T db pg_restore --list) <"$work/dump" >/dev/null ||
			fail "pg_restore cannot read the dump"
	else
		command -v pg_dump >/dev/null || fail "pg_dump is not installed (apt install postgresql-client)"
		pass="$work/pgpass"
		printf '%s:%s:%s:%s:%s\n' "$(pgpass_field "$host")" "${port:-5432}" "$(pgpass_field "$database")" \
			"$(pgpass_field "$user")" "$(pgpass_field "$password")" >"$pass"
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
	kind=sqlite ext=.db.gz
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
# Place the backup under a name no other backup has, never replacing one: the name is claimed
# atomically (noclobber creates it only if it does not exist), so backups started in the same second,
# even at the same instant, each get their own (-2, -3, ...).
base="$dir/net-backend-$kind${tag:+-$tag}-$stamp"
out="$base$ext"
n=1
until (set -C && : >"$out") 2>/dev/null; do
	[ -e "$out" ] || fail "cannot create $out"
	n=$((n + 1))
	[ "$n" -le 1000 ] || fail "no free name for $base$ext"
	out="$base-$n$ext"
done
claimed="$out"
mv -f "$final" "$out"
claimed=""
log "wrote $out ($(du -h "$out" | cut -f1))"

if [ "$keep_days" -gt 0 ]; then
	find "$dir" -maxdepth 1 -type f -name 'net-backend-*' -mtime +"$keep_days" -print -delete |
		sed 's/^/net-backend-backup: deleted (older than '"$keep_days"' days) /'
fi
