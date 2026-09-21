#!/bin/sh
set -eu

export PGPASSWORD="$(cat /run/secrets/postgres_migration_password)"
exec pg_dump \
    --host=127.0.0.1 \
    --port=5432 \
    --username=brokerrouter_migration \
    --dbname=brokerrouter \
    --format=custom \
    --compress=6 \
    --no-password
