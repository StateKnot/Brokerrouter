ALTER TABLE endpoints ADD COLUMN operation text NOT NULL DEFAULT 'chat' CHECK (operation IN ('chat','embedding'));
ALTER TABLE endpoints ADD COLUMN supports_tools boolean NOT NULL DEFAULT false;
ALTER TABLE endpoints ADD COLUMN embedding_dimension integer CHECK (embedding_dimension BETWEEN 1 AND 65536);
ALTER TABLE endpoints ADD CONSTRAINT embedding_contract CHECK (
    (operation='chat' AND embedding_dimension IS NULL) OR
    (operation='embedding' AND embedding_dimension IS NOT NULL AND NOT supports_tools)
);
ALTER TABLE requests ADD COLUMN routing_snapshot jsonb;
ALTER TABLE requests ADD COLUMN dispatch_deadline timestamptz;
ALTER TABLE requests ADD COLUMN result_expires_at timestamptz;
ALTER TABLE requests DROP CONSTRAINT requests_status_check;
ALTER TABLE requests ADD CONSTRAINT requests_status_check CHECK(status IN ('reserved','submitting','submission_unknown','succeeded','failed','reconciled'));
ALTER TABLE requests DROP CONSTRAINT settlement_state;
ALTER TABLE requests ADD CONSTRAINT settlement_state CHECK (
    (status IN ('reserved','submitting','submission_unknown') AND final_micros IS NULL)
    OR (status IN ('succeeded','reconciled') AND final_micros IS NOT NULL AND final_micros <= reserve_micros)
    OR (status='failed' AND final_micros=0)
);
CREATE INDEX request_recovery ON requests(dispatch_deadline) WHERE status IN ('reserved','submitting');
CREATE INDEX result_expiry ON requests(result_expires_at) WHERE result_ciphertext IS NOT NULL;
CREATE INDEX request_log_page ON requests(tenant_id,key_id,created_at DESC,id DESC);

CREATE TABLE attempts (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    request_id uuid NOT NULL,
    endpoint_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal BETWEEN 0 AND 2),
    status text NOT NULL CHECK (status IN ('submitting','not_sent','submission_unknown','succeeded','reconciled')),
    http_status integer CHECK (http_status BETWEEN 100 AND 599),
    error_code text,
    usage jsonb,
    cost_micros bigint CHECK (cost_micros >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    UNIQUE(tenant_id,request_id,ordinal),
    FOREIGN KEY(tenant_id,request_id) REFERENCES requests(tenant_id,id),
    FOREIGN KEY(tenant_id,endpoint_id) REFERENCES endpoints(tenant_id,id)
);
CREATE UNIQUE INDEX one_submitting_attempt ON attempts(tenant_id,request_id) WHERE status='submitting';

CREATE OR REPLACE FUNCTION protect_request_admission() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.id, NEW.tenant_id, NEW.key_id, NEW.model_id, NEW.idempotency_key,
        NEW.request_hash, NEW.reserve_micros, NEW.price_snapshot, NEW.created_at,
        NEW.routing_snapshot, NEW.dispatch_deadline)
       IS DISTINCT FROM
       (OLD.id, OLD.tenant_id, OLD.key_id, OLD.model_id, OLD.idempotency_key,
        OLD.request_hash, OLD.reserve_micros, OLD.price_snapshot, OLD.created_at,
        OLD.routing_snapshot, OLD.dispatch_deadline) THEN
        RAISE EXCEPTION 'request admission facts are immutable';
    END IF;
    RETURN NEW;
END;
$$;
