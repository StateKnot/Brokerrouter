ALTER TABLE console_permissions DROP CONSTRAINT console_permissions_permission_check;
ALTER TABLE console_permissions ADD CONSTRAINT console_permissions_permission_check
CHECK(permission IN ('usage_read','audit_read','catalog_read','catalog_write','keys_read','keys_write','fx_read','fx_write','governance_read','governance_write'));

ALTER TABLE guardrails ADD COLUMN config_revision bigint NOT NULL DEFAULT 1 CHECK(config_revision>0);
ALTER TABLE guardrail_policies ADD COLUMN config_revision bigint NOT NULL DEFAULT 1 CHECK(config_revision>0);
CREATE TRIGGER guardrail_config_revision BEFORE UPDATE ON guardrails
FOR EACH ROW EXECUTE FUNCTION advance_config_revision();
CREATE TRIGGER policy_config_revision BEFORE UPDATE ON guardrail_policies
FOR EACH ROW EXECUTE FUNCTION advance_config_revision();

-- A disabled version must not be reopened to resurrect old turn grants.
-- CLI publishers also obey monotonic versions; active-only changes now require publication.
CREATE FUNCTION protect_guardrail_head() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id) IS DISTINCT FROM (OLD.tenant_id,OLD.id)
      OR NEW.version<OLD.version
      OR (NEW.active IS DISTINCT FROM OLD.active AND NEW.version<=OLD.version)
    THEN RAISE EXCEPTION 'guardrail state changes require a new immutable version'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER guardrail_head_monotonic BEFORE UPDATE ON guardrails
FOR EACH ROW EXECUTE FUNCTION protect_guardrail_head();
-- The current pointer must name an actual immutable version at commit, including CLI writes.
ALTER TABLE guardrails ADD CONSTRAINT guardrail_current_version
FOREIGN KEY(tenant_id,id,version) REFERENCES guardrail_versions(tenant_id,id,version)
DEFERRABLE INITIALLY DEFERRED;

CREATE OR REPLACE VIEW console_audit_facts WITH (security_barrier=true) AS
SELECT tenant_id,id,action,object_id,created_at,database_actor::text AS database_actor,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated') AND length(details->>'user_id')<=36 THEN details->>'user_id' END AS actor_user_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated') AND length(details->>'reason')<=500 THEN details->>'reason' END AS change_reason,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated') AND length(details->>'operation_id')<=36 THEN details->>'operation_id' END AS operation_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published','settings.guardrail_published','settings.policy_updated') AND details->>'revision' ~ '^[0-9]{1,19}$' THEN details->>'revision' END AS config_revision
FROM audit_events;
