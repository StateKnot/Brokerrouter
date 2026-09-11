-- Run as the deployment administrator in the migrated, dedicated database.
-- NOLOGIN group roles; attach separately provisioned TLS login roles externally.
BEGIN;
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_runtime') THEN
        CREATE ROLE brokerrouter_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_operator') THEN
        CREATE ROLE brokerrouter_operator NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
END $$;

REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO brokerrouter_runtime, brokerrouter_operator;
GRANT SELECT ON _sqlx_migrations, tenants, virtual_keys, models, key_models,
    fx_rates, endpoints, requests, ledger_entries, audit_events
    TO brokerrouter_runtime, brokerrouter_operator;

GRANT UPDATE (spent_micros, reserved_micros) ON tenants, virtual_keys TO brokerrouter_runtime;
GRANT INSERT ON requests, ledger_entries TO brokerrouter_runtime;
GRANT UPDATE (status, final_micros, result_ciphertext, error_code, updated_at)
    ON requests TO brokerrouter_runtime;
GRANT USAGE ON SEQUENCE ledger_entries_id_seq TO brokerrouter_runtime;

GRANT INSERT ON tenants, virtual_keys, models, key_models, endpoints, fx_rates, audit_events
    TO brokerrouter_operator;
GRANT UPDATE (name, active, budget_micros) ON tenants TO brokerrouter_operator;
GRANT UPDATE (name, budget_micros, expires_at, revoked_at) ON virtual_keys TO brokerrouter_operator;
GRANT UPDATE (name, active) ON models TO brokerrouter_operator;
GRANT UPDATE (active) ON endpoints TO brokerrouter_operator;
GRANT DELETE ON key_models TO brokerrouter_operator;
GRANT USAGE ON SEQUENCE fx_rates_id_seq, audit_events_id_seq TO brokerrouter_operator;
COMMIT;
