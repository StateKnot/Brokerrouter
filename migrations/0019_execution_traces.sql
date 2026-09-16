-- Read-only metadata projections. No payloads, receipt contents, feedback,
-- idempotency keys, provider URLs, credentials or unfiltered event JSON.
ALTER TABLE console_permissions DROP CONSTRAINT console_permissions_permission_check;
ALTER TABLE console_permissions ADD CONSTRAINT console_permissions_permission_check
CHECK(permission IN ('usage_read','audit_read','catalog_read','catalog_write','keys_read','keys_write','fx_read','fx_write','governance_read','governance_write','monitoring_read','traces_read'));
CREATE INDEX trace_turn_time ON governance_turns(tenant_id,created_at DESC,id DESC);
CREATE INDEX trace_nodes_time ON governance_operations(tenant_id,turn_id,created_at DESC,id DESC);
CREATE INDEX trace_requests_turn ON requests(tenant_id,governance_turn_id,created_at DESC,id DESC);
CREATE INDEX trace_approval_time ON approvals(tenant_id,turn_id,created_at DESC,id DESC);

CREATE VIEW console_trace_turns WITH (security_barrier=true) AS
SELECT t.tenant_id,t.id,t.key_id,t.user_id,t.session_id,t.run_id,t.generation,
       t.state AS recorded_state,
       CASE WHEN t.state='active' AND t.expires_at<=transaction_timestamp() THEN 'expired' ELSE t.state END AS state,
       t.created_at,t.expires_at,t.event_clock,COALESCE(a.sequence,0) AS acknowledged_sequence,
       b.runtime_id,b.agent_id,k.name AS key_alias,
       u.active AND b.active AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>transaction_timestamp()) AS identity_active
FROM governance_turns t
JOIN runtime_bindings b ON b.tenant_id=t.tenant_id AND b.key_id=t.key_id
JOIN virtual_keys k ON k.tenant_id=t.tenant_id AND k.id=t.key_id
JOIN governance_users u ON u.tenant_id=t.tenant_id AND u.id=t.user_id
LEFT JOIN governance_event_acks a ON a.tenant_id=t.tenant_id AND a.turn_id=t.id;

CREATE VIEW console_trace_nodes WITH (security_barrier=true) AS
SELECT tenant_id,id,turn_id,key_id,stage,model_id,tool_name,created_at,execution_deadline,
       state AS recorded_state,
       CASE WHEN state='executing' AND execution_deadline<=transaction_timestamp() THEN 'execution_unknown' ELSE state END AS state,
       encode(payload_hash,'hex') AS operation_sha256
FROM governance_operations;

-- Judge calls intentionally have no model governance turn; the immutable
-- evaluation -> operation relationship supplies their real owning turn.
CREATE VIEW console_trace_requests WITH (security_barrier=true) AS
SELECT f.*,r.governance_turn_id AS turn_id,
       NULL::uuid AS judge_operation_id,j.state AS media_state,j.endpoint_id AS media_endpoint_id,j.cancel_requested,
       r.dispatch_deadline,r.result_expires_at
FROM console_request_facts f
JOIN requests r ON r.tenant_id=f.tenant_id AND r.id=f.id
LEFT JOIN media_jobs j ON j.tenant_id=r.tenant_id AND j.id=r.id
WHERE r.guardrail_evaluation_id IS NULL AND r.governance_turn_id IS NOT NULL
UNION ALL
-- Keep turn_id a direct indexed column in both disjoint branches. A CASE
-- predicate would force scanning all tenant requests for each task detail.
SELECT f.*,o.turn_id,o.id,NULL::text,NULL::uuid,NULL::boolean,r.dispatch_deadline,r.result_expires_at
FROM governance_operations o
JOIN guardrail_evaluations e ON e.tenant_id=o.tenant_id AND e.operation_id=o.id
JOIN requests r ON r.tenant_id=e.tenant_id AND r.guardrail_evaluation_id=e.id
JOIN console_request_facts f ON f.tenant_id=r.tenant_id AND f.id=r.id;

-- Associations follow existing server-owned gateway contracts, not fuzzy text
-- matching or inferred runtime DAG edges. The request remains the cost unit.
CREATE VIEW console_trace_links WITH (security_barrier=true) AS
SELECT r.tenant_id,r.id AS request_id,o.id AS operation_id,o.turn_id,'model_input'::text AS relation
FROM requests r JOIN governance_operations o ON o.tenant_id=r.tenant_id AND o.key_id=r.key_id AND o.turn_id=r.governance_turn_id AND o.model_id=r.model_id
WHERE r.guardrail_evaluation_id IS NULL AND o.stage='pre' AND o.operation_key=r.idempotency_key
AND NOT EXISTS(SELECT 1 FROM media_jobs j WHERE j.tenant_id=r.tenant_id AND j.id=r.id)
UNION ALL
SELECT r.tenant_id,r.id,o.id,o.turn_id,'model_output'
FROM requests r JOIN governance_operations o ON o.tenant_id=r.tenant_id AND o.key_id=r.key_id AND o.turn_id=r.governance_turn_id AND o.model_id=r.model_id
WHERE r.guardrail_evaluation_id IS NULL AND o.stage='post' AND o.operation_key=r.id::text
UNION ALL
SELECT r.tenant_id,r.id,o.id,o.turn_id,'media_input'
FROM media_jobs j JOIN requests r ON r.tenant_id=j.tenant_id AND r.id=j.id
JOIN governance_operations o ON o.tenant_id=r.tenant_id AND o.key_id=r.key_id AND o.turn_id=r.governance_turn_id AND o.model_id=r.model_id
WHERE o.stage='pre' AND o.operation_key='media:'||j.id::text
UNION ALL
SELECT r.tenant_id,r.id,o.id,o.turn_id,'media_output'
FROM media_output_deliveries d JOIN requests r ON r.tenant_id=d.tenant_id AND r.id=d.job_id
JOIN governance_operations o ON o.tenant_id=d.tenant_id AND o.id=d.operation_id AND o.turn_id=r.governance_turn_id AND o.key_id=r.key_id
UNION ALL
SELECT r.tenant_id,r.id,o.id,o.turn_id,'guardrail_judge'
FROM requests r JOIN guardrail_evaluations e ON e.tenant_id=r.tenant_id AND e.id=r.guardrail_evaluation_id
JOIN governance_operations o ON o.tenant_id=e.tenant_id AND o.id=e.operation_id;

CREATE VIEW console_trace_attempts WITH (security_barrier=true) AS
SELECT a.tenant_id,a.request_id,a.id,a.endpoint_id,e.name AS endpoint_name,a.ordinal,a.status,a.http_status,a.error_code,a.cost_micros,a.created_at,a.finished_at
FROM attempts a JOIN endpoints e ON e.tenant_id=a.tenant_id AND e.id=a.endpoint_id;
CREATE VIEW console_trace_approvals WITH (security_barrier=true) AS
SELECT tenant_id,id,turn_id,operation_id,state AS recorded_state,
       CASE WHEN state='pending' AND expires_at<=transaction_timestamp() THEN 'expired_pending' ELSE state END AS state,
       created_at,expires_at,decided_at,decided_by
FROM approvals;
CREATE VIEW console_trace_grants WITH (security_barrier=true) AS
SELECT tenant_id,turn_id,approval_id,guardrail_id,version,expires_at FROM guardrail_grants;
CREATE VIEW console_trace_events WITH (security_barrier=true) AS
SELECT e.tenant_id,e.turn_id,e.sequence,e.created_at,
       CASE WHEN e.event_type IN ('turn.created','turn.closed','approval.required','approval.decided','guardrails.evaluated','guardrail.started','guardrail.finished','operation.claimed','operation.receipted','operation.reconciled') THEN e.event_type ELSE 'unrecognized' END AS event_type,
       o.id AS operation_id,a.id AS approval_id,
       CASE WHEN e.event_type='operation.reconciled' AND e.data->>'outcome' IN ('confirmed_executed','confirmed_not_executed') THEN e.data->>'outcome' END AS reconciliation
FROM governance_events e
LEFT JOIN guardrail_evaluations g ON g.tenant_id=e.tenant_id AND e.event_type IN ('guardrail.started','guardrail.finished') AND g.id::text=e.data->>'evaluation_id'
LEFT JOIN governance_operations o ON o.tenant_id=e.tenant_id AND o.turn_id=e.turn_id
    AND o.id::text=CASE WHEN e.event_type='guardrail.finished' THEN g.operation_id::text ELSE e.data->>'operation_id' END
LEFT JOIN approvals a ON a.tenant_id=e.tenant_id AND a.turn_id=e.turn_id AND a.id::text=e.data->>'approval_id';
