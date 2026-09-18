-- MCP server identities are assigned by Brokerrouter and never derived from
-- self-reported serverInfo. Credential references are encrypted in the app.
CREATE TABLE mcp_servers (
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    id uuid NOT NULL,
    display_name text NOT NULL CHECK(length(display_name) BETWEEN 1 AND 200),
    transport text NOT NULL CHECK(transport IN ('streamable_http','stdio')),
    endpoint_url text CHECK(endpoint_url IS NULL OR (length(endpoint_url) BETWEEN 9 AND 2048 AND endpoint_url ~ '^https://')),
    stdio_profile text CHECK(stdio_profile IS NULL OR length(stdio_profile) BETWEEN 1 AND 200),
    allow_private boolean NOT NULL DEFAULT false,
    auth_mode text NOT NULL CHECK(auth_mode IN ('none','bearer','oauth_client_credentials')),
    credential_ref_kind text CHECK(credential_ref_kind IN ('environment','secret_manager')),
    credential_ref_ciphertext bytea CHECK(credential_ref_ciphertext IS NULL OR octet_length(credential_ref_ciphertext) BETWEEN 30 AND 4096),
    oauth_issuer text CHECK(oauth_issuer IS NULL OR (length(oauth_issuer) BETWEEN 9 AND 2048 AND oauth_issuer ~ '^https://')),
    oauth_resource text CHECK(oauth_resource IS NULL OR (length(oauth_resource) BETWEEN 9 AND 2048 AND oauth_resource ~ '^https://')),
    oauth_audience text CHECK(oauth_audience IS NULL OR length(oauth_audience) BETWEEN 1 AND 2048),
    active boolean NOT NULL DEFAULT false,
    config_revision bigint NOT NULL DEFAULT 1 CHECK(config_revision > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,id),
    CHECK((transport='streamable_http' AND endpoint_url IS NOT NULL AND stdio_profile IS NULL)
       OR (transport='stdio' AND endpoint_url IS NULL AND stdio_profile IS NOT NULL AND NOT allow_private)),
    CHECK((auth_mode='none' AND credential_ref_kind IS NULL AND credential_ref_ciphertext IS NULL
            AND oauth_issuer IS NULL AND oauth_resource IS NULL AND oauth_audience IS NULL)
       OR (auth_mode='bearer' AND credential_ref_kind IS NOT NULL AND credential_ref_ciphertext IS NOT NULL
            AND oauth_issuer IS NULL AND oauth_resource IS NULL AND oauth_audience IS NULL)
       OR (auth_mode='oauth_client_credentials' AND credential_ref_kind IS NOT NULL AND credential_ref_ciphertext IS NOT NULL
            AND oauth_issuer IS NOT NULL AND oauth_resource IS NOT NULL AND oauth_audience IS NOT NULL))
);
CREATE UNIQUE INDEX mcp_servers_display_name ON mcp_servers(tenant_id,lower(display_name));
CREATE FUNCTION advance_mcp_server_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.tenant_id<>OLD.tenant_id OR NEW.id<>OLD.id OR NEW.created_at<>OLD.created_at THEN
        RAISE EXCEPTION 'immutable MCP server identity';
    END IF;
    NEW.config_revision := OLD.config_revision + 1;
    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;
CREATE TRIGGER mcp_server_revision BEFORE UPDATE ON mcp_servers
FOR EACH ROW EXECUTE FUNCTION advance_mcp_server_revision();

-- Append-only discovery observations. They remain bound to the exact server
-- configuration revision that was used for the network request.
CREATE TABLE mcp_discovery_snapshots (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    server_id uuid NOT NULL,
    server_config_revision bigint NOT NULL CHECK(server_config_revision > 0),
    protocol_version text NOT NULL CHECK(protocol_version='2026-07-28'),
    result_type text NOT NULL CHECK(result_type='complete'),
    supported_versions jsonb NOT NULL CHECK(jsonb_typeof(supported_versions)='array'),
    capabilities jsonb NOT NULL CHECK(jsonb_typeof(capabilities)='object'),
    server_info jsonb CHECK(server_info IS NULL OR jsonb_typeof(server_info)='object'),
    instructions text CHECK(instructions IS NULL OR length(instructions)<=16384),
    capability_hash bytea NOT NULL CHECK(octet_length(capability_hash)=32),
    ttl_ms integer NOT NULL CHECK(ttl_ms BETWEEN 0 AND 86400000),
    cache_scope text NOT NULL CHECK(cache_scope IN ('private','public')),
    authorization_context_hash bytea CHECK(authorization_context_hash IS NULL OR octet_length(authorization_context_hash)=32),
    observed_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id),
    CHECK((cache_scope='public' AND authorization_context_hash IS NULL)
       OR (cache_scope='private' AND authorization_context_hash IS NOT NULL))
);
CREATE INDEX mcp_discovery_current ON mcp_discovery_snapshots
    (tenant_id,server_id,server_config_revision,observed_at DESC);
CREATE TRIGGER mcp_discovery_immutable BEFORE UPDATE OR DELETE ON mcp_discovery_snapshots
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

CREATE TABLE mcp_catalog_mutations (
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    operation_id uuid NOT NULL,
    actor text NOT NULL CHECK(length(actor) BETWEEN 1 AND 200),
    kind text NOT NULL CHECK(kind IN ('server_register','server_update','discovery_record')),
    body_hash bytea NOT NULL CHECK(octet_length(body_hash)=32),
    response jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,operation_id)
);
CREATE TRIGGER mcp_catalog_mutations_immutable BEFORE UPDATE OR DELETE ON mcp_catalog_mutations
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
