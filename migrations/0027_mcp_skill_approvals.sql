-- A Skill decision is a turn-scoped human fact, not an activation or tool grant.
-- The complete observed manifest is immutable in mcp_skill_get_observations.
CREATE TABLE mcp_skill_approval_requests (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    turn_id uuid NOT NULL,
    key_id uuid NOT NULL,
    user_id uuid NOT NULL,
    server_id uuid NOT NULL,
    observation_id uuid NOT NULL,
    server_config_revision bigint NOT NULL CHECK(server_config_revision > 0),
    uri text NOT NULL CHECK(length(uri) BETWEEN 1 AND 4096),
    approval_fingerprint text NOT NULL CHECK(approval_fingerprint ~ '^[0-9a-f]{64}$'),
    manifest_hash bytea NOT NULL CHECK(octet_length(manifest_hash)=32),
    authorization_context_hash bytea,
    state text NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','approved','rejected','feedback')),
    decision_hash bytea CHECK(decision_hash IS NULL OR octet_length(decision_hash)=32),
    decided_by uuid,
    feedback_ciphertext bytea,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    decided_at timestamptz,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id),
    FOREIGN KEY(tenant_id,observation_id) REFERENCES mcp_skill_get_observations(tenant_id,id),
    FOREIGN KEY(tenant_id,decided_by) REFERENCES governance_users(tenant_id,id),
    CHECK(authorization_context_hash IS NULL OR octet_length(authorization_context_hash)=32),
    CHECK(expires_at>created_at),
    CHECK((state='pending' AND decision_hash IS NULL AND decided_by IS NULL AND decided_at IS NULL AND feedback_ciphertext IS NULL)
       OR (state<>'pending' AND decision_hash IS NOT NULL AND decided_by IS NOT NULL AND decided_at IS NOT NULL
           AND ((state='feedback') = (feedback_ciphertext IS NOT NULL))))
);
CREATE INDEX mcp_skill_approval_inbox ON mcp_skill_approval_requests(tenant_id,user_id,created_at DESC,id DESC);
CREATE INDEX mcp_skill_approval_turn_quota ON mcp_skill_approval_requests(tenant_id,turn_id);

CREATE FUNCTION protect_mcp_skill_approval_request() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id OR NEW.id IS DISTINCT FROM OLD.id
       OR NEW.turn_id IS DISTINCT FROM OLD.turn_id OR NEW.key_id IS DISTINCT FROM OLD.key_id
       OR NEW.user_id IS DISTINCT FROM OLD.user_id OR NEW.server_id IS DISTINCT FROM OLD.server_id
       OR NEW.observation_id IS DISTINCT FROM OLD.observation_id
       OR NEW.server_config_revision IS DISTINCT FROM OLD.server_config_revision
       OR NEW.uri IS DISTINCT FROM OLD.uri
       OR NEW.approval_fingerprint IS DISTINCT FROM OLD.approval_fingerprint
       OR NEW.manifest_hash IS DISTINCT FROM OLD.manifest_hash
       OR NEW.authorization_context_hash IS DISTINCT FROM OLD.authorization_context_hash
       OR NEW.created_at IS DISTINCT FROM OLD.created_at OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
       OR OLD.state <> 'pending' OR NEW.state NOT IN ('approved','rejected','feedback') THEN
        RAISE EXCEPTION 'Skill request binding and terminal decision are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER mcp_skill_approval_transition BEFORE UPDATE ON mcp_skill_approval_requests
FOR EACH ROW EXECUTE FUNCTION protect_mcp_skill_approval_request();
CREATE TRIGGER mcp_skill_approval_no_delete BEFORE DELETE ON mcp_skill_approval_requests
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

CREATE TABLE mcp_skill_approval_reviews (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    request_id uuid NOT NULL,
    user_id uuid NOT NULL,
    manifest_hash bytea NOT NULL CHECK(octet_length(manifest_hash)=32),
    approval_fingerprint text NOT NULL CHECK(approval_fingerprint ~ '^[0-9a-f]{64}$'),
    authorization_context_hash bytea,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,request_id) REFERENCES mcp_skill_approval_requests(tenant_id,id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id),
    CHECK(authorization_context_hash IS NULL OR octet_length(authorization_context_hash)=32),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '5 minutes')
);
CREATE INDEX mcp_skill_approval_review_history ON mcp_skill_approval_reviews(tenant_id,request_id,created_at DESC);
CREATE TRIGGER mcp_skill_approval_review_immutable BEFORE UPDATE OR DELETE ON mcp_skill_approval_reviews
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
