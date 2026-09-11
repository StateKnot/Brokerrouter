CREATE TABLE tenants (
    id uuid PRIMARY KEY,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    active boolean NOT NULL DEFAULT true,
    budget_micros bigint NOT NULL CHECK (budget_micros >= 0),
    spent_micros bigint NOT NULL DEFAULT 0 CHECK (spent_micros >= 0),
    reserved_micros bigint NOT NULL DEFAULT 0 CHECK (reserved_micros >= 0),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE virtual_keys (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    key_hash bytea NOT NULL UNIQUE CHECK (octet_length(key_hash) = 32),
    key_prefix text NOT NULL,
    budget_micros bigint NOT NULL CHECK (budget_micros >= 0),
    spent_micros bigint NOT NULL DEFAULT 0 CHECK (spent_micros >= 0),
    reserved_micros bigint NOT NULL DEFAULT 0 CHECK (reserved_micros >= 0),
    expires_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id)
);

CREATE TABLE models (
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    id text NOT NULL CHECK (length(id) BETWEEN 1 AND 200),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE key_models (
    tenant_id uuid NOT NULL,
    key_id uuid NOT NULL,
    model_id text NOT NULL,
    PRIMARY KEY (tenant_id, key_id, model_id),
    FOREIGN KEY (tenant_id, key_id) REFERENCES virtual_keys(tenant_id, id),
    FOREIGN KEY (tenant_id, model_id) REFERENCES models(tenant_id, id)
);

CREATE TABLE fx_rates (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    usd_cny numeric(20,10) NOT NULL CHECK (usd_cny > 0 AND usd_cny <= 1000000),
    source text NOT NULL CHECK (length(source) BETWEEN 1 AND 200),
    effective_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, effective_at)
);
CREATE INDEX fx_rates_effective ON fx_rates(tenant_id, effective_at DESC);

CREATE TABLE endpoints (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    model_id text NOT NULL,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    upstream_model text NOT NULL CHECK (length(upstream_model) BETWEEN 1 AND 200),
    base_url text NOT NULL,
    credential_env text NOT NULL CHECK (credential_env ~ '^[A-Z][A-Z0-9_]{0,127}$'),
    allow_private boolean NOT NULL DEFAULT false,
    currency text NOT NULL CHECK (currency IN ('CNY', 'USD')),
    input_per_million numeric(24,12) NOT NULL CHECK (input_per_million >= 0),
    output_per_million numeric(24,12) NOT NULL CHECK (output_per_million >= 0),
    context_tokens integer NOT NULL CHECK (context_tokens > 0),
    max_output_tokens integer NOT NULL CHECK (max_output_tokens > 0 AND max_output_tokens <= context_tokens),
    priority integer NOT NULL DEFAULT 100 CHECK (priority >= 0),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id),
    FOREIGN KEY (tenant_id, model_id) REFERENCES models(tenant_id, id)
);

CREATE TABLE requests (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    key_id uuid NOT NULL,
    model_id text NOT NULL,
    idempotency_key text NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 200),
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    status text NOT NULL CHECK (status IN ('reserved','submitting','submission_unknown','succeeded','failed')),
    reserve_micros bigint NOT NULL CHECK (reserve_micros >= 0),
    final_micros bigint CHECK (final_micros >= 0),
    price_snapshot jsonb NOT NULL,
    result_ciphertext bytea,
    error_code text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id),
    UNIQUE (tenant_id, key_id, idempotency_key),
    FOREIGN KEY (tenant_id, key_id) REFERENCES virtual_keys(tenant_id, id),
    FOREIGN KEY (tenant_id, model_id) REFERENCES models(tenant_id, id)
);

CREATE TABLE ledger_entries (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid NOT NULL,
    request_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN ('reserve','finalize','release')),
    amount_micros bigint NOT NULL CHECK (amount_micros >= 0),
    details jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, request_id, kind),
    FOREIGN KEY (tenant_id, request_id) REFERENCES requests(tenant_id, id)
);

CREATE TABLE audit_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    action text NOT NULL,
    object_id text NOT NULL,
    details jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION prevent_fact_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'immutable fact table';
END;
$$;
CREATE TRIGGER ledger_immutable BEFORE UPDATE OR DELETE ON ledger_entries
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER fx_immutable BEFORE UPDATE OR DELETE ON fx_rates
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
CREATE TRIGGER audit_immutable BEFORE UPDATE OR DELETE ON audit_events
FOR EACH ROW EXECUTE FUNCTION prevent_fact_mutation();
