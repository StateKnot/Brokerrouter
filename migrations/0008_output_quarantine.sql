-- Output capacity is reserved with paid admission, before any provider POST.
ALTER TABLE media_jobs ADD CONSTRAINT media_key_identity UNIQUE(tenant_id,id,key_id);
ALTER TABLE assets ADD COLUMN kind text NOT NULL DEFAULT 'input' CHECK(kind IN ('input','output'));
ALTER TABLE assets ADD COLUMN source_job_id uuid;
ALTER TABLE assets ALTER COLUMN sha256 DROP NOT NULL;
ALTER TABLE assets ADD CONSTRAINT asset_source_identity CHECK(
    (kind='input' AND source_job_id IS NULL AND sha256 IS NOT NULL)
    OR (kind='output' AND source_job_id IS NOT NULL AND (state<>'available' OR sha256 IS NOT NULL))
);
ALTER TABLE assets ADD FOREIGN KEY(tenant_id,source_job_id,key_id) REFERENCES media_jobs(tenant_id,id,key_id);
ALTER TABLE assets ADD CONSTRAINT asset_key_identity UNIQUE(tenant_id,id,key_id);
CREATE UNIQUE INDEX asset_output_job ON assets(tenant_id,source_job_id) WHERE kind='output';

CREATE OR REPLACE FUNCTION protect_asset() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.idempotency_key,NEW.request_hash,NEW.byte_length,
        NEW.declared_media_type,NEW.chunk_count,NEW.created_at,NEW.upload_expires_at,NEW.expires_at,NEW.kind,NEW.source_job_id)
      IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.key_id,OLD.idempotency_key,OLD.request_hash,OLD.byte_length,
        OLD.declared_media_type,OLD.chunk_count,OLD.created_at,OLD.upload_expires_at,OLD.expires_at,OLD.kind,OLD.source_job_id)
      OR (NEW.sha256 IS DISTINCT FROM OLD.sha256 AND NOT (OLD.kind='output' AND OLD.sha256 IS NULL AND NEW.sha256 IS NOT NULL AND OLD.state='uploading' AND NEW.state='available'))
      OR (OLD.state='deleted' AND NEW.state<>'deleted') OR (OLD.state='available' AND NEW.state='uploading')
      OR (OLD.completed_at IS NOT NULL AND NEW.completed_at IS DISTINCT FROM OLD.completed_at)
      OR (OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS DISTINCT FROM OLD.deleted_at) THEN
        RAISE EXCEPTION 'asset identity, content and terminal state are immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TABLE media_output_imports (
    tenant_id uuid NOT NULL,
    job_id uuid NOT NULL,
    key_id uuid NOT NULL,
    maximum_bytes bigint NOT NULL CHECK(maximum_bytes BETWEEN 1 AND 536870912),
    reserved_bytes bigint NOT NULL CHECK(reserved_bytes>=0 AND reserved_bytes<=maximum_bytes),
    asset_id uuid,
    source_etag text CHECK(length(source_etag) BETWEEN 2 AND 512),
    source_hash bytea CHECK(octet_length(source_hash)=32),
    state text NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','copying','quarantined','failed','cancelled','expired')),
    error_code text,
    lease_token uuid,
    lease_until timestamptz,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL DEFAULT now()+interval '24 hours',
    PRIMARY KEY(tenant_id,job_id),
    FOREIGN KEY(tenant_id,job_id,key_id) REFERENCES media_jobs(tenant_id,id,key_id),
    FOREIGN KEY(tenant_id,asset_id,key_id) REFERENCES assets(tenant_id,id,key_id),
    CHECK((lease_token IS NULL)=(lease_until IS NULL)),
    CHECK((asset_id IS NULL AND source_etag IS NULL AND source_hash IS NULL) OR (asset_id IS NOT NULL AND source_etag IS NOT NULL AND source_hash IS NOT NULL AND reserved_bytes=0)),
    CHECK(state NOT IN ('copying','quarantined') OR asset_id IS NOT NULL),
    CHECK(state NOT IN ('quarantined','failed','cancelled','expired') OR reserved_bytes=0)
);
CREATE INDEX output_import_work ON media_output_imports(next_attempt_at) WHERE state IN ('pending','copying');
CREATE INDEX output_import_expiry ON media_output_imports(expires_at) WHERE state IN ('pending','copying','quarantined');
CREATE FUNCTION protect_output_import() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.job_id,NEW.key_id,NEW.maximum_bytes,NEW.created_at,NEW.expires_at)
      IS DISTINCT FROM (OLD.tenant_id,OLD.job_id,OLD.key_id,OLD.maximum_bytes,OLD.created_at,OLD.expires_at)
      OR (OLD.asset_id IS NOT NULL AND (NEW.asset_id,NEW.source_etag,NEW.source_hash) IS DISTINCT FROM (OLD.asset_id,OLD.source_etag,OLD.source_hash))
      OR NEW.reserved_bytes>OLD.reserved_bytes
      OR (OLD.state IN ('failed','cancelled','expired') AND NEW.state<>OLD.state)
      OR (OLD.state='quarantined' AND NEW.state NOT IN ('quarantined','cancelled','expired'))
      OR (OLD.state='copying' AND NEW.state='pending') THEN
        RAISE EXCEPTION 'output source, admission and terminal facts are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER output_import_immutable BEFORE UPDATE ON media_output_imports FOR EACH ROW EXECUTE FUNCTION protect_output_import();
CREATE TRIGGER output_import_no_delete BEFORE DELETE ON media_output_imports FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER output_import_no_truncate BEFORE TRUNCATE ON media_output_imports FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();

-- Existing input-only binaries must not expose newly generated output rows.
-- Policies are installed by deploy/permissions.sql after the purpose roles exist.
ALTER TABLE assets ENABLE ROW LEVEL SECURITY;
ALTER TABLE asset_chunks ENABLE ROW LEVEL SECURITY;
