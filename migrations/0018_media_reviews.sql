-- Append-only metadata. No source URLs, media bytes, cookies or approval decisions.
CREATE TABLE media_review_sessions (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    approval_id uuid NOT NULL,
    user_id uuid NOT NULL,
    payload_hash bytea NOT NULL CHECK(octet_length(payload_hash)=32),
    policy_hash bytea NOT NULL CHECK(octet_length(policy_hash)=32),
    sha256 bytea NOT NULL CHECK(octet_length(sha256)=32),
    byte_length bigint NOT NULL CHECK(byte_length BETWEEN 1 AND 67108864),
    chunk_count integer NOT NULL CHECK(chunk_count BETWEEN 1 AND 16),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,approval_id) REFERENCES approvals(tenant_id,id),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '5 minutes'),
    CHECK(chunk_count=(byte_length+4194303)/4194304)
);
CREATE INDEX media_review_owner ON media_review_sessions(tenant_id,user_id,expires_at);
CREATE TABLE media_review_chunks (
    tenant_id uuid NOT NULL,
    session_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK(ordinal BETWEEN 0 AND 15),
    byte_length integer NOT NULL CHECK(byte_length BETWEEN 1 AND 4194304),
    sha256 bytea NOT NULL CHECK(octet_length(sha256)=32),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(tenant_id,session_id,ordinal),
    FOREIGN KEY(tenant_id,session_id) REFERENCES media_review_sessions(tenant_id,id)
);
CREATE TABLE media_review_completions (
    tenant_id uuid NOT NULL,
    review_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(tenant_id,review_id),
    FOREIGN KEY(tenant_id,review_id) REFERENCES media_review_sessions(tenant_id,id),
    FOREIGN KEY(tenant_id,review_id) REFERENCES approval_reviews(tenant_id,id)
);
CREATE TRIGGER media_review_session_immutable BEFORE UPDATE OR DELETE ON media_review_sessions FOR EACH ROW EXECUTE FUNCTION protect_approval_review();
CREATE TRIGGER media_review_session_no_truncate BEFORE TRUNCATE ON media_review_sessions FOR EACH STATEMENT EXECUTE FUNCTION protect_approval_review();
CREATE TRIGGER media_review_chunk_immutable BEFORE UPDATE OR DELETE ON media_review_chunks FOR EACH ROW EXECUTE FUNCTION protect_approval_review();
CREATE TRIGGER media_review_chunk_no_truncate BEFORE TRUNCATE ON media_review_chunks FOR EACH STATEMENT EXECUTE FUNCTION protect_approval_review();
CREATE TRIGGER media_review_completion_immutable BEFORE UPDATE OR DELETE ON media_review_completions FOR EACH ROW EXECUTE FUNCTION protect_approval_review();
CREATE TRIGGER media_review_completion_no_truncate BEFORE TRUNCATE ON media_review_completions FOR EACH STATEMENT EXECUTE FUNCTION protect_approval_review();
