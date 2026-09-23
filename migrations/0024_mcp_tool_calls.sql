-- Durable MCP tool execution facts. The public MCP edge can enqueue and
-- release results, while the isolated worker is the only role allowed to
-- claim calls and resolve upstream credentials.
CREATE TABLE mcp_tool_calls (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    key_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    input_operation_id uuid NOT NULL,
    output_operation_id uuid,
    server_id uuid NOT NULL,
    server_config_revision bigint NOT NULL CHECK(server_config_revision > 0),
    discovery_snapshot_id uuid NOT NULL,
    upstream_name text NOT NULL CHECK(length(upstream_name) BETWEEN 1 AND 128),
    downstream_name text NOT NULL CHECK(downstream_name ~ '^[A-Za-z0-9_.-]{1,128}$'),
    descriptor_hash bytea NOT NULL CHECK(octet_length(descriptor_hash)=32),
    arguments_hash bytea NOT NULL CHECK(octet_length(arguments_hash)=32),
    state text NOT NULL DEFAULT 'queued' CHECK(state IN (
        'queued','authorizing','executing','execution_unknown','failed',
        'input_required','task','output_pending','completed','expired'
    )),
    claim_hash bytea CHECK(claim_hash IS NULL OR octet_length(claim_hash)=32),
    execution_deadline timestamptz,
    response_kind text CHECK(response_kind IS NULL OR response_kind IN ('complete','input_required','task')),
    response_hash bytea CHECK(response_hash IS NULL OR octet_length(response_hash)=32),
    response_ciphertext bytea,
    failure_code text CHECK(failure_code IS NULL OR length(failure_code) BETWEEN 1 AND 100),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,input_operation_id),
    UNIQUE(tenant_id,output_operation_id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,input_operation_id) REFERENCES governance_operations(tenant_id,id),
    FOREIGN KEY(tenant_id,output_operation_id) REFERENCES governance_operations(tenant_id,id),
    FOREIGN KEY(tenant_id,server_id) REFERENCES mcp_servers(tenant_id,id),
    FOREIGN KEY(tenant_id,discovery_snapshot_id)
        REFERENCES mcp_discovery_snapshots(tenant_id,id),
    CHECK(expires_at > created_at),
    CHECK((state IN ('authorizing','executing') AND execution_deadline IS NOT NULL)
       OR (state NOT IN ('authorizing','executing'))),
    CHECK((response_kind IS NULL AND response_hash IS NULL AND response_ciphertext IS NULL)
       OR (response_kind IS NOT NULL AND response_hash IS NOT NULL AND response_ciphertext IS NOT NULL)),
    CHECK((state IN ('input_required','task','output_pending','completed')) = (response_kind IS NOT NULL)),
    CHECK((state='failed') = (failure_code IS NOT NULL)),
    CHECK((state IN ('completed','failed','execution_unknown','input_required','task','expired')) = (completed_at IS NOT NULL))
);
CREATE INDEX mcp_tool_calls_worker_queue
    ON mcp_tool_calls(state,created_at,id)
    WHERE state IN ('queued','authorizing','executing');
CREATE INDEX mcp_tool_calls_turn
    ON mcp_tool_calls(tenant_id,turn_id,created_at,id);

CREATE FUNCTION enforce_mcp_tool_call_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.tenant_id<>OLD.tenant_id OR NEW.id<>OLD.id OR NEW.key_id<>OLD.key_id
       OR NEW.turn_id<>OLD.turn_id OR NEW.input_operation_id<>OLD.input_operation_id
       OR NEW.server_id<>OLD.server_id OR NEW.server_config_revision<>OLD.server_config_revision
       OR NEW.discovery_snapshot_id<>OLD.discovery_snapshot_id
       OR NEW.upstream_name<>OLD.upstream_name OR NEW.downstream_name<>OLD.downstream_name
       OR NEW.descriptor_hash<>OLD.descriptor_hash OR NEW.arguments_hash<>OLD.arguments_hash
       OR NEW.expires_at<>OLD.expires_at OR NEW.created_at<>OLD.created_at THEN
        RAISE EXCEPTION 'immutable MCP tool call identity';
    END IF;
    IF NEW.state=OLD.state THEN
        RAISE EXCEPTION 'MCP tool call updates require a state transition';
    END IF;
    IF NEW.response_kind IS DISTINCT FROM OLD.response_kind
       OR NEW.response_hash IS DISTINCT FROM OLD.response_hash
       OR NEW.response_ciphertext IS DISTINCT FROM OLD.response_ciphertext THEN
        IF NOT ((OLD.state IN ('executing','execution_unknown')
                  AND NEW.state IN ('input_required','task','output_pending')
                  AND OLD.response_kind IS NULL)
             OR (NEW.state='expired' AND NEW.response_kind IS NULL
                  AND NEW.response_hash IS NULL AND NEW.response_ciphertext IS NULL)) THEN
            RAISE EXCEPTION 'MCP tool response is immutable';
        END IF;
    END IF;
    IF NEW.output_operation_id IS DISTINCT FROM OLD.output_operation_id
       AND NOT (OLD.state='output_pending' AND NEW.state='completed'
                AND OLD.output_operation_id IS NULL AND NEW.output_operation_id IS NOT NULL) THEN
        RAISE EXCEPTION 'MCP output approval binding is immutable';
    END IF;
    IF NEW.claim_hash IS DISTINCT FROM OLD.claim_hash
       AND NOT ((OLD.state='queued' AND NEW.state='authorizing' AND NEW.claim_hash IS NOT NULL)
             OR (OLD.state='authorizing' AND NEW.state='queued' AND NEW.claim_hash IS NULL)) THEN
        RAISE EXCEPTION 'MCP dispatch claim is immutable';
    END IF;
    IF NEW.state<>OLD.state AND NOT (
        (OLD.state='queued' AND NEW.state IN ('authorizing','failed')) OR
        (OLD.state='authorizing' AND NEW.state IN ('queued','executing','failed','execution_unknown')) OR
        (OLD.state='executing' AND NEW.state IN ('failed','execution_unknown','input_required','task','output_pending')) OR
        (OLD.state='output_pending' AND NEW.state='completed') OR
        (OLD.state='execution_unknown' AND NEW.state IN ('failed','input_required','task','output_pending')) OR
        (OLD.state IN ('input_required','task','output_pending','completed') AND NEW.state='expired')
    ) THEN
        RAISE EXCEPTION 'invalid MCP tool call state transition % -> %', OLD.state, NEW.state;
    END IF;
    IF NEW.state='expired' AND OLD.expires_at>clock_timestamp() THEN
        RAISE EXCEPTION 'MCP tool response cannot expire early';
    END IF;
    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;
CREATE TRIGGER mcp_tool_call_transition BEFORE UPDATE ON mcp_tool_calls
FOR EACH ROW EXECUTE FUNCTION enforce_mcp_tool_call_transition();
