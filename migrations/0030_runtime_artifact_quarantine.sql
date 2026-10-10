-- Native tools reserve private capacity before dispatch. These facts never
-- create a media job, input asset, scan verdict, approval or delivery grant.
CREATE TABLE runtime_artifact_batches (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    key_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    input_operation_id uuid NOT NULL,
    model_id text,
    tool_name text NOT NULL CHECK(octet_length(tool_name) BETWEEN 1 AND 200),
    claim_hash bytea NOT NULL CHECK(octet_length(claim_hash)=32),
    input_payload_hash bytea NOT NULL CHECK(octet_length(input_payload_hash)=32),
    original_payload_hash bytea NOT NULL CHECK(octet_length(original_payload_hash)=32),
    maximum_bytes bigint NOT NULL CHECK(maximum_bytes BETWEEN 1 AND 67108864),
    maximum_artifacts integer NOT NULL CHECK(maximum_artifacts BETWEEN 1 AND 8),
    reserved_bytes bigint NOT NULL CHECK(reserved_bytes>=0 AND reserved_bytes<=maximum_bytes),
    manifest_hash bytea CHECK(octet_length(manifest_hash)=32),
    receipt_hash bytea CHECK(octet_length(receipt_hash)=32),
    evidence_ref text CHECK(octet_length(evidence_ref) BETWEEN 1 AND 1000),
    state text NOT NULL DEFAULT 'reserved' CHECK(state IN ('reserved','capturing','quarantined','expired')),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    upload_expires_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,input_operation_id),
    UNIQUE(tenant_id,id,key_id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,turn_id) REFERENCES governance_turns(tenant_id,id),
    FOREIGN KEY(tenant_id,input_operation_id) REFERENCES governance_operations(tenant_id,id),
    CHECK(expires_at>=created_at+interval '60 seconds' AND expires_at<=created_at+interval '7 days'),
    CHECK(upload_expires_at>created_at AND upload_expires_at<=expires_at AND upload_expires_at<=created_at+interval '1 hour'),
    CHECK((manifest_hash IS NULL AND receipt_hash IS NULL AND evidence_ref IS NULL)
        OR (manifest_hash IS NOT NULL AND receipt_hash IS NOT NULL AND evidence_ref IS NOT NULL AND reserved_bytes=0)),
    CHECK(state<>'reserved' OR manifest_hash IS NULL),
    CHECK(state NOT IN ('capturing','quarantined') OR manifest_hash IS NOT NULL),
    CHECK(state<>'expired' OR reserved_bytes=0)
);
CREATE INDEX runtime_artifact_expiry ON runtime_artifact_batches(upload_expires_at,expires_at)
    WHERE state<>'expired';

ALTER TABLE assets DROP CONSTRAINT assets_kind_check;
ALTER TABLE assets ADD CONSTRAINT assets_kind_check CHECK(kind IN ('input','output','runtime_output'));
ALTER TABLE assets ADD COLUMN source_operation_id uuid;
ALTER TABLE assets DROP CONSTRAINT asset_source_identity;
ALTER TABLE assets ADD CONSTRAINT asset_source_identity CHECK(
    (kind='input' AND source_job_id IS NULL AND source_operation_id IS NULL AND sha256 IS NOT NULL)
    OR (kind='output' AND source_job_id IS NOT NULL AND source_operation_id IS NULL AND (state<>'available' OR sha256 IS NOT NULL))
    OR (kind='runtime_output' AND source_job_id IS NULL AND source_operation_id IS NOT NULL AND sha256 IS NOT NULL)
);
ALTER TABLE assets ADD FOREIGN KEY(tenant_id,source_operation_id) REFERENCES governance_operations(tenant_id,id);

CREATE TABLE runtime_artifacts (
    tenant_id uuid NOT NULL,
    batch_id uuid NOT NULL,
    key_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK(ordinal BETWEEN 0 AND 7),
    asset_id uuid NOT NULL,
    source_operation_id uuid NOT NULL,
    sha256 bytea NOT NULL CHECK(octet_length(sha256)=32),
    byte_length bigint NOT NULL CHECK(byte_length BETWEEN 1 AND 67108864),
    declared_media_type text NOT NULL CHECK(declared_media_type='video/mp4'),
    producer_revision bytea NOT NULL CHECK(octet_length(producer_revision)=32),
    PRIMARY KEY(tenant_id,batch_id,ordinal),
    UNIQUE(tenant_id,asset_id),
    FOREIGN KEY(tenant_id,batch_id,key_id) REFERENCES runtime_artifact_batches(tenant_id,id,key_id),
    FOREIGN KEY(tenant_id,asset_id,key_id) REFERENCES assets(tenant_id,id,key_id),
    FOREIGN KEY(tenant_id,source_operation_id) REFERENCES governance_operations(tenant_id,id)
);

CREATE FUNCTION bind_runtime_artifact_batch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM governance_operations o WHERE o.tenant_id=NEW.tenant_id
        AND o.id=NEW.input_operation_id AND o.key_id=NEW.key_id AND o.turn_id=NEW.turn_id
        AND o.model_id IS NOT DISTINCT FROM NEW.model_id AND o.tool_name=NEW.tool_name
        AND o.stage='tool_input' AND o.state='executing' AND o.claim_hash=NEW.claim_hash
        AND o.payload_hash=NEW.input_payload_hash AND o.execution_deadline>clock_timestamp()) THEN
        RAISE EXCEPTION 'runtime artifact reservation requires its live original tool claim';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runtime_artifact_claim_binding BEFORE INSERT ON runtime_artifact_batches
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_artifact_batch();

CREATE FUNCTION protect_runtime_artifact_batch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.turn_id,NEW.input_operation_id,NEW.model_id,NEW.tool_name,
        NEW.claim_hash,NEW.input_payload_hash,NEW.original_payload_hash,NEW.maximum_bytes,NEW.maximum_artifacts,
        NEW.created_at,NEW.upload_expires_at,NEW.expires_at)
      IS DISTINCT FROM
       (OLD.tenant_id,OLD.id,OLD.key_id,OLD.turn_id,OLD.input_operation_id,OLD.model_id,OLD.tool_name,
        OLD.claim_hash,OLD.input_payload_hash,OLD.original_payload_hash,OLD.maximum_bytes,OLD.maximum_artifacts,
        OLD.created_at,OLD.upload_expires_at,OLD.expires_at)
      OR NEW.reserved_bytes>OLD.reserved_bytes
      OR (OLD.manifest_hash IS NULL AND NEW.manifest_hash IS NOT NULL
          AND NOT (OLD.state='reserved' AND NEW.state='capturing'))
      OR (OLD.manifest_hash IS NOT NULL AND (NEW.manifest_hash,NEW.receipt_hash,NEW.evidence_ref)
           IS DISTINCT FROM (OLD.manifest_hash,OLD.receipt_hash,OLD.evidence_ref))
      OR NOT (NEW.state=OLD.state OR (OLD.state='reserved' AND NEW.state IN ('capturing','expired'))
           OR (OLD.state='capturing' AND NEW.state IN ('quarantined','expired'))
           OR (OLD.state='quarantined' AND NEW.state='expired')) THEN
        RAISE EXCEPTION 'runtime artifact source, capacity and terminal facts are immutable';
    END IF;
    IF NEW.manifest_hash IS NOT NULL AND OLD.manifest_hash IS NULL AND NOT EXISTS(
        SELECT 1 FROM governance_operations o WHERE o.tenant_id=NEW.tenant_id AND o.id=NEW.input_operation_id
            AND o.state='completed' AND o.claim_hash=NEW.claim_hash AND o.receipt_hash=NEW.receipt_hash) THEN
        RAISE EXCEPTION 'runtime artifact manifest requires its completed original receipt';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runtime_artifact_batch_immutable BEFORE UPDATE ON runtime_artifact_batches
    FOR EACH ROW EXECUTE FUNCTION protect_runtime_artifact_batch();
CREATE TRIGGER runtime_artifact_batch_no_delete BEFORE DELETE ON runtime_artifact_batches
    FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER runtime_artifact_batch_no_truncate BEFORE TRUNCATE ON runtime_artifact_batches
    FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER runtime_artifact_immutable BEFORE UPDATE OR DELETE ON runtime_artifacts
    FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER runtime_artifact_no_truncate BEFORE TRUNCATE ON runtime_artifacts
    FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();

CREATE FUNCTION bind_runtime_artifact_asset() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM runtime_artifact_batches b JOIN assets a ON a.tenant_id=b.tenant_id
        AND a.id=NEW.asset_id AND a.key_id=b.key_id WHERE b.tenant_id=NEW.tenant_id AND b.id=NEW.batch_id
        AND b.key_id=NEW.key_id AND b.input_operation_id=NEW.source_operation_id
        AND b.state='capturing' AND a.kind='runtime_output' AND a.source_operation_id=b.input_operation_id
        AND a.sha256=NEW.sha256 AND a.byte_length=NEW.byte_length
        AND a.declared_media_type=NEW.declared_media_type AND a.expires_at=b.expires_at
        AND a.upload_expires_at=b.upload_expires_at) THEN
        RAISE EXCEPTION 'runtime artifact requires its immutable original-source asset';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER runtime_artifact_asset_binding BEFORE INSERT ON runtime_artifacts
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_artifact_asset();

CREATE OR REPLACE FUNCTION protect_asset() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.idempotency_key,NEW.request_hash,NEW.byte_length,
        NEW.declared_media_type,NEW.chunk_count,NEW.created_at,NEW.upload_expires_at,NEW.expires_at,NEW.kind,
        NEW.source_job_id,NEW.source_operation_id)
      IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.key_id,OLD.idempotency_key,OLD.request_hash,OLD.byte_length,
        OLD.declared_media_type,OLD.chunk_count,OLD.created_at,OLD.upload_expires_at,OLD.expires_at,OLD.kind,
        OLD.source_job_id,OLD.source_operation_id)
      OR (NEW.sha256 IS DISTINCT FROM OLD.sha256 AND NOT (OLD.kind='output' AND OLD.sha256 IS NULL
          AND NEW.sha256 IS NOT NULL AND OLD.state='uploading' AND NEW.state='available'))
      OR (OLD.state='deleted' AND NEW.state<>'deleted') OR (OLD.state='available' AND NEW.state='uploading')
      OR (OLD.completed_at IS NOT NULL AND NEW.completed_at IS DISTINCT FROM OLD.completed_at)
      OR (OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS DISTINCT FROM OLD.deleted_at) THEN
        RAISE EXCEPTION 'asset identity, content and terminal state are immutable';
    END IF;
    RETURN NEW;
END;
$$;
