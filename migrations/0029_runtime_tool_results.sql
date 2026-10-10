-- Runtime text is an immutable execution fact. Output approval is evaluated at
-- release time and can change without rewriting this captured execution result.
CREATE TABLE runtime_tool_results (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    key_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    input_operation_id uuid NOT NULL,
    model_id text,
    tool_name text NOT NULL CHECK(octet_length(tool_name) BETWEEN 1 AND 200),
    claim_hash bytea NOT NULL CHECK(octet_length(claim_hash)=32),
    receipt_hash bytea NOT NULL CHECK(octet_length(receipt_hash)=32),
    evidence_ref text NOT NULL CHECK(octet_length(evidence_ref) BETWEEN 1 AND 1000),
    text_sha256 bytea NOT NULL CHECK(octet_length(text_sha256)=32),
    bytes integer NOT NULL CHECK(bytes BETWEEN 0 AND 524288),
    result_ciphertext bytea,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,input_operation_id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,input_operation_id) REFERENCES governance_operations(tenant_id,id),
    CHECK(expires_at>=created_at+interval '60 seconds'
        AND expires_at<=created_at+interval '7 days'),
    CHECK(result_ciphertext IS NULL OR octet_length(result_ciphertext) BETWEEN 30 AND 524350)
);
CREATE INDEX runtime_result_expiry ON runtime_tool_results(expires_at,id)
    WHERE result_ciphertext IS NOT NULL;

CREATE FUNCTION bind_runtime_result_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.result_ciphertext IS NULL OR NOT EXISTS (
        SELECT 1 FROM governance_operations o
        WHERE o.tenant_id=NEW.tenant_id AND o.id=NEW.input_operation_id
            AND o.key_id=NEW.key_id AND o.turn_id=NEW.turn_id
            AND o.stage='tool_input' AND o.state='completed'
            AND o.model_id IS NOT DISTINCT FROM NEW.model_id
            AND o.tool_name=NEW.tool_name
            AND o.claim_hash=NEW.claim_hash AND o.receipt_hash=NEW.receipt_hash
    ) THEN
        RAISE EXCEPTION 'runtime result requires its completed claimed tool_input receipt';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runtime_result_receipt_binding BEFORE INSERT ON runtime_tool_results
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_result_receipt();

CREATE FUNCTION protect_runtime_result() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.turn_id,NEW.input_operation_id,
        NEW.model_id,NEW.tool_name,NEW.claim_hash,NEW.receipt_hash,NEW.evidence_ref,
        NEW.text_sha256,NEW.bytes,NEW.expires_at,NEW.created_at)
        IS DISTINCT FROM
       (OLD.tenant_id,OLD.id,OLD.key_id,OLD.turn_id,OLD.input_operation_id,
        OLD.model_id,OLD.tool_name,OLD.claim_hash,OLD.receipt_hash,OLD.evidence_ref,
        OLD.text_sha256,OLD.bytes,OLD.expires_at,OLD.created_at)
       OR (NEW.result_ciphertext IS DISTINCT FROM OLD.result_ciphertext
           AND NOT (NEW.result_ciphertext IS NULL AND OLD.result_ciphertext IS NOT NULL
                    AND OLD.expires_at<=clock_timestamp())) THEN
        RAISE EXCEPTION 'runtime result identity and content are immutable; only expired ciphertext may be cleared';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runtime_result_immutable BEFORE UPDATE ON runtime_tool_results
    FOR EACH ROW EXECUTE FUNCTION protect_runtime_result();
CREATE TRIGGER runtime_result_no_delete BEFORE DELETE ON runtime_tool_results
    FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
