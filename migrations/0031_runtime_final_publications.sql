-- Final assistant text is a distinct immutable publication, after native host
-- transformations. Source IDs refer to gateway facts, never provider/local IDs.
CREATE TABLE runtime_final_publications (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    key_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    model_id text NOT NULL,
    source_requests jsonb NOT NULL CHECK(jsonb_typeof(source_requests)='array'
        AND jsonb_array_length(source_requests) BETWEEN 1 AND 64),
    text_sha256 bytea NOT NULL CHECK(octet_length(text_sha256)=32),
    bytes integer NOT NULL CHECK(bytes BETWEEN 0 AND 524288),
    result_ciphertext bytea,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,key_id,turn_id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,model_id) REFERENCES models(tenant_id,id),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '7 days'),
    CHECK(result_ciphertext IS NULL OR octet_length(result_ciphertext) BETWEEN 30 AND 524350)
);
CREATE INDEX runtime_publication_expiry ON runtime_final_publications(expires_at,id)
    WHERE result_ciphertext IS NOT NULL;

CREATE FUNCTION bind_runtime_publication_sources() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE source jsonb; previous text := ''; source_id text;
BEGIN
    IF NEW.result_ciphertext IS NULL OR NOT EXISTS (
        SELECT 1 FROM governance_turns t WHERE t.tenant_id=NEW.tenant_id
            AND t.id=NEW.turn_id AND t.key_id=NEW.key_id
    ) THEN RAISE EXCEPTION 'publication requires its original runtime turn'; END IF;
    FOR source IN SELECT value FROM jsonb_array_elements(NEW.source_requests) LOOP
        source_id := source->>'id';
        IF jsonb_typeof(source)<>'object' OR
            (SELECT count(*) FROM jsonb_object_keys(source))<>4 OR
            source_id IS NULL OR source_id<=previous OR NOT EXISTS (
                SELECT 1 FROM requests r WHERE r.tenant_id=NEW.tenant_id
                    AND r.id=source_id::uuid AND r.key_id=NEW.key_id
                    AND r.governance_turn_id=NEW.turn_id AND r.model_id=NEW.model_id
                    AND r.status='succeeded' AND r.guardrail_evaluation_id IS NULL
                    AND r.routing_snapshot->0->'endpoint'->>'operation'='chat'
                    AND r.result_ciphertext IS NOT NULL
                    AND r.result_expires_at>clock_timestamp()
                    AND r.result_expires_at>=NEW.expires_at
                    AND source->>'request_hash'=encode(r.request_hash,'hex')
                    AND source->>'ciphertext_sha256'=encode(sha256(r.result_ciphertext),'hex')
                    AND (source->>'expires_at')::timestamptz=r.result_expires_at
            ) THEN RAISE EXCEPTION 'publication requires exact successful gateway source facts'; END IF;
        previous := source_id;
    END LOOP;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runtime_publication_source_binding BEFORE INSERT ON runtime_final_publications
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_publication_sources();

CREATE FUNCTION protect_runtime_publication() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.turn_id,NEW.model_id,NEW.source_requests,
        NEW.text_sha256,NEW.bytes,NEW.created_at,NEW.expires_at) IS DISTINCT FROM
       (OLD.tenant_id,OLD.id,OLD.key_id,OLD.turn_id,OLD.model_id,OLD.source_requests,
        OLD.text_sha256,OLD.bytes,OLD.created_at,OLD.expires_at)
       OR (NEW.result_ciphertext IS DISTINCT FROM OLD.result_ciphertext AND NOT (
           NEW.result_ciphertext IS NULL AND OLD.result_ciphertext IS NOT NULL
           AND OLD.expires_at<=clock_timestamp())) THEN
        RAISE EXCEPTION 'final publication facts are immutable; only expired ciphertext may be cleared';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runtime_publication_immutable BEFORE UPDATE ON runtime_final_publications
    FOR EACH ROW EXECUTE FUNCTION protect_runtime_publication();
CREATE TRIGGER runtime_publication_no_delete BEFORE DELETE ON runtime_final_publications
    FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

-- Human review can authenticate its displayed operation against original facts
-- without SELECT access to either publication or provider ciphertext.
CREATE VIEW runtime_final_publication_review_facts WITH (security_barrier=true) AS
SELECT p.tenant_id,p.id,p.key_id,p.turn_id,p.model_id,p.source_requests,
    p.text_sha256,p.bytes,p.expires_at,
    (p.result_ciphertext IS NOT NULL AND p.expires_at>clock_timestamp()
        AND NOT EXISTS (
            SELECT 1 FROM jsonb_array_elements(p.source_requests) s
            WHERE NOT EXISTS (
                SELECT 1 FROM requests r WHERE r.tenant_id=p.tenant_id
                    AND r.key_id=p.key_id AND r.id::text=s->>'id'
                    AND r.governance_turn_id=p.turn_id AND r.model_id=p.model_id
                    AND r.status='succeeded' AND r.guardrail_evaluation_id IS NULL
                    AND r.routing_snapshot->0->'endpoint'->>'operation'='chat'
                    AND r.result_ciphertext IS NOT NULL
                    AND r.result_expires_at>clock_timestamp()
                    AND r.result_expires_at>=p.expires_at
                    AND r.result_expires_at=(s->>'expires_at')::timestamptz
                    AND encode(r.request_hash,'hex')=s->>'request_hash'
                    AND encode(sha256(r.result_ciphertext),'hex')=s->>'ciphertext_sha256'
            )
        )) AS available
FROM runtime_final_publications p;
