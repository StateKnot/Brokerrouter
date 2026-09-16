-- One observed check per operation / immutable rule version. Polling and a later
-- same-turn grant must not erase the original risk or multiply the denominator.
CREATE TABLE guardrail_observations (
    tenant_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    guardrail_id text NOT NULL,
    version integer NOT NULL,
    first_outcome text NOT NULL CHECK(first_outcome IN ('pending','passed','risk','failed','skipped_by_user_approval')),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    approval_skipped_at timestamptz,
    inline_duration_us bigint CHECK(inline_duration_us >= 0),
    source text NOT NULL CHECK(source IN ('live','legacy_event','legacy_evaluation')),
    PRIMARY KEY(tenant_id,operation_id,guardrail_id,version),
    FOREIGN KEY(tenant_id,operation_id) REFERENCES governance_operations(tenant_id,id),
    FOREIGN KEY(tenant_id,guardrail_id,version) REFERENCES guardrail_versions(tenant_id,id,version)
);
CREATE INDEX monitoring_observations_time ON guardrail_observations(tenant_id,created_at,guardrail_id,version);
CREATE INDEX monitoring_approvals_operation ON approvals(tenant_id,operation_id);

-- Existing semantic evaluations are authoritative even if no client polled again.
INSERT INTO guardrail_observations(tenant_id,operation_id,guardrail_id,version,first_outcome,created_at,source)
SELECT tenant_id,operation_id,guardrail_id,version,'pending',created_at,'legacy_evaluation'
FROM guardrail_evaluations;
-- Only join validated identities to immutable versions. No arbitrary event JSON
-- reaches the reporting role; repeated events collapse to their first observation.
WITH facts AS (
    SELECT e.tenant_id,o.id AS operation_id,v.id AS guardrail_id,v.version,
           r.value->>'outcome' AS outcome,e.created_at,e.sequence
    FROM governance_events e
    CROSS JOIN LATERAL jsonb_array_elements(CASE
        WHEN e.event_type='guardrails.evaluated' AND jsonb_typeof(e.data->'results')='array' THEN e.data->'results'
        WHEN e.event_type='approval.required' AND jsonb_typeof(e.data->'evaluations')='array' THEN e.data->'evaluations'
        ELSE '[]'::jsonb END) r
    JOIN governance_operations o ON o.tenant_id=e.tenant_id AND o.turn_id=e.turn_id AND o.id::text=e.data->>'operation_id'
    JOIN guardrail_versions v ON v.tenant_id=e.tenant_id AND v.id=r.value->>'id' AND v.version::text=r.value->>'version'
    WHERE r.value->>'outcome' IN ('passed','risk','failed','skipped_by_user_approval')
), firsts AS (
    SELECT DISTINCT ON (tenant_id,operation_id,guardrail_id,version) * FROM facts
    ORDER BY tenant_id,operation_id,guardrail_id,version,created_at,sequence
), skips AS (
    SELECT tenant_id,operation_id,guardrail_id,version,min(created_at) AS skipped_at
    FROM facts WHERE outcome='skipped_by_user_approval' GROUP BY 1,2,3,4
)
INSERT INTO guardrail_observations(tenant_id,operation_id,guardrail_id,version,first_outcome,created_at,approval_skipped_at,source)
SELECT f.tenant_id,f.operation_id,f.guardrail_id,f.version,f.outcome,f.created_at,s.skipped_at,'legacy_event'
FROM firsts f LEFT JOIN skips s USING(tenant_id,operation_id,guardrail_id,version)
ON CONFLICT(tenant_id,operation_id,guardrail_id,version) DO UPDATE
SET approval_skipped_at=EXCLUDED.approval_skipped_at;

CREATE FUNCTION protect_guardrail_observation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.tenant_id,NEW.operation_id,NEW.guardrail_id,NEW.version,NEW.first_outcome,NEW.created_at,NEW.inline_duration_us,NEW.source)
       IS DISTINCT FROM (OLD.tenant_id,OLD.operation_id,OLD.guardrail_id,OLD.version,OLD.first_outcome,OLD.created_at,OLD.inline_duration_us,OLD.source)
       OR (OLD.approval_skipped_at IS NOT NULL AND NEW.approval_skipped_at IS DISTINCT FROM OLD.approval_skipped_at) THEN
        RAISE EXCEPTION 'original guardrail observation is immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER observation_immutable BEFORE UPDATE ON guardrail_observations FOR EACH ROW EXECUTE FUNCTION protect_guardrail_observation();
CREATE TRIGGER observation_no_delete BEFORE DELETE ON guardrail_observations FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER observation_no_truncate BEFORE TRUNCATE ON guardrail_observations FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();

CREATE TABLE monitoring_coverage (
    singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
    recorded_from timestamptz NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO monitoring_coverage DEFAULT VALUES;
CREATE TRIGGER monitoring_coverage_immutable BEFORE UPDATE OR DELETE ON monitoring_coverage FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();

CREATE VIEW console_guardrail_facts WITH (security_barrier=true) AS
SELECT b.tenant_id,b.operation_id,b.guardrail_id,b.version,b.created_at,b.source,
       b.approval_skipped_at,b.inline_duration_us,v.name,v.stage,v.rule->>'kind' AS kind,
       g.active AND g.version=b.version AS current_active,
       o.turn_id,o.key_id,o.model_id,o.state AS operation_state,o.execution_deadline,
       t.state AS turn_state,t.expires_at AS turn_expires_at,
       CASE WHEN b.first_outcome='pending' THEN COALESCE(e.state,'pending') ELSE b.first_outcome END AS outcome,
       e.deadline AS judge_deadline,
       CASE WHEN e.finished_at>=e.created_at THEN extract(epoch FROM e.finished_at-e.created_at)*1000 ELSE NULL END AS judge_lifecycle_ms,
       r.id AS request_id,rf.cost_micros,rf.reserved_micros
FROM guardrail_observations b
JOIN guardrail_versions v ON v.tenant_id=b.tenant_id AND v.id=b.guardrail_id AND v.version=b.version
JOIN guardrails g ON g.tenant_id=v.tenant_id AND g.id=v.id
JOIN governance_operations o ON o.tenant_id=b.tenant_id AND o.id=b.operation_id
JOIN governance_turns t ON t.tenant_id=o.tenant_id AND t.id=o.turn_id
LEFT JOIN guardrail_evaluations e ON e.tenant_id=b.tenant_id AND e.operation_id=b.operation_id AND e.guardrail_id=b.guardrail_id AND e.version=b.version
LEFT JOIN requests r ON r.tenant_id=e.tenant_id AND r.guardrail_evaluation_id=e.id
LEFT JOIN console_request_facts rf ON rf.tenant_id=r.tenant_id AND rf.id=r.id;

CREATE VIEW console_approval_facts WITH (security_barrier=true) AS
SELECT tenant_id,id,operation_id,state,created_at,expires_at,decided_at FROM approvals;
CREATE VIEW console_active_guardrails WITH (security_barrier=true) AS
SELECT tenant_id,id FROM guardrails WHERE active;
ALTER TABLE console_permissions DROP CONSTRAINT console_permissions_permission_check;
ALTER TABLE console_permissions ADD CONSTRAINT console_permissions_permission_check
CHECK(permission IN ('usage_read','audit_read','catalog_read','catalog_write','keys_read','keys_write','fx_read','fx_write','governance_read','governance_write','monitoring_read'));
