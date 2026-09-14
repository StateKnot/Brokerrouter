-- Explicit human permissions. No Agent key, runtime binding or approval grants
-- imply permission to read tenant-wide operational data.
CREATE TABLE console_permissions (
    tenant_id uuid NOT NULL,
    user_id uuid NOT NULL,
    permission text NOT NULL CHECK(permission IN ('usage_read','audit_read')),
    enabled boolean NOT NULL,
    PRIMARY KEY(tenant_id,user_id,permission),
    FOREIGN KEY(tenant_id,user_id) REFERENCES governance_users(tenant_id,id)
);
CREATE INDEX console_requests_time ON requests(tenant_id,created_at DESC,id DESC);
CREATE INDEX console_audit_time ON audit_events(tenant_id,created_at DESC,id DESC);

-- Purpose-specific projections: the reporting identity cannot select source
-- bodies/ciphertext, key hashes, routing snapshots, secrets or arbitrary JSON.
CREATE VIEW console_request_facts WITH (security_barrier=true) AS
SELECT r.tenant_id,r.id,r.key_id,k.name AS key_alias,r.model_id,m.name AS model_name,
       r.status,r.created_at,r.updated_at,r.error_code,
       CASE WHEN r.guardrail_evaluation_id IS NULL THEN 'model' ELSE 'guardrail' END AS purpose,
       CASE WHEN r.status='failed' THEN 0 ELSE l.amount_micros END AS cost_micros,
       CASE WHEN r.status IN ('reserved','submitting','submission_unknown')
            THEN r.reserve_micros ELSE 0 END AS reserved_micros,
       CASE WHEN jsonb_typeof(l.details->'input_tokens')='number'
                  AND l.details->>'input_tokens' ~ '^[0-9]{1,20}$'
            THEN (l.details->>'input_tokens')::numeric ELSE NULL END AS input_tokens,
       CASE WHEN jsonb_typeof(l.details->'output_tokens')='number'
                  AND l.details->>'output_tokens' ~ '^[0-9]{1,20}$'
            THEN (l.details->>'output_tokens')::numeric ELSE NULL END AS output_tokens
FROM requests r
JOIN virtual_keys k ON k.tenant_id=r.tenant_id AND k.id=r.key_id
JOIN models m ON m.tenant_id=r.tenant_id AND m.id=r.model_id
LEFT JOIN ledger_entries l ON l.tenant_id=r.tenant_id AND l.request_id=r.id AND l.kind='finalize';

CREATE VIEW console_audit_facts WITH (security_barrier=true) AS
SELECT tenant_id,id,action,object_id,created_at,database_actor::text AS database_actor
FROM audit_events;
