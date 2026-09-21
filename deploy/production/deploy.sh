#!/bin/sh
set -eu

root_dir="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "$root_dir"

: "${BROKERROUTER_SECRET_DIR:?set BROKERROUTER_SECRET_DIR}"
: "${BROKERROUTER_IMAGE_TAG:?set BROKERROUTER_IMAGE_TAG}"

compose='docker compose --file compose.production.yaml'

$compose config --quiet
$compose build gateway
$compose up --detach postgres
$compose --profile ops run --rm migrate
$compose --profile ops run --rm permissions
$compose up --detach gateway
$compose ps

attempt=0
until curl --fail --silent --show-error http://127.0.0.1:${BROKERROUTER_BIND_PORT:-18080}/health/ready >/dev/null; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 30 ]; then
        $compose logs --tail=100 gateway >&2
        exit 1
    fi
    sleep 2
done

echo "Brokerrouter data plane is ready on the loopback listener"
