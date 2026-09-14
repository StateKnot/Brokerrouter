-- One immutable evaluation identity per operation and guardrail version. A lost
-- dispatch owner is never replaced: uncertain paid work goes to human review.
ALTER TABLE governance_operations DROP CONSTRAINT governance_operations_state_check;
ALTER TABLE governance_operations ADD CONSTRAINT governance_operations_state_check
    CHECK(state IN ('ready','checking','waiting_approval','rejected','feedback','executing','execution_unknown','completed'));
CREATE TABLE guardrail_evaluations (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    operation_id uuid NOT NULL,
    guardrail_id text NOT NULL,
    version integer NOT NULL,
    state text NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','running','passed','risk','failed')),
    reason_code text,
    deadline timestamptz,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    finished_at timestamptz,
    PRIMARY KEY(tenant_id,id),
    UNIQUE(tenant_id,operation_id,guardrail_id,version),
    FOREIGN KEY(tenant_id,operation_id) REFERENCES governance_operations(tenant_id,id),
    FOREIGN KEY(tenant_id,guardrail_id,version) REFERENCES guardrail_versions(tenant_id,id,version),
    CHECK ((state IN ('pending','running') AND reason_code IS NULL AND finished_at IS NULL)
        OR (state IN ('passed','risk','failed') AND reason_code IS NOT NULL AND finished_at IS NOT NULL)),
    CHECK (state <> 'running' OR deadline IS NOT NULL)
);
CREATE INDEX guardrail_evaluation_deadline ON guardrail_evaluations(deadline) WHERE state='running';
ALTER TABLE requests ADD COLUMN guardrail_evaluation_id uuid;
ALTER TABLE requests ADD CONSTRAINT request_evaluation_fk FOREIGN KEY(tenant_id,guardrail_evaluation_id)
    REFERENCES guardrail_evaluations(tenant_id,id);
CREATE UNIQUE INDEX one_request_per_evaluation ON requests(tenant_id,guardrail_evaluation_id)
    WHERE guardrail_evaluation_id IS NOT NULL;

CREATE FUNCTION protect_evaluation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.id,NEW.operation_id,NEW.guardrail_id,NEW.version,NEW.created_at)
        IS DISTINCT FROM (OLD.tenant_id,OLD.id,OLD.operation_id,OLD.guardrail_id,OLD.version,OLD.created_at)
        OR (OLD.state IN ('passed','risk','failed') AND NEW IS DISTINCT FROM OLD)
        OR (OLD.state='running' AND (NEW.state='pending' OR NEW.deadline IS DISTINCT FROM OLD.deadline)) THEN
        RAISE EXCEPTION 'evaluation identity, dispatch ownership and terminal facts are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER evaluation_immutable BEFORE UPDATE ON guardrail_evaluations FOR EACH ROW EXECUTE FUNCTION protect_evaluation();
CREATE TRIGGER evaluation_no_delete BEFORE DELETE ON guardrail_evaluations FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER evaluation_no_truncate BEFORE TRUNCATE ON guardrail_evaluations FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
CREATE FUNCTION protect_request_evaluation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.guardrail_evaluation_id IS DISTINCT FROM OLD.guardrail_evaluation_id THEN
        RAISE EXCEPTION 'request evaluation purpose is immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER request_evaluation_immutable BEFORE UPDATE ON requests FOR EACH ROW EXECUTE FUNCTION protect_request_evaluation();
