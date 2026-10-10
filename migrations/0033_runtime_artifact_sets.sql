-- Original text and whole native file set share
-- immutable identity; current scan/review/policy remains B2 authority.
ALTER TABLE runtime_tool_results ADD COLUMN artifact_set jsonb;
ALTER TABLE runtime_tool_results ADD COLUMN artifact_set_hash bytea;
ALTER TABLE runtime_final_publications ADD COLUMN artifact_set jsonb;
ALTER TABLE runtime_final_publications ADD COLUMN artifact_set_hash bytea;

-- Fixed flat objects only: nine-field sets and reconstructed four-field native
-- manifests. Builders emit UUID, lowercase hex, integer and fixed UTC strings;
-- this is not a recursive/general JSON canonicalizer.
CREATE FUNCTION runtime_artifact_set_digest(files jsonb) RETURNS bytea
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT sha256(convert_to('[' || COALESCE(string_agg('{' || item.body || '}', ',' ORDER BY item.n), '') || ']', 'UTF8'))
    FROM (
        SELECT e.n, (SELECT string_agg(to_json(k.key)::text || ':' || k.value::text,
                    ',' ORDER BY k.key COLLATE "C") FROM jsonb_each(e.file) k) AS body
        FROM jsonb_array_elements(files) WITH ORDINALITY e(file,n)
    ) item
$$;

ALTER TABLE runtime_tool_results DROP CONSTRAINT runtime_tool_results_check;
ALTER TABLE runtime_tool_results ADD CONSTRAINT runtime_tool_result_source_ttl CHECK(
    expires_at<=created_at+interval '7 days' AND
    ((artifact_set IS NULL AND expires_at>=created_at+interval '60 seconds')
     OR (artifact_set IS NOT NULL AND expires_at>created_at))
);
ALTER TABLE runtime_tool_results ADD CONSTRAINT runtime_tool_result_set CHECK(
    (artifact_set IS NULL AND artifact_set_hash IS NULL) OR
    (artifact_set IS NOT NULL AND artifact_set_hash IS NOT NULL
     AND jsonb_typeof(artifact_set)='array' AND jsonb_array_length(artifact_set) BETWEEN 1 AND 8
     AND octet_length(artifact_set_hash)=32 AND artifact_set_hash=runtime_artifact_set_digest(artifact_set))
);
ALTER TABLE runtime_final_publications ADD CONSTRAINT runtime_final_publication_set CHECK(
    (artifact_set IS NULL AND artifact_set_hash IS NULL) OR
    (artifact_set IS NOT NULL AND artifact_set_hash IS NOT NULL
     AND jsonb_typeof(artifact_set)='array' AND jsonb_array_length(artifact_set) BETWEEN 1 AND 64
     AND octet_length(artifact_set_hash)=32 AND artifact_set_hash=runtime_artifact_set_digest(artifact_set))
);

-- Metadata view deliberately keeps reserved, expired and broken batches. A
-- filtered join must not turn an incomplete/unauthorized source into no source.
-- It exposes no ciphertext, object path, storage credential or chunk row.
CREATE VIEW runtime_artifact_source_facts WITH (security_barrier=true) AS
SELECT b.tenant_id,b.key_id,b.turn_id,b.input_operation_id,b.id AS batch_id,
    b.model_id,b.tool_name,b.state,files.files,
    COALESCE((o.id IS NOT NULL AND t.id IS NOT NULL AND t.key_id=b.key_id
     AND o.key_id=b.key_id AND o.turn_id=b.turn_id
     AND o.stage='tool_input' AND o.state='completed'
     AND o.model_id IS NOT DISTINCT FROM b.model_id AND o.tool_name=b.tool_name
     AND o.claim_hash=b.claim_hash AND o.receipt_hash=b.receipt_hash
     AND o.payload_hash=b.input_payload_hash AND b.manifest_hash IS NOT NULL
     AND b.receipt_hash IS NOT NULL AND b.evidence_ref IS NOT NULL),false) AS source_matches,
    (files.n BETWEEN 1 AND b.maximum_artifacts AND files.n<=8
     AND files.first_ordinal=0 AND files.last_ordinal=files.n-1
     AND files.total_bytes BETWEEN 1 AND b.maximum_bytes AND files.total_bytes<=67108864
     AND files.identities_match AND files.manifest_hash=b.manifest_hash) AS files_match,
    files.complete,
    (b.state='quarantined' AND b.expires_at>clock_timestamp()
     AND t.state='active' AND t.expires_at>clock_timestamp()
     AND files.available AND files.first_expiry>clock_timestamp()) AS available,
    LEAST(b.expires_at,files.first_expiry,t.expires_at) AS source_deadline,
    result.id AS result_id,
    COALESCE((result.id IS NOT NULL AND result.key_id=b.key_id AND result.turn_id=b.turn_id
     AND result.model_id IS NOT DISTINCT FROM b.model_id AND result.tool_name=b.tool_name
     AND result.claim_hash=b.claim_hash AND result.receipt_hash=b.receipt_hash
     AND result.evidence_ref=b.evidence_ref
     AND result.artifact_set=files.files
     AND result.artifact_set_hash=runtime_artifact_set_digest(files.files)
     AND result.expires_at<=LEAST(b.expires_at,files.first_expiry,t.expires_at)),false) AS result_matches,
    COALESCE((result.result_ciphertext IS NOT NULL AND result.expires_at>clock_timestamp()),false) AS result_available,
    result.expires_at AS result_deadline
FROM runtime_artifact_batches b
LEFT JOIN governance_operations o ON o.tenant_id=b.tenant_id AND o.id=b.input_operation_id
LEFT JOIN governance_turns t ON t.tenant_id=b.tenant_id AND t.id=b.turn_id
LEFT JOIN runtime_tool_results result ON result.tenant_id=b.tenant_id AND result.input_operation_id=b.input_operation_id
LEFT JOIN LATERAL (
    SELECT count(*) AS n,min(f.ordinal) AS first_ordinal,max(f.ordinal) AS last_ordinal,
        COALESCE(sum(f.byte_length),0) AS total_bytes,min(a.expires_at) AS first_expiry,
        COALESCE(bool_and(COALESCE(a.id IS NOT NULL AND a.key_id=f.key_id AND f.key_id=b.key_id
            AND f.source_operation_id=b.input_operation_id AND a.source_operation_id=b.input_operation_id
            AND a.kind='runtime_output' AND a.source_job_id IS NULL
            AND a.sha256=f.sha256 AND a.byte_length=f.byte_length
            AND a.declared_media_type=f.declared_media_type AND f.declared_media_type='video/mp4'
            AND a.chunk_count=(f.byte_length+4194303)/4194304
            AND a.expires_at=b.expires_at AND a.upload_expires_at=b.upload_expires_at,false)),false) AS identities_match,
        COALESCE(bool_and(a.id IS NOT NULL AND a.completed_at IS NOT NULL),false) AS complete,
        COALESCE(bool_and(a.id IS NOT NULL AND a.state='available' AND a.expires_at>clock_timestamp()),false) AS available,
        runtime_artifact_set_digest(COALESCE(jsonb_agg(jsonb_build_object(
            'sha256',encode(f.sha256,'hex'),'byte_length',f.byte_length,
            'media_type',f.declared_media_type,'producer_revision',encode(f.producer_revision,'hex')
        ) ORDER BY f.ordinal),'[]'::jsonb)) AS manifest_hash,
        COALESCE(jsonb_agg(jsonb_build_object(
            'asset_id',f.asset_id::text,'batch_id',b.id::text,'input_operation_id',b.input_operation_id::text,
            'ordinal',f.ordinal,'sha256',encode(f.sha256,'hex'),'byte_length',f.byte_length,
            'producer_revision',encode(f.producer_revision,'hex'),'manifest_sha256',encode(b.manifest_hash,'hex'),
            'expires_at',to_char(a.expires_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"')
        ) ORDER BY f.ordinal),'[]'::jsonb) AS files
    FROM runtime_artifacts f LEFT JOIN assets a ON a.tenant_id=f.tenant_id AND a.id=f.asset_id
    WHERE f.tenant_id=b.tenant_id AND f.batch_id=b.id
) files ON true;

-- SECURITY INVOKER functions operate on the metadata view. They never elevate
-- runtime purpose to storage/human rights, and never filter on model or state.
CREATE FUNCTION runtime_artifact_operation_files(scope_tenant uuid,scope_key uuid,scope_input uuid)
RETURNS jsonb LANGUAGE sql STABLE AS $$
    SELECT files FROM runtime_artifact_source_facts
    WHERE tenant_id=scope_tenant AND input_operation_id=scope_input
$$;
CREATE FUNCTION runtime_artifact_turn_files(scope_tenant uuid,scope_key uuid,scope_turn uuid)
RETURNS jsonb LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN EXISTS(SELECT 1 FROM runtime_artifact_source_facts
        WHERE tenant_id=scope_tenant AND turn_id=scope_turn)
        THEN COALESCE((SELECT jsonb_agg(f.file ORDER BY s.input_operation_id,f.n)
            FROM runtime_artifact_source_facts s CROSS JOIN LATERAL jsonb_array_elements(s.files) WITH ORDINALITY f(file,n)
            WHERE s.tenant_id=scope_tenant AND s.turn_id=scope_turn),'[]'::jsonb)
        ELSE NULL END
$$;

CREATE FUNCTION bind_runtime_result_artifact_set() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE expected jsonb; fixed_body text;
BEGIN
    -- Also serialize direct purpose SQL and stale repeatable-read writers. An
    -- actual tuple UPDATE makes a stale snapshot fail rather than omit a source.
    UPDATE tenants SET governance_lock=governance_lock WHERE id=NEW.tenant_id;
    IF EXISTS(SELECT 1 FROM runtime_final_publications p WHERE p.tenant_id=NEW.tenant_id
        AND p.turn_id=NEW.turn_id) THEN
        RAISE EXCEPTION 'a final publication freezes its original turn output set';
    END IF;
    expected := runtime_artifact_operation_files(NEW.tenant_id,NEW.key_id,NEW.input_operation_id);
    IF NEW.artifact_set IS DISTINCT FROM expected OR
        NEW.artifact_set_hash IS DISTINCT FROM runtime_artifact_set_digest(expected) THEN
        RAISE EXCEPTION 'tool result must bind its entire original artifact batch';
    END IF;
    IF NEW.tool_name='brokerrouter_fixed_mp4' THEN
        IF expected IS NULL THEN
            RAISE EXCEPTION 'fixed producer requires its complete original artifact set';
        END IF;
        fixed_body := '{"artifact_count":' || jsonb_array_length(expected)::text ||
            ',"contract":"brokerrouter-fixed-mp4-result-v1"}';
        IF NEW.text_sha256<>sha256(convert_to(fixed_body,'UTF8')) OR NEW.bytes<>octet_length(fixed_body) THEN
            RAISE EXCEPTION 'fixed producer body must match its complete original artifact set';
        END IF;
    END IF;
    IF expected IS NOT NULL AND NOT EXISTS(SELECT 1 FROM runtime_artifact_source_facts s
        WHERE s.tenant_id=NEW.tenant_id AND s.key_id=NEW.key_id AND s.input_operation_id=NEW.input_operation_id
          AND s.turn_id=NEW.turn_id AND s.model_id IS NOT DISTINCT FROM NEW.model_id AND s.tool_name=NEW.tool_name
          AND s.source_matches AND s.files_match AND s.complete AND s.available
          AND s.source_deadline>=NEW.expires_at) THEN
        RAISE EXCEPTION 'tool result requires sealed live original artifact facts and fixed source deadline';
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER runtime_result_artifact_binding BEFORE INSERT ON runtime_tool_results
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_result_artifact_set();

CREATE FUNCTION bind_runtime_publication_artifact_set() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE expected jsonb;
BEGIN
    UPDATE tenants SET governance_lock=governance_lock WHERE id=NEW.tenant_id;
    expected := runtime_artifact_turn_files(NEW.tenant_id,NEW.key_id,NEW.turn_id);
    IF NEW.artifact_set IS DISTINCT FROM expected OR
        NEW.artifact_set_hash IS DISTINCT FROM runtime_artifact_set_digest(expected) THEN
        RAISE EXCEPTION 'final publication must bind the entire original turn artifact set';
    END IF;
    IF expected IS NOT NULL AND EXISTS(SELECT 1 FROM runtime_artifact_source_facts s
        WHERE s.tenant_id=NEW.tenant_id AND s.turn_id=NEW.turn_id
          AND (s.key_id<>NEW.key_id OR s.model_id IS DISTINCT FROM NEW.model_id OR NOT s.source_matches OR NOT s.files_match
            OR NOT s.complete OR NOT s.available OR NOT COALESCE(s.result_matches,false)
            OR NOT COALESCE(s.result_available,false)
            OR LEAST(s.source_deadline,s.result_deadline)<NEW.expires_at)) THEN
        RAISE EXCEPTION 'final publication requires every original batch and immutable tool result';
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER runtime_publication_artifact_binding BEFORE INSERT ON runtime_final_publications
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_publication_artifact_set();

-- Storage purpose needs only these freeze identities, not runtime body SELECT.
CREATE VIEW runtime_output_set_freezes WITH (security_barrier=true) AS
SELECT tenant_id,key_id,turn_id,input_operation_id FROM runtime_tool_results
UNION ALL
SELECT tenant_id,key_id,turn_id,NULL::uuid AS input_operation_id FROM runtime_final_publications;

CREATE FUNCTION prevent_late_runtime_artifact_batch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE tenants SET governance_lock=governance_lock WHERE id=NEW.tenant_id;
    IF EXISTS(SELECT 1 FROM runtime_output_set_freezes f WHERE f.tenant_id=NEW.tenant_id
        AND (f.input_operation_id=NEW.input_operation_id OR
            (f.input_operation_id IS NULL AND f.turn_id=NEW.turn_id))) THEN
        RAISE EXCEPTION 'original result and final turn output sets cannot acquire later batches';
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER runtime_artifact_batch_frozen_set BEFORE INSERT ON runtime_artifact_batches
    FOR EACH ROW EXECUTE FUNCTION prevent_late_runtime_artifact_batch();

CREATE OR REPLACE FUNCTION protect_runtime_result() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.turn_id,NEW.input_operation_id,
        NEW.model_id,NEW.tool_name,NEW.claim_hash,NEW.receipt_hash,NEW.evidence_ref,
        NEW.text_sha256,NEW.bytes,NEW.expires_at,NEW.created_at,NEW.artifact_set,NEW.artifact_set_hash)
       IS DISTINCT FROM
       (OLD.tenant_id,OLD.id,OLD.key_id,OLD.turn_id,OLD.input_operation_id,
        OLD.model_id,OLD.tool_name,OLD.claim_hash,OLD.receipt_hash,OLD.evidence_ref,
        OLD.text_sha256,OLD.bytes,OLD.expires_at,OLD.created_at,OLD.artifact_set,OLD.artifact_set_hash)
       OR (NEW.result_ciphertext IS DISTINCT FROM OLD.result_ciphertext AND NOT (
           NEW.result_ciphertext IS NULL AND OLD.result_ciphertext IS NOT NULL
           AND OLD.expires_at<=clock_timestamp())) THEN
        RAISE EXCEPTION 'runtime result and its entire artifact set are immutable';
    END IF;
    RETURN NEW;
END; $$;
CREATE OR REPLACE FUNCTION protect_runtime_publication() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.turn_id,NEW.model_id,NEW.source_requests,
        NEW.text_sha256,NEW.bytes,NEW.created_at,NEW.expires_at,NEW.artifact_set,NEW.artifact_set_hash)
       IS DISTINCT FROM
       (OLD.tenant_id,OLD.id,OLD.key_id,OLD.turn_id,OLD.model_id,OLD.source_requests,
        OLD.text_sha256,OLD.bytes,OLD.created_at,OLD.expires_at,OLD.artifact_set,OLD.artifact_set_hash)
       OR (NEW.result_ciphertext IS DISTINCT FROM OLD.result_ciphertext AND NOT (
           NEW.result_ciphertext IS NULL AND OLD.result_ciphertext IS NOT NULL
           AND OLD.expires_at<=clock_timestamp())) THEN
        RAISE EXCEPTION 'final publication and its entire artifact set are immutable';
    END IF;
    RETURN NEW;
END; $$;

CREATE VIEW runtime_tool_result_review_facts WITH (security_barrier=true) AS
SELECT r.tenant_id,r.id,r.key_id,r.turn_id,r.input_operation_id,r.model_id,r.tool_name,
    r.claim_hash,r.receipt_hash,r.evidence_ref,r.text_sha256,r.bytes,r.expires_at,r.artifact_set,r.artifact_set_hash,
    (r.result_ciphertext IS NOT NULL AND r.expires_at>clock_timestamp()
     AND EXISTS(SELECT 1 FROM governance_operations o WHERE o.tenant_id=r.tenant_id AND o.id=r.input_operation_id
        AND o.key_id=r.key_id AND o.turn_id=r.turn_id AND o.stage='tool_input' AND o.state='completed'
        AND o.model_id IS NOT DISTINCT FROM r.model_id AND o.tool_name=r.tool_name
        AND o.claim_hash=r.claim_hash AND o.receipt_hash=r.receipt_hash)
     AND r.artifact_set IS NOT DISTINCT FROM runtime_artifact_operation_files(r.tenant_id,r.key_id,r.input_operation_id)
     AND r.artifact_set_hash IS NOT DISTINCT FROM runtime_artifact_set_digest(runtime_artifact_operation_files(r.tenant_id,r.key_id,r.input_operation_id))
     AND NOT EXISTS(SELECT 1 FROM runtime_artifact_source_facts s
        WHERE s.tenant_id=r.tenant_id AND s.input_operation_id=r.input_operation_id
          AND (s.key_id<>r.key_id OR NOT s.source_matches OR NOT s.files_match OR NOT s.complete OR NOT s.available
            OR NOT COALESCE(s.result_matches,false) OR NOT COALESCE(s.result_available,false)))) AS available
FROM runtime_tool_results r;

-- Preserve existing view column order; append only the two immutable set facts.
CREATE OR REPLACE VIEW runtime_final_publication_review_facts WITH (security_barrier=true) AS
SELECT p.tenant_id,p.id,p.key_id,p.turn_id,p.model_id,p.source_requests,p.text_sha256,p.bytes,p.expires_at,
    (p.result_ciphertext IS NOT NULL AND p.expires_at>clock_timestamp()
     AND NOT EXISTS(SELECT 1 FROM jsonb_array_elements(p.source_requests) s
        WHERE NOT EXISTS(SELECT 1 FROM requests r WHERE r.tenant_id=p.tenant_id AND r.key_id=p.key_id
            AND r.id::text=s->>'id' AND r.governance_turn_id=p.turn_id AND r.model_id=p.model_id
            AND r.status='succeeded' AND r.guardrail_evaluation_id IS NULL
            AND r.routing_snapshot->0->'endpoint'->>'operation'='chat'
            AND r.result_ciphertext IS NOT NULL AND r.result_expires_at>clock_timestamp()
            AND r.result_expires_at>=p.expires_at AND r.result_expires_at=(s->>'expires_at')::timestamptz
            AND encode(r.request_hash,'hex')=s->>'request_hash'
            AND encode(sha256(r.result_ciphertext),'hex')=s->>'ciphertext_sha256'))
     AND p.artifact_set IS NOT DISTINCT FROM runtime_artifact_turn_files(p.tenant_id,p.key_id,p.turn_id)
     AND p.artifact_set_hash IS NOT DISTINCT FROM runtime_artifact_set_digest(runtime_artifact_turn_files(p.tenant_id,p.key_id,p.turn_id))
     AND NOT EXISTS(SELECT 1 FROM runtime_artifact_source_facts s
        WHERE s.tenant_id=p.tenant_id AND s.turn_id=p.turn_id
          AND (s.key_id<>p.key_id OR s.model_id IS DISTINCT FROM p.model_id OR NOT s.source_matches OR NOT s.files_match
            OR NOT s.complete OR NOT s.available OR NOT COALESCE(s.result_matches,false)
            OR NOT COALESCE(s.result_available,false)
            OR LEAST(s.source_deadline,s.result_deadline)<p.expires_at))) AS available,
    p.artifact_set,p.artifact_set_hash
FROM runtime_final_publications p;

-- New artifacts/text publication use invoker-only source builders. Deployment
-- permissions must grant metadata SELECT/EXECUTE to exactly their purposes.
REVOKE ALL ON FUNCTION runtime_artifact_set_digest(jsonb) FROM PUBLIC;
REVOKE ALL ON FUNCTION runtime_artifact_operation_files(uuid,uuid,uuid) FROM PUBLIC;
REVOKE ALL ON FUNCTION runtime_artifact_turn_files(uuid,uuid,uuid) FROM PUBLIC;

CREATE VIEW runtime_artifact_asset_facts WITH (security_barrier=true) AS
SELECT tenant_id,id,key_id,kind,state,source_job_id,source_operation_id,
    sha256,byte_length,chunk_count,expires_at
FROM assets WHERE kind='runtime_output';
