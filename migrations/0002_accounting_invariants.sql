ALTER TABLE tenants ADD CONSTRAINT tenant_budget_bound
    CHECK (spent_micros <= budget_micros AND reserved_micros <= budget_micros - spent_micros);
ALTER TABLE virtual_keys ADD CONSTRAINT key_budget_bound
    CHECK (spent_micros <= budget_micros AND reserved_micros <= budget_micros - spent_micros);
ALTER TABLE requests ADD CONSTRAINT settlement_state
    CHECK (
        (status IN ('reserved','submitting','submission_unknown') AND final_micros IS NULL)
        OR (status = 'succeeded' AND final_micros IS NOT NULL AND final_micros <= reserve_micros)
        OR (status = 'failed' AND final_micros = 0)
    );
ALTER TABLE audit_events ADD COLUMN database_actor name NOT NULL DEFAULT session_user;

CREATE FUNCTION protect_request_admission() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.id, NEW.tenant_id, NEW.key_id, NEW.model_id, NEW.idempotency_key,
        NEW.request_hash, NEW.reserve_micros, NEW.price_snapshot, NEW.created_at)
       IS DISTINCT FROM
       (OLD.id, OLD.tenant_id, OLD.key_id, OLD.model_id, OLD.idempotency_key,
        OLD.request_hash, OLD.reserve_micros, OLD.price_snapshot, OLD.created_at) THEN
        RAISE EXCEPTION 'request admission facts are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER request_admission_immutable BEFORE UPDATE ON requests
FOR EACH ROW EXECUTE FUNCTION protect_request_admission();

CREATE TRIGGER ledger_no_truncate BEFORE TRUNCATE ON ledger_entries
FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER fx_no_truncate BEFORE TRUNCATE ON fx_rates
FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER audit_no_truncate BEFORE TRUNCATE ON audit_events
FOR EACH STATEMENT EXECUTE FUNCTION prevent_fact_mutation();
