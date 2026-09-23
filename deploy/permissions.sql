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
-- Console reports are a separate read-only service, with no authority to approve,
-- execute, decrypt, reconcile, change budgets or grant its own permissions.
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_console') THEN
    CREATE ROLE brokerrouter_console NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
END IF; END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_console;
GRANT SELECT ON _sqlx_migrations,console_permissions,console_request_facts,console_audit_facts TO brokerrouter_console;
GRANT SELECT ON console_trace_turns,console_trace_nodes,console_trace_requests,console_trace_links,console_trace_attempts,console_trace_approvals,console_trace_grants,console_trace_events TO brokerrouter_console;
GRANT SELECT(id,active) ON tenants TO brokerrouter_console;
GRANT SELECT(tenant_id,id,issuer,subject,active) ON governance_users TO brokerrouter_console;
GRANT SELECT,INSERT ON console_permissions TO brokerrouter_operator;
GRANT UPDATE(enabled) ON console_permissions TO brokerrouter_operator;
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_bff') THEN
    CREATE ROLE brokerrouter_bff NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
END IF; END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_bff;
GRANT SELECT ON _sqlx_migrations,bff_identity_bindings TO brokerrouter_bff;
GRANT SELECT(id,active,bff_epoch) ON tenants TO brokerrouter_bff;
GRANT SELECT(tenant_id,id,issuer,subject,active,bff_epoch) ON governance_users TO brokerrouter_bff;
GRANT SELECT,INSERT,DELETE ON bff_logins,bff_sessions TO brokerrouter_bff;
GRANT UPDATE(revoked_at) ON bff_sessions TO brokerrouter_bff;
GRANT INSERT ON audit_events TO brokerrouter_bff;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_bff;
GRANT SELECT,INSERT ON bff_identity_bindings TO brokerrouter_operator;
GRANT UPDATE(active,generation) ON bff_identity_bindings TO brokerrouter_operator;
-- Configuration is NOT a permission escalation of the read-only report service.
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_catalog') THEN
    CREATE ROLE brokerrouter_catalog NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
END IF; END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_catalog;
GRANT SELECT ON _sqlx_migrations,console_permissions,models,endpoints,catalog_mutations TO brokerrouter_catalog;
GRANT SELECT(id,active) ON tenants TO brokerrouter_catalog;
GRANT SELECT(tenant_id,id,issuer,subject,active) ON governance_users TO brokerrouter_catalog;
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_catalog;
GRANT UPDATE(catalog_lock) ON governance_users,console_permissions TO brokerrouter_catalog;
GRANT INSERT ON models,endpoints,catalog_mutations,audit_events TO brokerrouter_catalog;
GRANT SELECT,INSERT ON catalog_endpoint_connections TO brokerrouter_catalog;
GRANT UPDATE(name,active) ON models TO brokerrouter_catalog;
GRANT UPDATE(name,active,priority,currency,input_per_million,output_per_million) ON endpoints TO brokerrouter_catalog;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_catalog;

-- Key/FX administration is not catalog, reporting, approval, or inference authority.
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_settings') THEN
    CREATE ROLE brokerrouter_settings NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
END IF; END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_settings;
GRANT SELECT ON _sqlx_migrations,console_permissions,settings_mutations,fx_rates,key_models TO brokerrouter_settings;
GRANT SELECT(id,active,budget_micros,spent_micros,reserved_micros) ON tenants TO brokerrouter_settings;
GRANT SELECT(tenant_id,id,issuer,subject,active) ON governance_users TO brokerrouter_settings;
GRANT SELECT(tenant_id,id,name,active) ON models TO brokerrouter_settings;
GRANT SELECT(tenant_id,id,name,key_prefix,budget_micros,spent_micros,reserved_micros,expires_at,revoked_at,created_at,config_revision) ON virtual_keys TO brokerrouter_settings;
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_settings;
GRANT UPDATE(catalog_lock) ON governance_users,console_permissions TO brokerrouter_settings;
GRANT INSERT ON virtual_keys,key_models,fx_rates,settings_mutations,audit_events TO brokerrouter_settings;
GRANT UPDATE(name,budget_micros,expires_at,revoked_at,config_revision) ON virtual_keys TO brokerrouter_settings;
GRANT DELETE ON key_models TO brokerrouter_settings;
GRANT USAGE ON SEQUENCE fx_rates_id_seq,audit_events_id_seq TO brokerrouter_settings;
-- Grant-change trigger runs as the caller; operator CLI creation must still work.
GRANT UPDATE(config_revision) ON virtual_keys TO brokerrouter_operator;
-- Governance configuration shares the human settings listener, but has separate application grants.
-- No access to approvals, turn payloads, scan receipts or runtime/scanner credentials.
GRANT SELECT,INSERT ON guardrails,guardrail_versions,guardrail_policies TO brokerrouter_settings;
GRANT UPDATE(version,active) ON guardrails TO brokerrouter_settings;
GRANT UPDATE(guardrail_id,model_id,key_id,runtime_id,tool_name,active) ON guardrail_policies TO brokerrouter_settings;
GRANT SELECT(tenant_id,key_id,user_id,runtime_id,active) ON runtime_bindings TO brokerrouter_settings;
GRANT SELECT(tenant_id,id,model_id,version,active) ON output_scan_profiles TO brokerrouter_settings;
GRANT SELECT(tenant_id,model_id,active,operation) ON endpoints TO brokerrouter_settings;
GRANT SELECT,INSERT ON guardrail_observations TO brokerrouter_runtime,brokerrouter_output_delivery;
GRANT UPDATE(approval_skipped_at) ON guardrail_observations TO brokerrouter_runtime,brokerrouter_output_delivery;
GRANT SELECT ON monitoring_coverage,console_guardrail_facts,console_approval_facts,console_active_guardrails TO brokerrouter_console;
-- Human-owned review/decision service. No turn creation, execution receipts, model keys or assets.
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_approvals') THEN
    CREATE ROLE brokerrouter_approvals NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
END IF; END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_approvals;
GRANT SELECT ON _sqlx_migrations,governance_users,runtime_bindings,governance_turns,guardrails,guardrail_versions,guardrail_policies,governance_operations,approvals,approval_reviews,guardrail_grants,human_assertions,key_models TO brokerrouter_approvals;
GRANT SELECT(id,active) ON tenants TO brokerrouter_approvals;
GRANT SELECT(tenant_id,id,revoked_at,expires_at) ON virtual_keys TO brokerrouter_approvals;
GRANT SELECT(tenant_id,id,active) ON models TO brokerrouter_approvals;
GRANT SELECT(tenant_id,id,governance_turn_id) ON requests TO brokerrouter_approvals;
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_approvals;
GRANT INSERT ON approval_reviews,human_assertions,guardrail_grants,governance_events,audit_events TO brokerrouter_approvals;
GRANT DELETE ON human_assertions TO brokerrouter_approvals;
GRANT UPDATE(expires_at) ON human_assertions TO brokerrouter_approvals;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_approvals;
GRANT UPDATE(state,event_clock) ON governance_turns TO brokerrouter_approvals;
GRANT UPDATE(state,decision_hash,decided_by,feedback_ciphertext,decided_at) ON approvals TO brokerrouter_approvals;
GRANT UPDATE(state,payload_ciphertext) ON governance_operations TO brokerrouter_approvals;
GRANT UPDATE(result_ciphertext) ON requests TO brokerrouter_approvals;
-- The review reader cannot decide, grant, execute, change policies or write storage.
DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_media_review') THEN
    CREATE ROLE brokerrouter_media_review NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
END IF; END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_media_review;
GRANT SELECT ON _sqlx_migrations,governance_users,runtime_bindings,governance_turns,guardrails,guardrail_versions,guardrail_policies,governance_operations,approvals,guardrail_grants,human_assertions,key_models,approval_reviews,media_review_sessions,media_review_chunks,media_review_completions,media_output_deliveries,output_scans,output_scan_profiles,assets,asset_chunks TO brokerrouter_media_review;
GRANT SELECT(id,active) ON tenants TO brokerrouter_media_review;
GRANT SELECT(tenant_id,id,revoked_at,expires_at) ON virtual_keys TO brokerrouter_media_review;
GRANT SELECT(tenant_id,id,active) ON models TO brokerrouter_media_review;
GRANT SELECT(tenant_id,id,governance_turn_id,model_id) ON requests TO brokerrouter_media_review;
GRANT SELECT(tenant_id,id,key_id,state,cancel_requested) ON media_jobs TO brokerrouter_media_review,brokerrouter_approvals;
GRANT SELECT(tenant_id,job_id,asset_id,state) ON media_output_imports TO brokerrouter_media_review,brokerrouter_approvals;
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_media_review;
GRANT UPDATE(event_clock) ON governance_turns TO brokerrouter_media_review;
GRANT INSERT ON human_assertions,media_review_sessions,media_review_chunks,media_review_completions,approval_reviews,audit_events TO brokerrouter_media_review;
GRANT DELETE ON human_assertions TO brokerrouter_media_review;
GRANT UPDATE(expires_at) ON human_assertions TO brokerrouter_media_review;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_media_review;
DROP POLICY IF EXISTS brokerrouter_review_output_read ON assets;
CREATE POLICY brokerrouter_review_output_read ON assets FOR SELECT TO brokerrouter_media_review,brokerrouter_approvals USING(kind='output');
DROP POLICY IF EXISTS brokerrouter_review_chunk_read ON asset_chunks;
CREATE POLICY brokerrouter_review_chunk_read ON asset_chunks FOR SELECT TO brokerrouter_media_review
    USING(EXISTS(SELECT 1 FROM assets a WHERE a.tenant_id=asset_chunks.tenant_id AND a.id=asset_chunks.asset_id AND a.kind='output'));
-- Decision service sees scan/receipt metadata for fresh revalidation, never asset chunks.
GRANT SELECT ON media_review_completions,media_review_sessions,media_output_deliveries,output_scans,output_scan_profiles,assets TO brokerrouter_approvals;
GRANT SELECT(model_id) ON requests TO brokerrouter_approvals;

-- MCP configuration and network execution remain separate identities. The
-- catalog role can change connection metadata but cannot invoke servers; the
-- client role can discover active servers but cannot alter their identity.
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_mcp_catalog') THEN
        CREATE ROLE brokerrouter_mcp_catalog NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_mcp_client') THEN
        CREATE ROLE brokerrouter_mcp_client NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_mcp_server') THEN
        CREATE ROLE brokerrouter_mcp_server NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='brokerrouter_mcp_worker') THEN
        CREATE ROLE brokerrouter_mcp_worker NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
    END IF;
END $$;
GRANT USAGE ON SCHEMA public TO brokerrouter_mcp_catalog,brokerrouter_mcp_client,brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT SELECT ON _sqlx_migrations TO brokerrouter_mcp_catalog,brokerrouter_mcp_client,brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT SELECT(id,active) ON tenants TO brokerrouter_mcp_catalog,brokerrouter_mcp_client,brokerrouter_mcp_server,brokerrouter_mcp_worker;
-- Harmless lock column enables SELECT ... FOR SHARE so tenant revocation is
-- serialized with a catalog/discovery commit without granting state changes.
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_mcp_catalog,brokerrouter_mcp_client;
GRANT SELECT,INSERT ON mcp_catalog_mutations TO brokerrouter_mcp_catalog,brokerrouter_mcp_client;
GRANT SELECT,INSERT ON mcp_discovery_snapshots TO brokerrouter_mcp_client;
GRANT SELECT,INSERT ON mcp_discovered_tools TO brokerrouter_mcp_client;
GRANT SELECT,INSERT ON mcp_discovered_skills TO brokerrouter_mcp_client;
GRANT SELECT,INSERT ON mcp_skill_get_observations TO brokerrouter_mcp_client;
GRANT SELECT,INSERT ON mcp_servers TO brokerrouter_mcp_catalog;
GRANT UPDATE(display_name,transport,endpoint_url,stdio_profile,allow_private,auth_mode,
    credential_ref_kind,credential_ref_ciphertext,oauth_issuer,oauth_resource,oauth_audience,active)
    ON mcp_servers TO brokerrouter_mcp_catalog;
GRANT SELECT ON mcp_servers TO brokerrouter_mcp_client;
GRANT SELECT ON mcp_discovery_snapshots,mcp_discovered_tools,mcp_discovered_skills,mcp_skill_get_observations TO brokerrouter_mcp_catalog;
GRANT SELECT(tenant_id,id,revoked_at,expires_at) ON virtual_keys TO brokerrouter_mcp_catalog;
GRANT SELECT,INSERT ON mcp_key_tool_grants TO brokerrouter_mcp_catalog;
GRANT UPDATE(server_config_revision,descriptor_hash,active) ON mcp_key_tool_grants TO brokerrouter_mcp_catalog;
GRANT SELECT,INSERT ON mcp_oauth_principals TO brokerrouter_mcp_catalog;
GRANT UPDATE(key_id,active) ON mcp_oauth_principals TO brokerrouter_mcp_catalog;

-- The downstream MCP server can authenticate virtual keys and read only
-- current descriptors/grants. It cannot read upstream credential references,
-- mutate grants, invoke connectors, or access governance approvals.
GRANT SELECT(tenant_id,id,key_hash,revoked_at,expires_at) ON virtual_keys TO brokerrouter_mcp_server;
GRANT SELECT(tenant_id,id,active,config_revision) ON mcp_servers TO brokerrouter_mcp_server;
GRANT SELECT ON mcp_discovery_snapshots,mcp_discovered_tools,mcp_key_tool_grants TO brokerrouter_mcp_server;
GRANT SELECT ON mcp_oauth_principals TO brokerrouter_mcp_server;
-- MCP edge and worker run the same policy kernel but have separate DB identities.
-- Neither role may decide approvals or update grants. Only the worker can read
-- upstream secret references; only the edge can enqueue a client call.
GRANT UPDATE(governance_lock) ON tenants TO brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT SELECT ON governance_users,runtime_bindings,governance_turns,guardrails,
    guardrail_versions,guardrail_policies,governance_operations,approvals,
    guardrail_grants,guardrail_evaluations,guardrail_observations
    TO brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT UPDATE(event_clock) ON governance_turns TO brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT INSERT ON governance_operations,approvals,governance_events,guardrail_evaluations,guardrail_observations
    TO brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT UPDATE(state,claim_hash,execution_deadline,receipt_hash,receipt_ciphertext,payload_ciphertext)
    ON governance_operations TO brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT UPDATE(state,reason_code,deadline,finished_at) ON guardrail_evaluations
    TO brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT UPDATE(approval_skipped_at) ON guardrail_observations TO brokerrouter_mcp_server,brokerrouter_mcp_worker;
-- Runtime can request consent but cannot approve it; the reviewer cannot create requests.
GRANT SELECT ON mcp_skill_approval_requests TO brokerrouter_runtime,brokerrouter_approvals;
GRANT INSERT(tenant_id,id,turn_id,key_id,user_id,server_id,observation_id,
    server_config_revision,uri,approval_fingerprint,manifest_hash,
    authorization_context_hash,expires_at) ON mcp_skill_approval_requests TO brokerrouter_runtime;
GRANT SELECT ON mcp_skill_approval_reviews TO brokerrouter_approvals;
GRANT INSERT ON mcp_skill_approval_reviews TO brokerrouter_approvals;
GRANT UPDATE(state,decision_hash,decided_by,feedback_ciphertext,decided_at)
    ON mcp_skill_approval_requests TO brokerrouter_approvals;
GRANT SELECT(tenant_id,id,active,config_revision,transport) ON mcp_servers TO brokerrouter_runtime,brokerrouter_approvals;
GRANT SELECT ON mcp_skill_get_observations TO brokerrouter_runtime,brokerrouter_approvals;
GRANT SELECT,INSERT ON mcp_skill_resource_reads TO brokerrouter_runtime;
GRANT SELECT ON mcp_skill_resource_reads,mcp_skill_get_observations TO brokerrouter_mcp_worker;
GRANT SELECT(tenant_id,id,turn_id,key_id,user_id,server_id,server_config_revision,uri,
    approval_fingerprint,manifest_hash,authorization_context_hash,state,expires_at)
    ON mcp_skill_approval_requests TO brokerrouter_mcp_worker;
GRANT UPDATE(state,claim_id,lease_until,content_sha256,content_ciphertext,mime_type,content_kind,resource_valid_until,failure_code,completed_at)
    ON mcp_skill_resource_reads TO brokerrouter_mcp_worker;
GRANT SELECT,INSERT ON mcp_tool_calls TO brokerrouter_mcp_server;
GRANT UPDATE(state,output_operation_id,completed_at) ON mcp_tool_calls TO brokerrouter_mcp_server;
GRANT SELECT ON mcp_tool_calls TO brokerrouter_mcp_worker;
GRANT UPDATE(state,claim_hash,execution_deadline,response_kind,response_hash,response_ciphertext,
    failure_code,completed_at) ON mcp_tool_calls TO brokerrouter_mcp_worker;
GRANT SELECT(tenant_id,id,revoked_at,expires_at) ON virtual_keys TO brokerrouter_mcp_worker;
GRANT SELECT ON mcp_servers,mcp_discovery_snapshots,mcp_discovered_tools,mcp_key_tool_grants
    TO brokerrouter_mcp_worker;
GRANT INSERT ON audit_events TO brokerrouter_mcp_catalog,brokerrouter_mcp_client,brokerrouter_mcp_server,brokerrouter_mcp_worker;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO brokerrouter_mcp_catalog,brokerrouter_mcp_client,brokerrouter_mcp_server,brokerrouter_mcp_worker;
COMMIT;
