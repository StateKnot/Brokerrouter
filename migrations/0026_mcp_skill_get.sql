-- Direct skills/get observations may be absent from skills/list. They remain
-- immutable metadata; no approval, content, or invocation authority is implied.
CREATE TABLE mcp_skill_get_observations (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    server_id uuid NOT NULL,
    server_config_revision bigint NOT NULL CHECK(server_config_revision > 0),
    operation_id uuid NOT NULL,
    uri text NOT NULL CHECK(length(uri) BETWEEN 1 AND 4096),
    name text NOT NULL CHECK(length(name) BETWEEN 1 AND 64),
    entry jsonb NOT NULL CHECK(jsonb_typeof(entry)='object'),
    approval_fingerprint text NOT NULL CHECK(approval_fingerprint ~ '^[0-9a-f]{64}$'),
    requests_tool_grant boolean NOT NULL,
    capability_hash bytea NOT NULL CHECK(octet_length(capability_hash)=32),
    discovery_ttl_ms integer NOT NULL CHECK(discovery_ttl_ms BETWEEN 0 AND 86400000),
    skill_ttl_ms integer NOT NULL CHECK(skill_ttl_ms BETWEEN 0 AND 86400000),
    cache_scope text NOT NULL CHECK(cache_scope IN ('private','public')),
    authorization_context_hash bytea,
    observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,operation_id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id),
    CHECK((cache_scope='public' AND authorization_context_hash IS NULL)
       OR (cache_scope='private' AND authorization_context_hash IS NOT NULL
           AND octet_length(authorization_context_hash)=32))
);
CREATE INDEX mcp_skill_get_current
    ON mcp_skill_get_observations(tenant_id,server_id,uri,observed_at DESC);
CREATE TRIGGER mcp_skill_get_immutable BEFORE UPDATE OR DELETE ON mcp_skill_get_observations
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

ALTER TABLE mcp_catalog_mutations DROP CONSTRAINT mcp_catalog_mutations_kind_check;
ALTER TABLE mcp_catalog_mutations ADD CONSTRAINT mcp_catalog_mutations_kind_check
    CHECK(kind IN ('server_register','server_update','discovery_record','tool_grant_set','oauth_principal_set','skill_get_record'));
