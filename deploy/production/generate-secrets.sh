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

if [ -e "$secret_dir/runtime.env" ]; then
    echo "refusing to overwrite an existing deployment secret set" >&2
    exit 1
fi

umask 077
mkdir -p "$secret_dir"

migration_password="$(openssl rand -hex 32)"
runtime_password="$(openssl rand -hex 32)"
operator_password="$(openssl rand -hex 32)"
result_key="$(openssl rand -hex 32)"
key_id="result-$(date -u +%Y%m%d)"

printf '%s' "$migration_password" >"$secret_dir/postgres-migration-password"
printf '%s' "$runtime_password" >"$secret_dir/postgres-runtime-password"
printf '%s' "$operator_password" >"$secret_dir/postgres-operator-password"

openssl req -x509 -newkey rsa:3072 -sha256 -days 825 -nodes \
    -subj '/CN=Brokerrouter PostgreSQL Internal CA' \
    -keyout "$secret_dir/pg-ca.key" \
    -out "$secret_dir/pg-ca.crt"
openssl req -newkey rsa:3072 -sha256 -nodes \
    -subj '/CN=postgres' \
    -addext 'subjectAltName=DNS:postgres' \
    -keyout "$secret_dir/pg-server.key" \
    -out "$secret_dir/pg-server.csr"
openssl x509 -req -sha256 -days 397 \
    -in "$secret_dir/pg-server.csr" \
    -CA "$secret_dir/pg-ca.crt" \
    -CAkey "$secret_dir/pg-ca.key" \
    -CAcreateserial \
    -copy_extensions copy \
    -out "$secret_dir/pg-server.crt"
rm -f "$secret_dir/pg-server.csr" "$secret_dir/pg-ca.srl" "$secret_dir/pg-ca.key"

cat >"$secret_dir/migration.env" <<EOF
DATABASE_URL='postgres://brokerrouter_migration:${migration_password}@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca'
EOF
cat >"$secret_dir/runtime.env" <<EOF
DATABASE_URL='postgres://brokerrouter_runtime_login:${runtime_password}@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca'
BROKERROUTER_ACTIVE_RESULT_KEY='${key_id}'
BROKERROUTER_RESULT_KEYS='{"${key_id}":"${result_key}"}'
EOF
cat >"$secret_dir/operator.env" <<EOF
DATABASE_URL='postgres://brokerrouter_operator_login:${operator_password}@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca'
EOF

chmod 0400 "$secret_dir"/*
chmod 0444 "$secret_dir/pg-ca.crt" "$secret_dir/pg-server.crt"
chown 999:999 \
    "$secret_dir/pg-server.key" \
    "$secret_dir/pg-server.crt" \
    "$secret_dir/postgres-runtime-password" \
    "$secret_dir/postgres-operator-password"

unset migration_password runtime_password operator_password result_key
echo "Brokerrouter deployment secrets created"
