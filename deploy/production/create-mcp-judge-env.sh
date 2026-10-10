#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 ABSOLUTE_SECRET_DIRECTORY" >&2
    exit 2
fi

secret_dir="$1"
case "$secret_dir" in
    /*) ;;
    *) echo "secret directory must be absolute" >&2; exit 2 ;;
esac
source_file="$secret_dir/runtime.env"
target_file="$secret_dir/mcp-judge.env"
if [ ! -f "$source_file" ] || [ -e "$target_file" ]; then
    echo "runtime.env must exist and mcp-judge.env must not exist" >&2
    exit 1
fi

umask 077
temporary="$(mktemp "$secret_dir/.mcp-judge.env.XXXXXX")"
trap 'rm -f "$temporary"' EXIT
trap 'exit 1' HUP INT TERM
for name in DATABASE_URL BROKERROUTER_ACTIVE_RESULT_KEY BROKERROUTER_RESULT_KEYS; do
    count="$(grep -c "^${name}=" "$source_file" || true)"
    if [ "$count" -ne 1 ]; then
        echo "runtime.env must contain exactly one $name entry" >&2
        exit 1
    fi
    grep "^${name}=" "$source_file" >>"$temporary"
done
chmod 0400 "$temporary"
ln "$temporary" "$target_file"
rm -f "$temporary"
trap - EXIT HUP INT TERM
echo "MCP judge worker environment secret created; add only its selected judge provider credentials"
