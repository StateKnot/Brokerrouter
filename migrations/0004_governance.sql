ALTER TABLE tenants ADD COLUMN governance_lock bigint NOT NULL DEFAULT 0;
CREATE TABLE governance_users (
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    id uuid NOT NULL,
    issuer text NOT NULL CHECK(length(issuer) BETWEEN 1 AND 500),
    subject text NOT NULL CHECK(length(subject) BETWEEN 1 AND 500),
    active boolean NOT NULL DEFAULT true,
    PRIMARY KEY(tenant_id,id), UNIQUE(tenant_id,issuer,subject)
);
CREATE TABLE runtime_bindings (
    tenant_id uuid NOT NULL,
    key_id uuid NOT NULL,
    user_id uuid NOT NULL,
    runtime_id text NOT NULL CHECK(length(runtime_id) BETWEEN 1 AND 200),
    agent_id text NOT NULL CHECK(length(agent_id) BETWEEN 1 AND 200),
    active boolean NOT NULL DEFAULT true,
    PRIMARY KEY(tenant_id,key_id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id)
);
CREATE TABLE governance_turns (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    key_id uuid NOT NULL,
    user_id uuid NOT NULL,
    session_id text NOT NULL CHECK(length(session_id) BETWEEN 1 AND 200),
    message_id text NOT NULL CHECK(length(message_id) BETWEEN 1 AND 200),
    run_id text NOT NULL CHECK(length(run_id) BETWEEN 1 AND 200),
    generation integer NOT NULL CHECK(generation >= 0),
    state text NOT NULL DEFAULT 'active' CHECK(state IN ('active','completed','cancelled','failed')),
    expires_at timestamptz NOT NULL,
    event_clock bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,id), UNIQUE(tenant_id,key_id,session_id,message_id,generation),
    FOREIGN KEY(tenant_id,key_id) REFERENCES runtime_bindings(tenant_id,key_id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '1 hour')
);
CREATE TABLE guardrails (
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    id text NOT NULL CHECK(length(id) BETWEEN 1 AND 100),
    version integer NOT NULL CHECK(version>0),
    active boolean NOT NULL DEFAULT true,
    PRIMARY KEY(tenant_id,id)
);
CREATE TABLE guardrail_versions (
    tenant_id uuid NOT NULL,
    id text NOT NULL,
    version integer NOT NULL CHECK(version>0),
    name text NOT NULL CHECK(length(name) BETWEEN 1 AND 200),
    stage text NOT NULL CHECK(stage IN ('pre','post','tool_input','tool_output','artifact')),
    rule jsonb NOT NULL,
    PRIMARY KEY(tenant_id,id,version),
    FOREIGN KEY(tenant_id,id) REFERENCES guardrails(tenant_id,id)
);
CREATE TRIGGER guardrail_version_immutable BEFORE UPDATE OR DELETE ON guardrail_versions
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TABLE guardrail_policies (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    guardrail_id text NOT NULL,
    model_id text,
    key_id uuid,
    runtime_id text,
    tool_name text,
    active boolean NOT NULL DEFAULT true,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,guardrail_id) REFERENCES guardrails(tenant_id,id),
    FOREIGN KEY(tenant_id,model_id) REFERENCES models(tenant_id,id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id)
);
CREATE TABLE governance_operations (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    turn_id uuid NOT NULL,
    key_id uuid NOT NULL,
    operation_key text NOT NULL CHECK(length(operation_key) BETWEEN 1 AND 200),
    stage text NOT NULL CHECK(stage IN ('pre','post','tool_input','tool_output','artifact')),
    model_id text,
    tool_name text,
    payload_hash bytea NOT NULL CHECK(octet_length(payload_hash)=32),
    payload_ciphertext bytea,
    state text NOT NULL DEFAULT 'ready' CHECK(state IN ('ready','waiting_approval','rejected','feedback','executing','execution_unknown','completed')),
    claim_hash bytea CHECK(octet_length(claim_hash)=32),
    execution_deadline timestamptz,
    receipt_hash bytea,
    receipt_ciphertext bytea,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,id), UNIQUE(tenant_id,key_id,operation_key,stage),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id)
);
CREATE TABLE approvals (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    operation_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    policy_hash bytea NOT NULL CHECK(octet_length(policy_hash)=32),
    policy_snapshot jsonb NOT NULL,
    summary jsonb NOT NULL,
    guardrails jsonb NOT NULL,
    state text NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','approved','rejected','feedback')),
    decision_hash bytea,
    decided_by uuid,
    feedback_ciphertext bytea,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    decided_at timestamptz,
    PRIMARY KEY(tenant_id,id), UNIQUE(tenant_id,operation_id,policy_hash),
    FOREIGN KEY(tenant_id,operation_id) REFERENCES governance_operations(tenant_id,id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,decided_by) REFERENCES governance_users(tenant_id,id),
    CHECK((state='pending' AND decision_hash IS NULL AND decided_by IS NULL AND decided_at IS NULL) OR (state<>'pending' AND decision_hash IS NOT NULL AND decided_by IS NOT NULL AND decided_at IS NOT NULL))
);
CREATE TABLE guardrail_grants (
    tenant_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    guardrail_id text NOT NULL,
    version integer NOT NULL,
    approval_id uuid NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,turn_id,guardrail_id,version),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,guardrail_id,version) REFERENCES guardrail_versions(tenant_id,id,version),
    FOREIGN KEY(tenant_id,approval_id) REFERENCES approvals(tenant_id,id)
);
CREATE TRIGGER grant_immutable BEFORE UPDATE OR DELETE ON guardrail_grants
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TABLE governance_events (
    tenant_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    sequence bigint NOT NULL,
    event_type text NOT NULL,
    data jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,turn_id,sequence),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id)
);
CREATE TRIGGER governance_event_immutable BEFORE UPDATE OR DELETE ON governance_events
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TABLE governance_event_acks (
    tenant_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    sequence bigint NOT NULL CHECK(sequence>=0),
    PRIMARY KEY(tenant_id,turn_id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id)
);
CREATE TABLE human_assertions (
    issuer text NOT NULL,
    jti text NOT NULL CHECK(length(jti) BETWEEN 1 AND 200),
    request_hash bytea NOT NULL CHECK(octet_length(request_hash)=32),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(issuer,jti)
);
ALTER TABLE requests ADD COLUMN governance_turn_id uuid;
ALTER TABLE requests ADD FOREIGN KEY(tenant_id,governance_turn_id) REFERENCES governance_turns(tenant_id,id);
CREATE FUNCTION protect_request_turn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.governance_turn_id IS DISTINCT FROM OLD.governance_turn_id THEN
        RAISE EXCEPTION 'request governance context is immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER request_turn_immutable BEFORE UPDATE ON requests FOR EACH ROW EXECUTE FUNCTION protect_request_turn();
CREATE INDEX operation_expiry ON governance_operations(execution_deadline) WHERE state='executing';
CREATE INDEX approval_turn ON approvals(tenant_id,turn_id,created_at,id);
CREATE INDEX human_assertion_expiry ON human_assertions(expires_at);
CREATE UNIQUE INDEX one_active_turn_per_session ON governance_turns(tenant_id,key_id,session_id) WHERE state='active';
CREATE FUNCTION protect_turn_lifecycle() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.user_id,NEW.session_id,NEW.message_id,NEW.run_id,NEW.generation,NEW.expires_at,NEW.created_at)
        IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.key_id,OLD.user_id,OLD.session_id,OLD.message_id,OLD.run_id,OLD.generation,OLD.expires_at,OLD.created_at)
        OR (OLD.state<>'active' AND NEW.state<>OLD.state) THEN
        RAISE EXCEPTION 'turn identity, expiry and terminal state are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER turn_lifecycle_immutable BEFORE UPDATE ON governance_turns FOR EACH ROW EXECUTE FUNCTION protect_turn_lifecycle();
