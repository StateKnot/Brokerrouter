CREATE TABLE media_endpoints (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    model_id text NOT NULL,
    config jsonb NOT NULL,
    priority integer NOT NULL CHECK(priority BETWEEN 0 AND 1000000),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,model_id) REFERENCES models(tenant_id,id)
);
CREATE FUNCTION protect_media_endpoint() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.model_id,NEW.config,NEW.priority,NEW.created_at)
        IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.model_id,OLD.config,OLD.priority,OLD.created_at) THEN
        RAISE EXCEPTION 'media endpoint versions are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER media_endpoint_immutable BEFORE UPDATE ON media_endpoints FOR EACH ROW EXECUTE FUNCTION protect_media_endpoint();
ALTER TABLE requests ADD CONSTRAINT request_key_identity UNIQUE(tenant_id,id,key_id);
CREATE TABLE media_jobs (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    key_id uuid NOT NULL,
    endpoint_id uuid NOT NULL,
    state text NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','waiting_approval','submitting','submission_unknown','running','output_pending','needs_reconciliation','cancelled','reconciled')),
    input_ciphertext bytea,
    output_ciphertext bytea,
    output_expires_at timestamptz,
    provider_task_id text CHECK(length(provider_task_id) BETWEEN 1 AND 200 AND provider_task_id ~ '^[a-zA-Z0-9_-]+$'),
    provider_status text,
    cancel_requested boolean NOT NULL DEFAULT false,
    next_poll_at timestamptz NOT NULL DEFAULT now(),
    lease_token uuid,
    lease_until timestamptz,
    event_clock bigint NOT NULL DEFAULT 0 CHECK(event_clock>=0),
    error_code text,
    created_at timestamptz NOT NULL DEFAULT now(),
    admission_deadline timestamptz NOT NULL DEFAULT now()+interval '1 hour',
    poll_deadline timestamptz NOT NULL DEFAULT now()+interval '23 hours',
    PRIMARY KEY(tenant_id,id),
    FOREIGN KEY(tenant_id,id,key_id) REFERENCES requests(tenant_id,id,key_id),
    FOREIGN KEY(tenant_id,key_id) REFERENCES virtual_keys(tenant_id,id),
    FOREIGN KEY(tenant_id,endpoint_id) REFERENCES media_endpoints(tenant_id,id),
    CHECK((lease_token IS NULL)=(lease_until IS NULL)),
    CHECK(output_ciphertext IS NULL OR (state='output_pending' AND output_expires_at IS NOT NULL)),
    CHECK(state NOT IN ('running','output_pending') OR provider_task_id IS NOT NULL)
);
CREATE INDEX media_work ON media_jobs(next_poll_at) WHERE state IN ('queued','waiting_approval','running','submitting');
CREATE INDEX media_outstanding ON media_jobs(tenant_id,key_id) WHERE state IN ('queued','waiting_approval','submitting','submission_unknown','running','needs_reconciliation');
CREATE INDEX media_output_expiry ON media_jobs(output_expires_at) WHERE output_ciphertext IS NOT NULL;
CREATE INDEX media_input_expiry ON media_jobs(poll_deadline) WHERE input_ciphertext IS NOT NULL AND state NOT IN ('queued','waiting_approval','running','submitting');
CREATE TABLE media_events (
    tenant_id uuid NOT NULL,
    job_id uuid NOT NULL,
    sequence bigint NOT NULL,
    state text NOT NULL,
    code text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,job_id,sequence),
    FOREIGN KEY(tenant_id,job_id) REFERENCES media_jobs(tenant_id,id)
);
CREATE FUNCTION protect_media_job() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.key_id,NEW.endpoint_id,NEW.created_at,NEW.admission_deadline,NEW.poll_deadline)
        IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.key_id,OLD.endpoint_id,OLD.created_at,OLD.admission_deadline,OLD.poll_deadline)
        OR (OLD.provider_task_id IS NOT NULL AND NEW.provider_task_id IS DISTINCT FROM OLD.provider_task_id)
        OR (NEW.input_ciphertext IS NOT NULL AND NEW.input_ciphertext IS DISTINCT FROM OLD.input_ciphertext)
        OR (OLD.output_ciphertext IS NOT NULL AND NEW.output_ciphertext IS NOT NULL AND NEW.output_ciphertext IS DISTINCT FROM OLD.output_ciphertext)
        OR (OLD.cancel_requested AND NOT NEW.cancel_requested)
        OR (OLD.state IN ('submitting','submission_unknown','running','output_pending','needs_reconciliation','cancelled','reconciled') AND NEW.state IN ('queued','waiting_approval'))
        OR (OLD.state IN ('output_pending','cancelled','reconciled') AND NEW.state<>OLD.state) THEN
        RAISE EXCEPTION 'media identity, submitted work and terminal state cannot be rewritten';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER media_job_immutable BEFORE UPDATE ON media_jobs FOR EACH ROW EXECUTE FUNCTION protect_media_job();
CREATE TRIGGER media_events_immutable BEFORE UPDATE OR DELETE ON media_events FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER media_events_no_truncate BEFORE TRUNCATE ON media_events FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER media_jobs_no_delete BEFORE DELETE ON media_jobs FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER media_jobs_no_truncate BEFORE TRUNCATE ON media_jobs FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER media_endpoints_no_delete BEFORE DELETE ON media_endpoints FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER media_endpoints_no_truncate BEFORE TRUNCATE ON media_endpoints FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
