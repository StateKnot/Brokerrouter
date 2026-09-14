ALTER TABLE guardrail_versions DROP CONSTRAINT guardrail_versions_stage_check;
ALTER TABLE guardrail_versions ADD CHECK(stage IN ('pre','post','tool_input','tool_output','artifact','media_output'));
ALTER TABLE governance_operations DROP CONSTRAINT governance_operations_stage_check;
ALTER TABLE governance_operations ADD CHECK(stage IN ('pre','post','tool_input','tool_output','artifact','media_output'));

-- This is an immutable evidence binding, not a cached authorization or public URL.
CREATE TABLE media_output_deliveries (
    tenant_id uuid NOT NULL,
    scan_id uuid NOT NULL,
    job_id uuid NOT NULL,
    operation_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(tenant_id,scan_id),
    UNIQUE(tenant_id,operation_id),
    FOREIGN KEY(tenant_id,scan_id) REFERENCES output_scans(tenant_id,id),
    FOREIGN KEY(tenant_id,job_id) REFERENCES media_jobs(tenant_id,id),
    FOREIGN KEY(tenant_id,operation_id) REFERENCES governance_operations(tenant_id,id)
);
CREATE INDEX output_delivery_job ON media_output_deliveries(tenant_id,job_id);
CREATE FUNCTION validate_output_delivery() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM output_scans s
        JOIN requests r ON r.tenant_id=s.tenant_id AND r.id=s.job_id
        JOIN governance_operations o ON o.tenant_id=s.tenant_id AND o.id=NEW.operation_id
        WHERE s.tenant_id=NEW.tenant_id AND s.id=NEW.scan_id AND s.job_id=NEW.job_id
          AND s.state='complete' AND s.report->>'technical'='clean'
          AND o.stage='media_output' AND o.key_id=s.key_id AND o.turn_id=r.governance_turn_id
          AND o.model_id=r.model_id AND o.tool_name IS NULL
          AND o.operation_key='media-output:'||s.id::text) THEN
        RAISE EXCEPTION 'output delivery must bind a completed scan to its server operation';
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER output_delivery_valid BEFORE INSERT ON media_output_deliveries FOR EACH ROW EXECUTE FUNCTION validate_output_delivery();
CREATE TRIGGER output_delivery_immutable BEFORE UPDATE OR DELETE ON media_output_deliveries FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
