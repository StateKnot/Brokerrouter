#!/bin/sh
set -eu

root_dir="${1:-/opt/brokerrouter/current}"
backup_dir="${2:-/var/backups/brokerrouter}"
cd "$root_dir"

: "${BROKERROUTER_SECRET_DIR:?set BROKERROUTER_SECRET_DIR}"
: "${BROKERROUTER_IMAGE_TAG:?set BROKERROUTER_IMAGE_TAG}"

umask 077
mkdir -p "$backup_dir"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
partial="$backup_dir/brokerrouter-${stamp}.dump.partial"
final="$backup_dir/brokerrouter-${stamp}.dump"

trap 'rm -f "$partial"' EXIT HUP INT TERM
docker compose --file compose.production.yaml exec --no-TTY postgres \
    /usr/local/sbin/brokerrouter-pg-dump >"$partial"
test -s "$partial"
mv "$partial" "$final"
trap - EXIT HUP INT TERM
find "$backup_dir" -type f -name 'brokerrouter-*.dump' -mtime +14 -delete
