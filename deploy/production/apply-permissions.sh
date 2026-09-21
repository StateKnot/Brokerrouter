#!/bin/sh
set -eu

export PGPASSWORD="$(cat /run/secrets/postgres_migration_password)"
dsn="host=postgres port=5432 dbname=brokerrouter user=brokerrouter_migration sslmode=verify-full sslrootcert=/run/brokerrouter/tls/ca.crt"

psql "$dsn" --set=ON_ERROR_STOP=1 --file=/opt/brokerrouter/permissions.sql
psql "$dsn" --set=ON_ERROR_STOP=1 <<'SQL'
GRANT brokerrouter_runtime TO brokerrouter_runtime_login;
GRANT brokerrouter_operator TO brokerrouter_operator_login;
GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_runtime, brokerrouter_operator;
REVOKE ALL ON SCHEMA public FROM brokerrouter_runtime_login, brokerrouter_operator_login;
SQL
