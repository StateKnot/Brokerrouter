-- Metadata-only receipts: sensitive operation content stays in its existing encrypted store.
CREATE TABLE approval_reviews (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    approval_id uuid NOT NULL,
    user_id uuid NOT NULL,
    payload_hash bytea NOT NULL CHECK(octet_length(payload_hash)=32),
    policy_hash bytea NOT NULL CHECK(octet_length(policy_hash)=32),
    can_continue boolean NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,approval_id) REFERENCES approvals(tenant_id,id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '5 minutes')
);
CREATE FUNCTION protect_approval_review() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'approval review receipts are append-only'; END; $$;
CREATE TRIGGER approval_review_immutable BEFORE UPDATE OR DELETE ON approval_reviews
FOR EACH ROW EXECUTE FUNCTION protect_approval_review();
CREATE TRIGGER approval_review_no_truncate BEFORE TRUNCATE ON approval_reviews
FOR EACH STATEMENT EXECUTE FUNCTION protect_approval_review();
CREATE INDEX approval_review_history ON approval_reviews(tenant_id,approval_id,created_at,id);
CREATE INDEX approval_inbox_owner ON governance_turns(tenant_id,user_id,id);

CREATE OR REPLACE VIEW console_audit_facts WITH (security_barrier=true) AS
SELECT tenant_id,id,action,object_id,created_at,database_actor::text AS database_actor,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated','approval.reviewed','approval.decided') AND length(details->>'user_id')<=36 THEN details->>'user_id' END AS actor_user_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated') AND length(details->>'reason')<=500 THEN details->>'reason' END AS change_reason,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated','approval.reviewed','approval.decided') AND length(details->>'operation_id')<=36 THEN details->>'operation_id' END AS operation_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated') AND details->>'revision' ~ '^[0-9]{1,19}$' THEN details->>'revision' END AS config_revision
FROM audit_events;
