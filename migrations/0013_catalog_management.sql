-- Revisions include CLI changes, so a browser cannot overwrite newer operator work.
ALTER TABLE models ADD COLUMN config_revision bigint NOT NULL DEFAULT 1 CHECK(config_revision>0);
ALTER TABLE endpoints ADD COLUMN config_revision bigint NOT NULL DEFAULT 1 CHECK(config_revision>0);
CREATE FUNCTION advance_config_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.config_revision := OLD.config_revision + 1;
    RETURN NEW;
END;
$$;
CREATE TRIGGER model_config_revision BEFORE UPDATE ON models
FOR EACH ROW EXECUTE FUNCTION advance_config_revision();
CREATE TRIGGER endpoint_config_revision BEFORE UPDATE ON endpoints
FOR EACH ROW EXECUTE FUNCTION advance_config_revision();

ALTER TABLE console_permissions DROP CONSTRAINT console_permissions_permission_check;
ALTER TABLE console_permissions ADD CONSTRAINT console_permissions_permission_check
CHECK(permission IN ('usage_read','audit_read','catalog_read','catalog_write'));
-- Harmless lock columns permit SELECT FOR SHARE without granting identity changes.
ALTER TABLE governance_users ADD COLUMN catalog_lock boolean NOT NULL DEFAULT false;
ALTER TABLE console_permissions ADD COLUMN catalog_lock boolean NOT NULL DEFAULT false;

CREATE TABLE catalog_mutations (
    tenant_id uuid NOT NULL,
    user_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    body_hash bytea NOT NULL CHECK(octet_length(body_hash)=32),
    response jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,user_id,operation_id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id)
);
CREATE TRIGGER catalog_mutations_immutable BEFORE UPDATE OR DELETE ON catalog_mutations
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

CREATE TABLE catalog_endpoint_connections (
    tenant_id uuid NOT NULL,
    endpoint_id uuid NOT NULL,
    connection_id text NOT NULL CHECK(length(connection_id) BETWEEN 1 AND 100),
    connection_name text NOT NULL CHECK(length(connection_name) BETWEEN 1 AND 200),
    connection_revision text NOT NULL CHECK(connection_revision ~ '^[a-f0-9]{64}$'),
    PRIMARY KEY(tenant_id,endpoint_id),
    FOREIGN KEY(tenant_id,endpoint_id) REFERENCES endpoints(tenant_id,id)
);
CREATE TRIGGER catalog_endpoint_connections_immutable BEFORE UPDATE OR DELETE ON catalog_endpoint_connections
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

-- Only explicitly selected, bounded configuration audit fields cross the report boundary.
CREATE OR REPLACE VIEW console_audit_facts WITH (security_barrier=true) AS
SELECT tenant_id,id,action,object_id,created_at,database_actor::text AS database_actor,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated') AND length(details->>'user_id')<=36 THEN details->>'user_id' END AS actor_user_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated') AND length(details->>'reason')<=500 THEN details->>'reason' END AS change_reason,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated') AND length(details->>'operation_id')<=36 THEN details->>'operation_id' END AS operation_id,
    CASE WHEN action IN ('catalog.model_created','catalog.model_updated','catalog.endpoint_created','catalog.endpoint_updated') AND details->>'revision' ~ '^[0-9]{1,19}$' THEN details->>'revision' END AS config_revision
FROM audit_events;
