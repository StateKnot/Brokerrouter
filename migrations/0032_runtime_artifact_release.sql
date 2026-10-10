-- Source-owned native artifact scans and immutable per-file review/grant facts.
-- Existing media assignment JSON/evidence view/payload hash stay unchanged.
ALTER TABLE output_scans ALTER COLUMN job_id DROP NOT NULL;
ALTER TABLE output_scans ADD COLUMN runtime_asset_id uuid;
ALTER TABLE output_scans ADD CONSTRAINT output_scan_one_source CHECK(
    (job_id IS NOT NULL AND runtime_asset_id IS NULL)
    OR (job_id IS NULL AND runtime_asset_id IS NOT NULL)
);
ALTER TABLE output_scans ADD FOREIGN KEY(tenant_id,runtime_asset_id)
    REFERENCES runtime_artifacts(tenant_id,asset_id);
CREATE UNIQUE INDEX runtime_output_scan_source_profile
    ON output_scans(tenant_id,runtime_asset_id,profile_id) WHERE runtime_asset_id IS NOT NULL;

CREATE OR REPLACE FUNCTION protect_output_scan() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='INSERT' THEN
        IF NEW.runtime_asset_id IS NULL THEN
            -- Preserve the existing media insert contract exactly.
            IF NOT EXISTS(SELECT 1 FROM assets a JOIN requests r ON r.tenant_id=a.tenant_id AND r.id=a.source_job_id
                JOIN output_scan_profiles p ON p.tenant_id=r.tenant_id AND p.model_id=r.model_id
                WHERE a.tenant_id=NEW.tenant_id AND a.id=NEW.asset_id AND a.source_job_id=NEW.job_id AND a.key_id=NEW.key_id
                  AND a.kind='output' AND a.state='available' AND a.sha256=NEW.sha256 AND a.byte_length=NEW.byte_length
                  AND a.expires_at>=NEW.expires_at AND p.id=NEW.profile_id) THEN
                RAISE EXCEPTION 'scan must bind exact output and model profile';
            END IF;
        ELSIF NOT EXISTS(SELECT 1 FROM runtime_artifacts r
            JOIN runtime_artifact_batches b ON b.tenant_id=r.tenant_id AND b.id=r.batch_id
            JOIN assets a ON a.tenant_id=r.tenant_id AND a.id=r.asset_id
            JOIN governance_operations o ON o.tenant_id=b.tenant_id AND o.id=b.input_operation_id
            JOIN output_scan_profiles p ON p.tenant_id=b.tenant_id AND p.model_id=b.model_id
            WHERE r.tenant_id=NEW.tenant_id AND r.asset_id=NEW.runtime_asset_id AND NEW.asset_id=r.asset_id
              AND r.key_id=NEW.key_id AND a.key_id=r.key_id AND a.kind='runtime_output'
              AND a.source_operation_id=b.input_operation_id AND b.state='quarantined'
              AND a.state='available' AND a.sha256=r.sha256 AND a.sha256=NEW.sha256
              AND a.byte_length=r.byte_length AND a.byte_length=NEW.byte_length
              AND a.expires_at>=NEW.expires_at AND b.expires_at>=NEW.expires_at AND p.id=NEW.profile_id
              AND o.state='completed' AND o.claim_hash=b.claim_hash AND o.receipt_hash=b.receipt_hash
              AND o.payload_hash=b.input_payload_hash AND o.key_id=b.key_id AND o.turn_id=b.turn_id
              AND o.model_id IS NOT DISTINCT FROM b.model_id AND o.tool_name=b.tool_name) THEN
            RAISE EXCEPTION 'scan must bind sealed native bytes and their completed original source';
        END IF;
    ELSE
        IF (NEW.tenant_id,NEW.id,NEW.job_id,NEW.runtime_asset_id,NEW.asset_id,NEW.key_id,NEW.profile_id,
            NEW.sha256,NEW.byte_length,NEW.created_at,NEW.expires_at)
          IS DISTINCT FROM
           (OLD.tenant_id,OLD.id,OLD.job_id,OLD.runtime_asset_id,OLD.asset_id,OLD.key_id,OLD.profile_id,
            OLD.sha256,OLD.byte_length,OLD.created_at,OLD.expires_at)
          OR (OLD.state<>'scanning' AND NEW IS DISTINCT FROM OLD)
          OR NEW.attempts<OLD.attempts OR NEW.attempts>OLD.attempts+1 THEN
            RAISE EXCEPTION 'scan identity and receipt are immutable';
        END IF;
    END IF;
    IF NEW.current_attempt IS NOT NULL AND NOT EXISTS(
        SELECT 1 FROM output_scan_attempts a JOIN output_scanner_tokens t ON t.tenant_id=a.tenant_id AND t.id=a.token_id
        WHERE a.tenant_id=NEW.tenant_id AND a.id=NEW.current_attempt AND a.scan_id=NEW.id AND t.profile_id=NEW.profile_id) THEN
        RAISE EXCEPTION 'attempt belongs to another scan or profile';
    END IF;
    RETURN NEW;
END; $$;

-- Immutable binary evidence ownership. A completed report is not a grant.
CREATE TABLE runtime_artifact_deliveries (
    tenant_id uuid NOT NULL,
    scan_id uuid NOT NULL,
    asset_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(tenant_id,scan_id),
    UNIQUE(tenant_id,operation_id),
    FOREIGN KEY(tenant_id,scan_id) REFERENCES output_scans(tenant_id,id),
    FOREIGN KEY(tenant_id,asset_id) REFERENCES runtime_artifacts(tenant_id,asset_id),
    FOREIGN KEY(tenant_id,operation_id) REFERENCES governance_operations(tenant_id,id)
);
CREATE INDEX runtime_artifact_delivery_asset ON runtime_artifact_deliveries(tenant_id,asset_id);
CREATE FUNCTION bind_runtime_artifact_delivery() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM output_scans s
        JOIN runtime_artifacts r ON r.tenant_id=s.tenant_id AND r.asset_id=s.runtime_asset_id
        JOIN runtime_artifact_batches b ON b.tenant_id=r.tenant_id AND b.id=r.batch_id
        JOIN governance_operations o ON o.tenant_id=s.tenant_id AND o.id=NEW.operation_id
        WHERE s.tenant_id=NEW.tenant_id AND s.id=NEW.scan_id AND r.asset_id=NEW.asset_id
          AND s.asset_id=r.asset_id AND s.state='complete' AND s.report->>'technical'='clean'
          AND s.report->>'media_type'='video/mp4' AND s.sha256=r.sha256 AND s.byte_length=r.byte_length
          AND o.stage='artifact' AND o.key_id=r.key_id AND o.turn_id=b.turn_id
          AND o.model_id IS NOT DISTINCT FROM b.model_id AND o.tool_name=b.tool_name
          AND o.operation_key='_br_runtime_artifact_release:'||r.asset_id::text||':'||s.id::text) THEN
        RAISE EXCEPTION 'native delivery must bind exact completed scan to its reserved source operation';
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER runtime_artifact_delivery_binding BEFORE INSERT ON runtime_artifact_deliveries
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_artifact_delivery();
CREATE TRIGGER runtime_artifact_delivery_immutable BEFORE UPDATE OR DELETE ON runtime_artifact_deliveries
    FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER runtime_artifact_delivery_no_truncate BEFORE TRUNCATE ON runtime_artifact_deliveries
    FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();

-- Concrete file + scan + full-policy grants. Do not insert native decisions
-- into legacy turn-wide guardrail_grants, and never read them as file approval.
CREATE TABLE runtime_artifact_grants (
    tenant_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    approval_id uuid NOT NULL,
    guardrail_id text NOT NULL,
    version integer NOT NULL,
    payload_hash bytea NOT NULL CHECK(octet_length(payload_hash)=32),
    policy_hash bytea NOT NULL CHECK(octet_length(policy_hash)=32),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(tenant_id,operation_id,policy_hash,guardrail_id,version),
    FOREIGN KEY(tenant_id,operation_id) REFERENCES runtime_artifact_deliveries(tenant_id,operation_id),
    FOREIGN KEY(tenant_id,approval_id) REFERENCES approvals(tenant_id,id),
    FOREIGN KEY(tenant_id,guardrail_id,version) REFERENCES guardrail_versions(tenant_id,id,version),
    CHECK(expires_at>created_at)
);
CREATE FUNCTION bind_runtime_artifact_grant() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    -- Human's existing transaction inserts grants before changing pending to
    -- approved. Gate use separately requires final state approved. Only the
    -- dedicated trusted decision purposes get INSERT on this table.
    IF NOT EXISTS(SELECT 1 FROM approvals a
        JOIN governance_operations o ON o.tenant_id=a.tenant_id AND o.id=a.operation_id
        JOIN governance_turns t ON t.tenant_id=o.tenant_id AND t.id=o.turn_id
        JOIN runtime_artifact_deliveries d ON d.tenant_id=o.tenant_id AND d.operation_id=o.id
        JOIN output_scans scan ON scan.tenant_id=d.tenant_id AND scan.id=d.scan_id
        JOIN assets asset ON asset.tenant_id=d.tenant_id AND asset.id=d.asset_id
        JOIN media_review_sessions s ON s.tenant_id=a.tenant_id AND s.approval_id=a.id
        JOIN media_review_completions c ON c.tenant_id=s.tenant_id AND c.review_id=s.id
        JOIN approval_reviews review ON review.tenant_id=s.tenant_id AND review.id=s.id
        WHERE a.tenant_id=NEW.tenant_id AND a.id=NEW.approval_id AND o.id=NEW.operation_id
          AND o.stage='artifact' AND a.state IN('pending','approved')
          AND o.payload_hash=NEW.payload_hash
          -- approvals.policy_hash hashes {policy,needed}; it is not the
          -- full selected-policy digest stored in isolated review sessions.
          AND s.payload_hash=NEW.payload_hash AND s.policy_hash=NEW.policy_hash
          AND review.payload_hash=NEW.payload_hash AND review.policy_hash=NEW.policy_hash
          AND review.can_continue AND review.approval_id=a.id AND review.user_id=t.user_id
          AND s.user_id=t.user_id AND s.sha256=asset.sha256 AND s.byte_length=asset.byte_length
          AND s.chunk_count=asset.chunk_count AND s.expires_at>clock_timestamp()
          AND review.expires_at>clock_timestamp() AND a.expires_at>clock_timestamp()
          AND asset.kind='runtime_output' AND asset.state='available' AND scan.state='complete'
          AND scan.report->>'technical'='clean' AND scan.report->>'media_type'='video/mp4'
          AND NEW.expires_at<=LEAST(t.expires_at,asset.expires_at,scan.expires_at)
          AND EXISTS(SELECT 1 FROM jsonb_array_elements(a.guardrails) risk
              WHERE risk->>'id'=NEW.guardrail_id AND risk->>'version'=NEW.version::text)) THEN
        RAISE EXCEPTION 'native grant requires its exact complete-byte human review and fixed authority scope';
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER runtime_artifact_grant_binding BEFORE INSERT ON runtime_artifact_grants
    FOR EACH ROW EXECUTE FUNCTION bind_runtime_artifact_grant();
CREATE TRIGGER runtime_artifact_grant_immutable BEFORE UPDATE OR DELETE ON runtime_artifact_grants
    FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER runtime_artifact_grant_no_truncate BEFORE TRUNCATE ON runtime_artifact_grants
    FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
