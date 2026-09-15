ALTER TABLE console_permissions DROP CONSTRAINT console_permissions_permission_check;
ALTER TABLE console_permissions ADD CONSTRAINT console_permissions_permission_check
CHECK(permission IN ('usage_read','audit_read','catalog_read','catalog_write','keys_read','keys_write','fx_read','fx_write'));

ALTER TABLE virtual_keys ADD COLUMN config_revision bigint NOT NULL DEFAULT 1 CHECK(config_revision>0);
-- Accounting / asset usage updates do not invalidate a configuration form.
CREATE TRIGGER key_config_revision BEFORE UPDATE OF name,budget_micros,expires_at,revoked_at ON virtual_keys
FOR EACH ROW EXECUTE FUNCTION advance_config_revision();
CREATE FUNCTION advance_key_grant_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE virtual_keys SET config_revision=config_revision+1
    WHERE tenant_id=COALESCE(NEW.tenant_id,OLD.tenant_id) AND id=COALESCE(NEW.key_id,OLD.key_id);
    RETURN NULL;
END;
$$;
CREATE TRIGGER key_grant_revision AFTER INSERT OR DELETE ON key_models
FOR EACH ROW EXECUTE FUNCTION advance_key_grant_revision();

CREATE TABLE settings_mutations (
    tenant_id uuid NOT NULL,
    user_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    body_hash bytea NOT NULL CHECK(octet_length(body_hash)=32),
    response jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,user_id,operation_id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id)
);
CREATE TRIGGER settings_mutations_immutable BEFORE UPDATE OR DELETE ON settings_mutations
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

CREATE OR REPLACE VIEW console_audit_facts WITH (security_barrier=true) AS
SELECT tenant_id,id,action,object_id,created_at,database_actor::text AS database_actor,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published') AND length(details->>'user_id')<=36 THEN details->>'user_id' END AS actor_user_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published') AND length(details->>'reason')<=500 THEN details->>'reason' END AS change_reason,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published') AND length(details->>'operation_id')<=36 THEN details->>'operation_id' END AS operation_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated','settings.key_created','settings.key_updated','settings.key_revoked','settings.fx_published') AND details->>'revision' ~ '^[0-9]{1,19}$' THEN details->>'revision' END AS config_revision
FROM audit_events;
