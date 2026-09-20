-- An OAuth token never selects a tenant or virtual key by itself. The signed
-- issuer/subject/tenant tuple must be explicitly mapped by an operator to an
-- active virtual key, whose existing model/tool grants remain authoritative.
CREATE TABLE mcp_oauth_principals (
    tenant_id uuid NOT NULL,
    issuer text NOT NULL CHECK(length(issuer) BETWEEN 9 AND 2048),
    subject text NOT NULL CHECK(length(subject) BETWEEN 1 AND 512),
    key_id uuid NOT NULL,
    active boolean NOT NULL DEFAULT true,
    revision bigint NOT NULL DEFAULT 1 CHECK(revision > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,issuer,subject),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id)
);
CREATE INDEX mcp_oauth_principals_lookup
    ON mcp_oauth_principals(issuer,subject,tenant_id) WHERE active;

CREATE FUNCTION advance_mcp_oauth_principal_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.tenant_id<>OLD.tenant_id OR NEW.issuer<>OLD.issuer
       OR NEW.subject<>OLD.subject OR NEW.created_at<>OLD.created_at THEN
        RAISE EXCEPTION 'immutable MCP OAuth principal identity';
    END IF;
    NEW.revision := OLD.revision + 1;
    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;
CREATE TRIGGER mcp_oauth_principal_revision BEFORE UPDATE ON mcp_oauth_principals
FOR EACH ROW EXECUTE FUNCTION advance_mcp_oauth_principal_revision();

ALTER TABLE mcp_catalog_mutations DROP CONSTRAINT mcp_catalog_mutations_kind_check;
ALTER TABLE mcp_catalog_mutations ADD CONSTRAINT mcp_catalog_mutations_kind_check
    CHECK(kind IN ('server_register','server_update','discovery_record','tool_grant_set','oauth_principal_set'));
