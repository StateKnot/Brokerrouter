CREATE TABLE output_scan_profiles (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    model_id text NOT NULL,
    version integer NOT NULL CHECK(version>0),
    engine_revision text NOT NULL CHECK(engine_revision ~ '^[a-f0-9]{64}$'),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,model_id,version),
    FOREIGN KEY(tenant_id,model_id) REFERENCES models(tenant_id,id)
);
CREATE UNIQUE INDEX one_active_output_scan_profile ON output_scan_profiles(tenant_id,model_id) WHERE active;
CREATE TABLE output_scanner_tokens (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    profile_id uuid NOT NULL,
    token_hash bytea NOT NULL UNIQUE CHECK(octet_length(token_hash)=32),
    name text NOT NULL CHECK(length(name) BETWEEN 1 AND 200),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,profile_id) REFERENCES output_scan_profiles(tenant_id,id),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '90 days')
);
CREATE TABLE output_scans (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    job_id uuid NOT NULL,
    asset_id uuid NOT NULL,
    key_id uuid NOT NULL,
    profile_id uuid NOT NULL,
    sha256 bytea NOT NULL CHECK(octet_length(sha256)=32),
    byte_length bigint NOT NULL CHECK(byte_length BETWEEN 1 AND 536870912),
    state text NOT NULL DEFAULT 'scanning' CHECK(state IN ('scanning','complete','closed')),
    current_attempt uuid,
    attempts integer NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 3),
    report jsonb,
    report_hash bytea CHECK(octet_length(report_hash)=32),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    finished_at timestamptz,
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,job_id,profile_id),
    FOREIGN KEY(tenant_id,job_id,key_id) REFERENCES media_jobs(tenant_id,id,key_id),
    FOREIGN KEY(tenant_id,asset_id,key_id) REFERENCES assets(tenant_id,id,key_id),
    FOREIGN KEY(tenant_id,profile_id) REFERENCES output_scan_profiles(tenant_id,id),
    CHECK((state='complete' AND report IS NOT NULL AND report_hash IS NOT NULL AND finished_at IS NOT NULL)
       OR (state<>'complete' AND report IS NULL AND report_hash IS NULL)),
    CHECK(expires_at>created_at AND expires_at<=created_at+interval '24 hours')
);
CREATE TABLE output_scan_attempts (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    scan_id uuid NOT NULL,
    token_id uuid NOT NULL,
    claim_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    lease_until timestamptz NOT NULL,
    deadline timestamptz NOT NULL,
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,token_id,claim_id),
    FOREIGN KEY(tenant_id,scan_id) REFERENCES output_scans(tenant_id,id),
    FOREIGN KEY(tenant_id,token_id) REFERENCES output_scanner_tokens(tenant_id,id),
    CHECK(lease_until>created_at AND lease_until<=deadline AND deadline<=created_at+interval '30 minutes')
);
ALTER TABLE output_scans ADD FOREIGN KEY(tenant_id,current_attempt) REFERENCES output_scan_attempts(tenant_id,id);
CREATE INDEX output_scan_open ON output_scans(tenant_id,profile_id,expires_at) WHERE state='scanning';
CREATE FUNCTION protect_output_scan_profile() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.model_id,NEW.version,NEW.engine_revision,NEW.created_at)
      IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.model_id,OLD.version,OLD.engine_revision,OLD.created_at)
      OR (NOT OLD.active AND NEW.active) THEN RAISE EXCEPTION 'scan profile is immutable and cannot reopen'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER output_scan_profile_immutable BEFORE UPDATE ON output_scan_profiles FOR EACH ROW EXECUTE FUNCTION protect_output_scan_profile();
CREATE FUNCTION protect_output_scanner_token() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.profile_id,NEW.token_hash,NEW.name,NEW.created_at,NEW.expires_at)
      IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.profile_id,OLD.token_hash,OLD.name,OLD.created_at,OLD.expires_at)
      OR (OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at) THEN RAISE EXCEPTION 'scanner credential is immutable'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER output_scanner_token_immutable BEFORE UPDATE ON output_scanner_tokens FOR EACH ROW EXECUTE FUNCTION protect_output_scanner_token();
CREATE FUNCTION protect_output_scan() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='INSERT' THEN
        IF NOT EXISTS(SELECT 1 FROM assets a JOIN requests r ON r.tenant_id=a.tenant_id AND r.id=a.source_job_id
            JOIN output_scan_profiles p ON p.tenant_id=r.tenant_id AND p.model_id=r.model_id
            WHERE a.tenant_id=NEW.tenant_id AND a.id=NEW.asset_id AND a.source_job_id=NEW.job_id AND a.key_id=NEW.key_id
              AND a.kind='output' AND a.state='available' AND a.sha256=NEW.sha256 AND a.byte_length=NEW.byte_length
              AND a.expires_at>=NEW.expires_at AND p.id=NEW.profile_id) THEN RAISE EXCEPTION 'scan must bind exact output and model profile'; END IF;
    ELSE
        IF (NEW.tenant_id,NEW.id,NEW.job_id,NEW.asset_id,NEW.key_id,NEW.profile_id,NEW.sha256,NEW.byte_length,NEW.created_at,NEW.expires_at)
          IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.job_id,OLD.asset_id,OLD.key_id,OLD.profile_id,OLD.sha256,OLD.byte_length,OLD.created_at,OLD.expires_at)
          OR (OLD.state<>'scanning' AND NEW IS DISTINCT FROM OLD)
          OR NEW.attempts<OLD.attempts OR NEW.attempts>OLD.attempts+1 THEN RAISE EXCEPTION 'scan identity and receipt are immutable'; END IF;
    END IF;
    IF NEW.current_attempt IS NOT NULL AND NOT EXISTS(SELECT 1 FROM output_scan_attempts a JOIN output_scanner_tokens t ON t.tenant_id=a.tenant_id AND t.id=a.token_id
       WHERE a.tenant_id=NEW.tenant_id AND a.id=NEW.current_attempt AND a.scan_id=NEW.id AND t.profile_id=NEW.profile_id) THEN RAISE EXCEPTION 'attempt belongs to another scan or profile'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER output_scan_immutable BEFORE INSERT OR UPDATE ON output_scans FOR EACH ROW EXECUTE FUNCTION protect_output_scan();
CREATE FUNCTION protect_output_scan_attempt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.scan_id,NEW.token_id,NEW.claim_id,NEW.created_at,NEW.deadline)
      IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.scan_id,OLD.token_id,OLD.claim_id,OLD.created_at,OLD.deadline)
      OR NEW.lease_until<OLD.lease_until OR OLD.lease_until<=clock_timestamp() THEN RAISE EXCEPTION 'scan attempt cannot change owner or revive'; END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER output_scan_attempt_immutable BEFORE UPDATE ON output_scan_attempts FOR EACH ROW EXECUTE FUNCTION protect_output_scan_attempt();
DO $$ DECLARE tab text; BEGIN
    FOREACH tab IN ARRAY ARRAY['output_scan_profiles','output_scanner_tokens','output_scans','output_scan_attempts'] LOOP
        EXECUTE format('CREATE TRIGGER scan_fact_no_delete BEFORE DELETE ON %I FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation()',tab);
        EXECUTE format('CREATE TRIGGER scan_fact_no_truncate BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation()',tab);
    END LOOP;
END $$;
