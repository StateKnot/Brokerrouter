#!/bin/sh
set -eu

env_file="${BROKERROUTER_ENV_FILE:-}"
if [ -n "$env_file" ]; then
    if [ ! -r "$env_file" ]; then
        echo "configured Brokerrouter environment secret is not readable" >&2
        exit 1
    fi
    set -a
    # The file is root-managed deployment state, never user input.
    # shellcheck disable=SC1090
    . "$env_file"
    set +a
fi

if [ "$(id -u)" -eq 0 ]; then
    exec gosu brokerrouter:brokerrouter /usr/local/bin/brokerrouter "$@"
fi
exec /usr/local/bin/brokerrouter "$@"
