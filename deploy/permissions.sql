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
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_assets') THEN
        CREATE ROLE brokerrouter_assets NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
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
GRANT SELECT ON guardrail_evaluations TO brokerrouter_runtime,brokerrouter_operator,brokerrouter_governance_control;
GRANT INSERT ON guardrail_evaluations TO brokerrouter_runtime;
GRANT UPDATE(state,reason_code,deadline,finished_at) ON guardrail_evaluations TO brokerrouter_runtime;
GRANT USAGE ON SCHEMA public TO brokerrouter_assets;
GRANT SELECT ON _sqlx_migrations,tenants,virtual_keys,assets,asset_chunks TO brokerrouter_assets;
GRANT SELECT ON assets,asset_chunks TO brokerrouter_operator;
GRANT INSERT ON assets,asset_chunks,audit_events TO brokerrouter_assets;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_assets;
GRANT UPDATE(asset_used_bytes) ON tenants,virtual_keys TO brokerrouter_assets;
GRANT UPDATE(asset_quota_bytes) ON tenants,virtual_keys TO brokerrouter_operator;
GRANT UPDATE(state,completed_at,deleted_at,gc_after,gc_checked_at,gc_cursor) ON assets TO brokerrouter_assets;
GRANT UPDATE(state) ON asset_chunks TO brokerrouter_assets;
GRANT SELECT ON media_endpoints,media_jobs,media_events TO brokerrouter_runtime,brokerrouter_operator;
GRANT INSERT ON media_endpoints TO brokerrouter_operator;
GRANT UPDATE(active) ON media_endpoints TO brokerrouter_operator;
GRANT INSERT ON media_jobs,media_events TO brokerrouter_runtime;
GRANT UPDATE(state,input_ciphertext,output_ciphertext,output_expires_at,provider_task_id,provider_status,cancel_requested,next_poll_at,lease_token,lease_until,event_clock,error_code) ON media_jobs TO brokerrouter_runtime;
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_media_outputs') THEN CREATE ROLE brokerrouter_media_outputs NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; END IF; END $$;
-- Separate purpose roles: inherited input membership would allow SET ROLE to bypass purpose RLS.
REVOKE brokerrouter_assets FROM brokerrouter_media_outputs;
GRANT USAGE ON SCHEMA public TO brokerrouter_media_outputs;
GRANT SELECT ON _sqlx_migrations,tenants,virtual_keys,assets,asset_chunks TO brokerrouter_media_outputs;
GRANT INSERT ON assets,asset_chunks,audit_events TO brokerrouter_media_outputs;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_media_outputs;
GRANT UPDATE(asset_used_bytes) ON tenants,virtual_keys TO brokerrouter_media_outputs;
GRANT UPDATE(state,completed_at,deleted_at,gc_after,gc_checked_at,gc_cursor) ON assets TO brokerrouter_media_outputs;
GRANT UPDATE(state) ON asset_chunks TO brokerrouter_media_outputs;
GRANT SELECT ON media_output_imports TO brokerrouter_runtime,brokerrouter_operator,brokerrouter_media_outputs;
GRANT SELECT ON assets TO brokerrouter_runtime;
GRANT INSERT ON media_output_imports TO brokerrouter_runtime;
GRANT UPDATE(reserved_bytes,state,error_code) ON media_output_imports TO brokerrouter_runtime;
GRANT UPDATE(asset_used_bytes) ON tenants,virtual_keys TO brokerrouter_runtime;
GRANT SELECT ON media_jobs,requests,governance_turns,runtime_bindings,governance_users TO brokerrouter_media_outputs;
GRANT UPDATE(output_ciphertext) ON media_jobs TO brokerrouter_media_outputs;
GRANT UPDATE(reserved_bytes,asset_id,source_etag,source_hash,state,error_code,lease_token,lease_until,next_attempt_at) ON media_output_imports TO brokerrouter_media_outputs;
GRANT UPDATE(sha256) ON assets TO brokerrouter_media_outputs;
DROP POLICY IF EXISTS brokerrouter_asset_metadata ON assets;
CREATE POLICY brokerrouter_asset_metadata ON assets FOR SELECT TO brokerrouter_runtime,brokerrouter_operator USING(true);
DROP POLICY IF EXISTS brokerrouter_asset_storage ON assets;
CREATE POLICY brokerrouter_asset_storage ON assets TO brokerrouter_assets,brokerrouter_media_outputs USING(true) WITH CHECK(true);
DROP POLICY IF EXISTS brokerrouter_asset_purpose ON assets;
CREATE POLICY brokerrouter_asset_purpose ON assets AS RESTRICTIVE TO brokerrouter_assets,brokerrouter_media_outputs
    USING(kind=CASE WHEN pg_has_role(current_user,'brokerrouter_media_outputs','MEMBER') THEN 'output' ELSE 'input' END)
    WITH CHECK(kind=CASE WHEN pg_has_role(current_user,'brokerrouter_media_outputs','MEMBER') THEN 'output' ELSE 'input' END);
DROP POLICY IF EXISTS brokerrouter_chunk_metadata ON asset_chunks;
CREATE POLICY brokerrouter_chunk_metadata ON asset_chunks FOR SELECT TO brokerrouter_operator USING(true);
DROP POLICY IF EXISTS brokerrouter_chunk_storage ON asset_chunks;
CREATE POLICY brokerrouter_chunk_storage ON asset_chunks TO brokerrouter_assets,brokerrouter_media_outputs USING(true) WITH CHECK(true);
DROP POLICY IF EXISTS brokerrouter_chunk_purpose ON asset_chunks;
CREATE POLICY brokerrouter_chunk_purpose ON asset_chunks AS RESTRICTIVE TO brokerrouter_assets,brokerrouter_media_outputs
    USING(EXISTS(SELECT 1 FROM assets a WHERE a.tenant_id=asset_chunks.tenant_id AND a.id=asset_chunks.asset_id))
    WITH CHECK(EXISTS(SELECT 1 FROM assets a WHERE a.tenant_id=asset_chunks.tenant_id AND a.id=asset_chunks.asset_id));
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_output_scan') THEN CREATE ROLE brokerrouter_output_scan NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; END IF; END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_output_scan;
GRANT SELECT ON output_scan_profiles,output_scanner_tokens,output_scans,output_scan_attempts TO brokerrouter_output_scan,brokerrouter_operator;
GRANT SELECT ON output_scan_profiles,output_scans TO brokerrouter_runtime;
GRANT INSERT ON output_scan_profiles,output_scanner_tokens TO brokerrouter_operator;
GRANT UPDATE(active) ON output_scan_profiles TO brokerrouter_operator;
GRANT UPDATE(revoked_at) ON output_scanner_tokens TO brokerrouter_operator;
GRANT SELECT ON _sqlx_migrations,tenants,virtual_keys,models,key_models,requests,media_jobs,media_output_imports,assets,asset_chunks,governance_turns,runtime_bindings,governance_users TO brokerrouter_output_scan;
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_output_scan;
GRANT INSERT ON output_scans,output_scan_attempts,audit_events TO brokerrouter_output_scan;
GRANT UPDATE(state,current_attempt,attempts,report,report_hash,finished_at) ON output_scans TO brokerrouter_output_scan;
GRANT UPDATE(lease_until) ON output_scan_attempts TO brokerrouter_output_scan;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_output_scan;
DROP POLICY IF EXISTS brokerrouter_scan_asset_read ON assets;
CREATE POLICY brokerrouter_scan_asset_read ON assets FOR SELECT TO brokerrouter_output_scan USING(kind='output');
DROP POLICY IF EXISTS brokerrouter_scan_chunk_read ON asset_chunks;
CREATE POLICY brokerrouter_scan_chunk_read ON asset_chunks FOR SELECT TO brokerrouter_output_scan
    USING(EXISTS(SELECT 1 FROM assets a WHERE a.tenant_id=asset_chunks.tenant_id AND a.id=asset_chunks.asset_id AND a.kind='output'));
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_output_delivery') THEN CREATE ROLE brokerrouter_output_delivery NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; END IF; END $$;
-- Do not attach runtime, scanner, storage, or human-control membership to this role.
GRANT USAGE ON SCHEMA public TO brokerrouter_output_delivery;
GRANT SELECT ON _sqlx_migrations,tenants,virtual_keys,models,key_models,requests,
    media_jobs,media_output_imports,assets,asset_chunks,output_scans,output_scan_profiles,
    governance_users,runtime_bindings,governance_turns,guardrails,guardrail_versions,
    guardrail_policies,governance_operations,approvals,guardrail_grants,human_assertions,
    media_output_deliveries TO brokerrouter_output_delivery;
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_output_delivery;
GRANT UPDATE(event_clock) ON governance_turns TO brokerrouter_output_delivery;
GRANT UPDATE(state) ON governance_operations TO brokerrouter_output_delivery;
GRANT INSERT ON governance_operations,approvals,governance_events,human_assertions,
    media_output_deliveries,audit_events TO brokerrouter_output_delivery;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_output_delivery;
GRANT SELECT ON media_output_deliveries TO brokerrouter_runtime,brokerrouter_operator,brokerrouter_governance_control;
GRANT SELECT ON media_jobs,media_output_imports,assets,output_scans,output_scan_profiles TO brokerrouter_governance_control;
DROP POLICY IF EXISTS brokerrouter_delivery_output_read ON assets;
CREATE POLICY brokerrouter_delivery_output_read ON assets FOR SELECT TO brokerrouter_output_delivery,brokerrouter_governance_control USING(kind='output');
DROP POLICY IF EXISTS brokerrouter_delivery_output_chunk_read ON asset_chunks;
CREATE POLICY brokerrouter_delivery_output_chunk_read ON asset_chunks FOR SELECT TO brokerrouter_output_delivery
    USING(EXISTS(SELECT 1 FROM assets a WHERE a.tenant_id=asset_chunks.tenant_id AND a.id=asset_chunks.asset_id AND a.kind='output'));
COMMIT;
