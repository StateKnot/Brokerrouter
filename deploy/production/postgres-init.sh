#!/bin/sh
set -eu

runtime_password="$(cat /run/secrets/postgres_runtime_password)"
operator_password="$(cat /run/secrets/postgres_operator_password)"

psql --set=ON_ERROR_STOP=1 \
    --set=runtime_password="$runtime_password" \
    --set=operator_password="$operator_password" \
    --username "$POSTGRES_USER" \
    --dbname "$POSTGRES_DB" <<'SQL'
SELECT format(
    'CREATE ROLE brokerrouter_runtime_login LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD %L',
    :'runtime_password'
) \gexec
SELECT format(
    'CREATE ROLE brokerrouter_operator_login LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD %L',
    :'operator_password'
) \gexec
ALTER ROLE brokerrouter_runtime_login SET statement_timeout = '15s';
ALTER ROLE brokerrouter_runtime_login SET lock_timeout = '3s';
ALTER ROLE brokerrouter_operator_login SET statement_timeout = '15s';
ALTER ROLE brokerrouter_operator_login SET lock_timeout = '3s';
REVOKE ALL ON DATABASE brokerrouter FROM PUBLIC;
GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_migration, brokerrouter_runtime_login, brokerrouter_operator_login;
SQL
