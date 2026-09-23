-- Skill entries are observed metadata, not approval or executable content.
-- A fresh snapshot never mutates an older manifest or silently grants access.
ALTER TABLE mcp_discovery_snapshots
    ADD COLUMN skills_ttl_ms integer CHECK(skills_ttl_ms BETWEEN 0 AND 86400000),
    ADD COLUMN skills_cache_scope text CHECK(skills_cache_scope IN ('private','public')),
    ADD CONSTRAINT mcp_skills_cache_metadata_pair
        CHECK((skills_ttl_ms IS NULL)=(skills_cache_scope IS NULL));

CREATE TABLE mcp_discovered_skills (
    tenant_id uuid NOT NULL,
    discovery_snapshot_id uuid NOT NULL,
    server_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK(ordinal BETWEEN 0 AND 1023),
    uri text NOT NULL CHECK(length(uri) BETWEEN 1 AND 4096),
    name text NOT NULL CHECK(length(name) BETWEEN 1 AND 64),
    entry jsonb NOT NULL CHECK(jsonb_typeof(entry)='object'),
    approval_fingerprint text NOT NULL CHECK(approval_fingerprint ~ '^[0-9a-f]{64}$'),
    requests_tool_grant boolean NOT NULL,
    cache_scope text NOT NULL CHECK(cache_scope IN ('private','public')),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,discovery_snapshot_id,uri),
    UNIQUE(tenant_id,discovery_snapshot_id,ordinal),
    FOREIGN KEY(tenant_id,discovery_snapshot_id)
        REFERENCES mcp_discovery_snapshots(tenant_id,id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id)
);
CREATE INDEX mcp_discovered_skills_current
    ON mcp_discovered_skills(tenant_id,server_id,expires_at);
CREATE TRIGGER mcp_discovered_skills_immutable BEFORE UPDATE OR DELETE ON mcp_discovered_skills
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
