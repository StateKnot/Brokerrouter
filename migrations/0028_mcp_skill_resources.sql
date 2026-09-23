-- An on-demand resource read is bound to one human-approved turn and one exact
-- Skill manifest. Ciphertext is written once by the isolated MCP worker.
CREATE TABLE mcp_skill_resource_reads (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    request_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    key_id uuid NOT NULL,
    server_id uuid NOT NULL,
    server_config_revision bigint NOT NULL CHECK(server_config_revision > 0),
    skill_uri text NOT NULL CHECK(length(skill_uri) BETWEEN 1 AND 4096),
    resource_uri text NOT NULL CHECK(length(resource_uri) BETWEEN 1 AND 4096),
    expected_size bigint NOT NULL CHECK(expected_size BETWEEN 0 AND 16777216),
    manifest_hash bytea NOT NULL CHECK(octet_length(manifest_hash)=32),
    authorization_context_hash bytea,
    state text NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','reading','ready','failed','expired')),
    claim_id uuid,
    lease_until timestamptz,
    content_sha256 bytea CHECK(content_sha256 IS NULL OR octet_length(content_sha256)=32),
    content_ciphertext bytea CHECK(content_ciphertext IS NULL OR octet_length(content_ciphertext)<=16777344),
    mime_type text CHECK(mime_type IS NULL OR length(mime_type)<=255),
    content_kind text CHECK(content_kind IS NULL OR content_kind IN ('text','blob')),
    resource_valid_until timestamptz,
    failure_code text CHECK(failure_code IS NULL OR length(failure_code)<=64),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    completed_at timestamptz,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,request_id) REFERENCES mcp_skill_approval_requests(tenant_id,id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id),
    CHECK(authorization_context_hash IS NULL OR octet_length(authorization_context_hash)=32),
    CHECK(expires_at>created_at),
    CHECK((state='ready' AND content_sha256 IS NOT NULL AND content_ciphertext IS NOT NULL AND content_kind IS NOT NULL AND resource_valid_until IS NOT NULL AND failure_code IS NULL)
       OR (state<>'ready' AND content_sha256 IS NULL AND content_ciphertext IS NULL AND content_kind IS NULL)),
    CHECK((state='reading' AND claim_id IS NOT NULL AND lease_until IS NOT NULL)
       OR (state<>'reading' AND claim_id IS NULL AND lease_until IS NULL))
);
CREATE INDEX mcp_skill_resource_read_queue ON mcp_skill_resource_reads(state,created_at,id) WHERE state='queued';
CREATE INDEX mcp_skill_resource_read_expiry ON mcp_skill_resource_reads(expires_at,id) WHERE state IN ('queued','reading','ready');
CREATE INDEX mcp_skill_resource_read_turn ON mcp_skill_resource_reads(tenant_id,turn_id,created_at DESC);

CREATE FUNCTION protect_mcp_skill_resource_read() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id OR NEW.id IS DISTINCT FROM OLD.id
       OR NEW.request_id IS DISTINCT FROM OLD.request_id OR NEW.turn_id IS DISTINCT FROM OLD.turn_id
       OR NEW.key_id IS DISTINCT FROM OLD.key_id OR NEW.server_id IS DISTINCT FROM OLD.server_id
       OR NEW.server_config_revision IS DISTINCT FROM OLD.server_config_revision
       OR NEW.skill_uri IS DISTINCT FROM OLD.skill_uri OR NEW.resource_uri IS DISTINCT FROM OLD.resource_uri
       OR NEW.expected_size IS DISTINCT FROM OLD.expected_size
       OR NEW.manifest_hash IS DISTINCT FROM OLD.manifest_hash
       OR NEW.authorization_context_hash IS DISTINCT FROM OLD.authorization_context_hash
       OR NEW.created_at IS DISTINCT FROM OLD.created_at OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       OR OLD.state NOT IN ('queued','reading','ready')
       OR (OLD.state='queued' AND NEW.state NOT IN ('reading','failed'))
       OR (OLD.state='reading' AND NEW.state NOT IN ('ready','failed'))
       OR (OLD.state='ready' AND (NEW.state<>'expired' OR NEW.content_ciphertext IS NOT NULL
           OR NEW.resource_valid_until IS DISTINCT FROM OLD.resource_valid_until))
       OR (OLD.content_ciphertext IS NOT NULL AND NEW.content_ciphertext IS NOT NULL) THEN
        RAISE EXCEPTION 'Skill resource read binding or verified content is immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER mcp_skill_resource_read_transition BEFORE UPDATE ON mcp_skill_resource_reads
FOR EACH ROW EXECUTE FUNCTION protect_mcp_skill_resource_read();
CREATE TRIGGER mcp_skill_resource_read_no_delete BEFORE DELETE ON mcp_skill_resource_reads
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
