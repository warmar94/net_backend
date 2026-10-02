#!/usr/bin/env bash
# Prepares a Docker Compose install of net_backend_server in this folder: .env, generated database
# secrets, config.toml and the migrations folder. It never overwrites a secret (the database volume
# was initialised with it) and keeps an existing .env / config.toml unless --force is given (then the
# old config.toml is kept as config.toml.bak).
#
#   sudo bash setup.sh --db mysql|postgres --domain api.example.com --email admin@example.com
#   docker compose up -d --build
#
# The Caddy <-> server network's addresses are in .env (EDGE_SUBNET, EDGE_SUBNET6, CADDY_IPV4,
# CADDY_IPV6); change them there only if they collide with a network on this machine.
set -euo pipefail

usage() {
	echo "usage: $0 --db mysql|postgres --domain <host name> --email <address> [--force]" >&2
	exit 64
}

db="" domain="" email="" force=0
while [ "$#" -gt 0 ]; do
	case "$1" in
	--db | --domain | --email)
		[ "$#" -ge 2 ] || usage
		case "$1" in --db) db="$2" ;; --domain) domain="$2" ;; --email) email="$2" ;; esac
		shift 2
		;;
	--force) force=1; shift ;;
	*) usage ;;
	esac
done
case "$db" in mysql | postgres) ;; *) usage ;; esac
[ -n "$domain" ] && [ -n "$email" ] || usage
case "$domain" in *[!A-Za-z0-9.-]*) echo "invalid host name: $domain" >&2; exit 64 ;; esac
case "$email" in *[!A-Za-z0-9.@+_-]* | *@*@* | @* | *@) echo "invalid email: $email" >&2; exit 64 ;; *@*) ;; *) echo "invalid email: $email" >&2; exit 64 ;; esac

cd "$(dirname "$0")"
command -v docker >/dev/null || { echo "docker is not installed" >&2; exit 69; }
docker compose version >/dev/null 2>&1 || { echo "the docker compose plugin is not installed" >&2; exit 69; }

# 48 hex characters: URL-safe, no quoting needed anywhere.
random_hex() { head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

umask 077
mkdir -p secrets
chmod 0700 secrets

# Readable by the containers' own users (the server runs as uid 65532); the folder stays 0700.
write_secret() {
	local file="secrets/$1" value="$2"
	if [ -s "$file" ]; then
		echo "kept     $file"
	else
		printf '%s' "$value" >"$file"
		echo "created  $file"
	fi
	chmod 0644 "$file"
}

# The app's password and the database's own administrator password (MySQL root / PostgreSQL postgres).
write_secret db_password "$(random_hex)"
write_secret db_root_password "$(random_hex)"
password="$(cat secrets/db_password)"
if [ "$db" = mysql ]; then
	url="mysql://nbs:${password}@db:3306/nbs"
else
	url="postgres://nbs:${password}@db:5432/nbs"
fi
if [ -s secrets/database_url ] && [ "$(cat secrets/database_url)" != "$url" ] && [ "$force" -ne 1 ]; then
	echo "secrets/database_url exists for another database; remove it or use --force" >&2
	exit 1
fi
printf '%s' "$url" >secrets/database_url
chmod 0644 secrets/database_url
echo "wrote    secrets/database_url"

umask 022
if [ -e .env ] && [ "$force" -ne 1 ]; then
	echo "kept     .env (use --force to rewrite it)"
else
	sed -e "s|^COMPOSE_FILE=.*|COMPOSE_FILE=compose.yaml:compose.${db}.yaml|" \
		-e "s|^DOMAIN=.*|DOMAIN=${domain}|" \
		-e "s|^ACME_EMAIL=.*|ACME_EMAIL=${email}|" \
		.env.example >.env
	chmod 0600 .env
	echo "wrote    .env"
fi

if [ -e config.toml ] && [ "$force" -ne 1 ]; then
	echo "kept     config.toml"
else
	if [ -e config.toml ]; then
		cp -p config.toml config.toml.bak
		echo "kept     config.toml.bak (the previous config.toml)"
	fi
	cp ../config/config.toml config.toml
	echo "wrote    config.toml (edit [modules.auth] app_name, mail and the chat rooms)"
fi
mkdir -p migrations

echo
echo "Next: review config.toml, then"
echo "  docker compose up -d --build"
echo "  docker compose ps -a"
