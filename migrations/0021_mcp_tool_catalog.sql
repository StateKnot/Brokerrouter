-- Immutable tool observations are owned by the connector. A virtual key sees a
-- tool only after an operator grants the exact observed descriptor digest.
CREATE TABLE mcp_discovered_tools (
    tenant_id uuid NOT NULL,
    discovery_snapshot_id uuid NOT NULL,
    server_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK(ordinal BETWEEN 0 AND 1023),
    upstream_name text NOT NULL CHECK(length(upstream_name) BETWEEN 1 AND 128),
    downstream_name text NOT NULL CHECK(downstream_name ~ '^[A-Za-z0-9_.-]{1,128}$'),
    descriptor jsonb NOT NULL CHECK(jsonb_typeof(descriptor)='object'),
    descriptor_hash bytea NOT NULL CHECK(octet_length(descriptor_hash)=32),
    cache_scope text NOT NULL CHECK(cache_scope IN ('private','public')),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,discovery_snapshot_id,downstream_name),
    UNIQUE(tenant_id,discovery_snapshot_id,upstream_name),
    FOREIGN KEY(tenant_id,discovery_snapshot_id)
        REFERENCES mcp_discovery_snapshots(tenant_id,id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id)
);
CREATE INDEX mcp_discovered_tools_current
    ON mcp_discovered_tools(tenant_id,server_id,downstream_name,expires_at);
CREATE TRIGGER mcp_discovered_tools_immutable BEFORE UPDATE OR DELETE ON mcp_discovered_tools
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

CREATE TABLE mcp_key_tool_grants (
    tenant_id uuid NOT NULL,
    key_id uuid NOT NULL,
    server_id uuid NOT NULL,
    server_config_revision bigint NOT NULL CHECK(server_config_revision>0),
    upstream_name text NOT NULL CHECK(length(upstream_name) BETWEEN 1 AND 128),
    descriptor_hash bytea NOT NULL CHECK(octet_length(descriptor_hash)=32),
    active boolean NOT NULL,
    revision bigint NOT NULL DEFAULT 1 CHECK(revision>0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,key_id,server_id,upstream_name),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id)
);
CREATE FUNCTION advance_mcp_tool_grant_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.tenant_id<>OLD.tenant_id OR NEW.key_id<>OLD.key_id
       OR NEW.server_id<>OLD.server_id OR NEW.upstream_name<>OLD.upstream_name
       OR NEW.created_at<>OLD.created_at THEN
        RAISE EXCEPTION 'immutable MCP tool grant identity';
    END IF;
    NEW.revision := OLD.revision + 1;
    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;
CREATE TRIGGER mcp_key_tool_grant_revision BEFORE UPDATE ON mcp_key_tool_grants
FOR EACH ROW EXECUTE FUNCTION advance_mcp_tool_grant_revision();

ALTER TABLE mcp_catalog_mutations DROP CONSTRAINT mcp_catalog_mutations_kind_check;
ALTER TABLE mcp_catalog_mutations ADD CONSTRAINT mcp_catalog_mutations_kind_check
    CHECK(kind IN ('server_register','server_update','discovery_record','tool_grant_set'));
