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
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_reconciler') THEN
        CREATE ROLE brokerrouter_reconciler NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_governance_control') THEN
        CREATE ROLE brokerrouter_governance_control NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
END $$;

REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO brokerrouter_runtime, brokerrouter_operator, brokerrouter_governance_control;
GRANT SELECT ON _sqlx_migrations, tenants, virtual_keys, models, key_models,
    fx_rates, endpoints, requests, ledger_entries, audit_events, attempts
    TO brokerrouter_runtime, brokerrouter_operator;

GRANT UPDATE (spent_micros, reserved_micros) ON tenants, virtual_keys TO brokerrouter_runtime;
GRANT INSERT ON requests, ledger_entries, attempts TO brokerrouter_runtime;
GRANT UPDATE (status, final_micros, result_ciphertext, result_expires_at, error_code, updated_at)
    ON requests TO brokerrouter_runtime;
GRANT USAGE ON SEQUENCE ledger_entries_id_seq TO brokerrouter_runtime;
GRANT UPDATE (status,http_status,error_code,usage,cost_micros,finished_at) ON attempts TO brokerrouter_runtime;

GRANT INSERT ON tenants, virtual_keys, models, key_models, endpoints, fx_rates, audit_events
    TO brokerrouter_operator;
GRANT UPDATE (name, active, budget_micros) ON tenants TO brokerrouter_operator;
GRANT UPDATE (name, budget_micros, expires_at, revoked_at) ON virtual_keys TO brokerrouter_operator;
GRANT UPDATE (name, active) ON models TO brokerrouter_operator;
GRANT UPDATE (active) ON endpoints TO brokerrouter_operator;
GRANT DELETE ON key_models TO brokerrouter_operator;
GRANT USAGE ON SEQUENCE fx_rates_id_seq, audit_events_id_seq TO brokerrouter_operator;
GRANT brokerrouter_runtime TO brokerrouter_reconciler;
GRANT INSERT ON audit_events TO brokerrouter_reconciler;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_reconciler;

GRANT SELECT ON _sqlx_migrations, tenants, virtual_keys, models, key_models, requests TO brokerrouter_governance_control;
GRANT SELECT ON governance_users, runtime_bindings, governance_turns, guardrails,
    guardrail_versions, guardrail_policies, governance_operations, approvals,
    guardrail_grants, governance_events, governance_event_acks
    TO brokerrouter_runtime, brokerrouter_operator, brokerrouter_governance_control;
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_governance_control;
GRANT INSERT ON governance_users,runtime_bindings,guardrails,guardrail_versions,guardrail_policies TO brokerrouter_operator;
GRANT UPDATE(active) ON governance_users,runtime_bindings TO brokerrouter_operator;
GRANT UPDATE(version,active) ON guardrails TO brokerrouter_operator;
GRANT UPDATE(guardrail_id,model_id,key_id,runtime_id,tool_name,active) ON guardrail_policies TO brokerrouter_operator;
GRANT INSERT ON governance_operations,approvals,governance_events,governance_event_acks TO brokerrouter_runtime;
GRANT UPDATE(state,payload_ciphertext,claim_hash,execution_deadline,receipt_hash,receipt_ciphertext) ON governance_operations TO brokerrouter_runtime;
GRANT UPDATE(event_clock,state) ON governance_turns TO brokerrouter_runtime;
GRANT UPDATE(sequence) ON governance_event_acks TO brokerrouter_runtime;
-- The model server cannot decide approvals, register users/turns or mint grants.
GRANT INSERT ON governance_turns,guardrail_grants,governance_events,human_assertions TO brokerrouter_governance_control;
GRANT SELECT ON human_assertions TO brokerrouter_governance_control;
GRANT DELETE ON human_assertions TO brokerrouter_governance_control;
GRANT UPDATE(expires_at) ON human_assertions TO brokerrouter_governance_control;
GRANT UPDATE(state,event_clock) ON governance_turns TO brokerrouter_governance_control;
GRANT UPDATE(state,decision_hash,decided_by,feedback_ciphertext,decided_at) ON approvals TO brokerrouter_governance_control;
GRANT UPDATE(state,payload_ciphertext,receipt_hash,receipt_ciphertext) ON governance_operations TO brokerrouter_governance_control;
GRANT UPDATE(result_ciphertext) ON requests TO brokerrouter_governance_control;
COMMIT;
