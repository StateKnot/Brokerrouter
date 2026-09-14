ALTER TABLE tenants ADD COLUMN asset_quota_bytes bigint NOT NULL DEFAULT 0 CHECK(asset_quota_bytes BETWEEN 0 AND 10995116277760);
ALTER TABLE tenants ADD COLUMN asset_used_bytes bigint NOT NULL DEFAULT 0 CHECK(asset_used_bytes>=0 AND asset_used_bytes<=asset_quota_bytes);
ALTER TABLE virtual_keys ADD COLUMN asset_quota_bytes bigint NOT NULL DEFAULT 0 CHECK(asset_quota_bytes BETWEEN 0 AND 10995116277760);
ALTER TABLE virtual_keys ADD COLUMN asset_used_bytes bigint NOT NULL DEFAULT 0 CHECK(asset_used_bytes>=0 AND asset_used_bytes<=asset_quota_bytes);

CREATE TABLE assets (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    key_id uuid NOT NULL,
    idempotency_key text NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 200),
    request_hash bytea NOT NULL CHECK(octet_length(request_hash)=32),
    sha256 bytea NOT NULL CHECK(octet_length(sha256)=32),
    byte_length bigint NOT NULL CHECK(byte_length BETWEEN 1 AND 536870912),
    declared_media_type text NOT NULL,
    chunk_count integer NOT NULL CHECK(chunk_count BETWEEN 1 AND 128),
    state text NOT NULL DEFAULT 'uploading' CHECK(state IN ('uploading','available','deleted')),
    created_at timestamptz NOT NULL DEFAULT statement_timestamp(),
    upload_expires_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    completed_at timestamptz,
    deleted_at timestamptz,
    gc_after timestamptz,
    gc_checked_at timestamptz,
    gc_cursor integer NOT NULL DEFAULT 0 CHECK(gc_cursor BETWEEN 0 AND 128),
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,key_id,idempotency_key),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '30 days'),
    CHECK(upload_expires_at>created_at AND upload_expires_at<=expires_at),
    CHECK((state='uploading' AND completed_at IS NULL AND deleted_at IS NULL)
       OR (state='available' AND completed_at IS NOT NULL AND deleted_at IS NULL)
       OR (state='deleted' AND deleted_at IS NOT NULL AND gc_after IS NOT NULL))
);
CREATE TABLE asset_chunks (
    tenant_id uuid NOT NULL,
    asset_id uuid NOT NULL,
    id uuid NOT NULL,
    ordinal integer NOT NULL CHECK(ordinal BETWEEN 0 AND 127),
    byte_length integer NOT NULL CHECK(byte_length BETWEEN 1 AND 4194304),
    sha256 bytea NOT NULL CHECK(octet_length(sha256)=32),
    state text NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','stored')),
    PRIMARY KEY(tenant_id,asset_id,ordinal),
    UNIQUE(tenant_id,id),
    FOREIGN KEY(tenant_id,asset_id) REFERENCES assets(tenant_id,id)
);
CREATE INDEX asset_expiration ON assets(expires_at) WHERE state<>'deleted';
CREATE INDEX asset_upload_expiration ON assets(upload_expires_at) WHERE state='uploading';
CREATE INDEX asset_gc ON assets(gc_after) WHERE state='deleted';

CREATE FUNCTION protect_asset() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.idempotency_key,NEW.request_hash,NEW.sha256,NEW.byte_length,
        NEW.declared_media_type,NEW.chunk_count,NEW.created_at,NEW.upload_expires_at,NEW.expires_at)
      IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.key_id,OLD.idempotency_key,OLD.request_hash,OLD.sha256,OLD.byte_length,
        OLD.declared_media_type,OLD.chunk_count,OLD.created_at,OLD.upload_expires_at,OLD.expires_at)
      OR (OLD.state='deleted' AND NEW.state<>'deleted') OR (OLD.state='available' AND NEW.state='uploading')
      OR (OLD.completed_at IS NOT NULL AND NEW.completed_at IS DISTINCT FROM OLD.completed_at)
      OR (OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS DISTINCT FROM OLD.deleted_at) THEN
        RAISE EXCEPTION 'asset identity, content and terminal state are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER asset_immutable BEFORE UPDATE ON assets FOR EACH ROW EXECUTE FUNCTION protect_asset();
CREATE FUNCTION protect_asset_chunk() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.asset_id,NEW.id,NEW.ordinal,NEW.byte_length,NEW.sha256)
        IS DISTINCT FROM (OLD.tenant_id,OLD.asset_id,OLD.id,OLD.ordinal,OLD.byte_length,OLD.sha256)
        OR (OLD.state='stored' AND NEW.state<>'stored') THEN
        RAISE EXCEPTION 'asset chunk identity and bytes are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER asset_chunk_immutable BEFORE UPDATE ON asset_chunks FOR EACH ROW EXECUTE FUNCTION protect_asset_chunk();
CREATE TRIGGER asset_no_delete BEFORE DELETE ON assets FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER asset_chunk_no_delete BEFORE DELETE ON asset_chunks FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER asset_no_truncate BEFORE TRUNCATE ON assets FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER asset_chunk_no_truncate BEFORE TRUNCATE ON asset_chunks FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
