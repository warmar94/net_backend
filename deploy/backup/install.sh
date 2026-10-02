#!/usr/bin/env bash
# Installs the daily database backup of net_backend_server (run as root):
#   /usr/local/bin/net-backend-backup, /usr/local/bin/net-backend-restore,
#   /etc/net-backend/backup.env (kept if it exists), net-backend-backup.service + .timer (enabled).
#
#   sudo bash deploy/backup/install.sh --mode systemd
#   sudo bash deploy/backup/install.sh --mode docker --compose-dir /opt/net_backend/deploy/docker
set -euo pipefail

mode="" compose_dir="" keep_days=14 backup_dir=/var/backups/net-backend
usage() {
	echo "usage: $0 --mode systemd|docker [--compose-dir <dir>] [--keep-days <n>] [--dir <backup dir>]" >&2
	exit 64
}
while [ "$#" -gt 0 ]; do
	case "$1" in
	--mode | --compose-dir | --keep-days | --dir)
		[ "$#" -ge 2 ] || usage
		case "$1" in --mode) mode="$2" ;; --compose-dir) compose_dir="$2" ;; --keep-days) keep_days="$2" ;; --dir) backup_dir="$2" ;; esac
		shift 2
		;;
	*) usage ;;
	esac
done
case "$mode" in systemd | docker) ;; *) echo "--mode systemd|docker is required" >&2; exit 64 ;; esac
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 77; }
here="$(cd "$(dirname "$0")" && pwd)"
if [ "$mode" = docker ]; then
	[ -n "$compose_dir" ] || compose_dir="$(cd "$here/../docker" && pwd)"
	[ -f "$compose_dir/compose.yaml" ] || { echo "no compose.yaml in $compose_dir" >&2; exit 66; }
fi

install -m 0755 "$here/backup.sh" /usr/local/bin/net-backend-backup
install -m 0755 "$here/restore.sh" /usr/local/bin/net-backend-restore
# Never re-mode an existing folder (the systemd install keeps /etc/net-backend at root:nbs 0750).
[ -d /etc/net-backend ] || install -d -m 0755 /etc/net-backend
if [ -e /etc/net-backend/backup.env ]; then
	echo "kept     /etc/net-backend/backup.env"
else
	sed -e "s|^NBS_BACKUP_MODE=.*|NBS_BACKUP_MODE=${mode}|" \
		-e "s|^NBS_BACKUP_DIR=.*|NBS_BACKUP_DIR=${backup_dir}|" \
		-e "s|^NBS_BACKUP_KEEP_DAYS=.*|NBS_BACKUP_KEEP_DAYS=${keep_days}|" \
		-e "s|^NBS_COMPOSE_DIR=.*|NBS_COMPOSE_DIR=${compose_dir:-/opt/net_backend/deploy/docker}|" \
		"$here/backup.env.example" >/etc/net-backend/backup.env
	chmod 0644 /etc/net-backend/backup.env
	echo "wrote    /etc/net-backend/backup.env"
fi
install -m 0644 "$here/net-backend-backup.service" /etc/systemd/system/net-backend-backup.service
install -m 0644 "$here/net-backend-backup.timer" /etc/systemd/system/net-backend-backup.timer
systemctl daemon-reload
systemctl enable --now net-backend-backup.timer
echo "Backups: daily into ${backup_dir} (see: systemctl list-timers net-backend-backup.timer)."
echo "Run one now: systemctl start net-backend-backup && journalctl -u net-backend-backup -n 20"
echo "Check the nightly result: systemctl status net-backend-backup (a failed run shows as failed)."
