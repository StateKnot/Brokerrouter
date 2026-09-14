-- Login bindings are operator-provisioned, never derived from email or browser roles.
ALTER TABLE tenants ADD COLUMN bff_epoch bigint NOT NULL DEFAULT 1 CHECK(bff_epoch>0);
ALTER TABLE governance_users ADD COLUMN bff_epoch bigint NOT NULL DEFAULT 1 CHECK(bff_epoch>0);
CREATE FUNCTION bff_bump_epoch() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog AS $$
BEGIN
    IF OLD.active IS DISTINCT FROM NEW.active THEN NEW.bff_epoch := OLD.bff_epoch+1; END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER bff_tenant_epoch BEFORE UPDATE OF active ON tenants FOR EACH ROW EXECUTE FUNCTION bff_bump_epoch();
CREATE TRIGGER bff_user_epoch BEFORE UPDATE OF active ON governance_users FOR EACH ROW EXECUTE FUNCTION bff_bump_epoch();
CREATE TABLE bff_identity_bindings (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    user_id uuid NOT NULL,
    oidc_issuer text NOT NULL CHECK (length(oidc_issuer) BETWEEN 1 AND 500),
    oidc_subject text NOT NULL CHECK (length(oidc_subject) BETWEEN 1 AND 500),
    active boolean NOT NULL DEFAULT true,
    generation bigint NOT NULL DEFAULT 1 CHECK (generation > 0),
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, oidc_issuer, oidc_subject),
    FOREIGN KEY (tenant_id, user_id) REFERENCES governance_users(tenant_id, id)
);
CREATE TABLE bff_logins (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants(id),
    cookie_hash bytea NOT NULL UNIQUE CHECK (octet_length(cookie_hash)=32),
    state_hash bytea NOT NULL UNIQUE CHECK (octet_length(state_hash)=32),
    config_hash bytea NOT NULL CHECK (octet_length(config_hash)=32),
    payload bytea NOT NULL CHECK (octet_length(payload) <= 16384),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    CHECK (expires_at > created_at AND expires_at <= created_at + interval '5 minutes')
);
CREATE INDEX bff_logins_expiry ON bff_logins(expires_at);
CREATE TABLE bff_sessions (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    binding_generation bigint NOT NULL,
    user_epoch bigint NOT NULL,
    tenant_epoch bigint NOT NULL,
    token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash)=32),
    csrf_hash bytea NOT NULL CHECK (octet_length(csrf_hash)=32),
    config_hash bytea NOT NULL CHECK (octet_length(config_hash)=32),
    auth_time timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    FOREIGN KEY (tenant_id,binding_id) REFERENCES bff_identity_bindings(tenant_id,id),
    CHECK (expires_at > auth_time AND expires_at <= auth_time + interval '1 hour')
);
CREATE INDEX bff_sessions_expiry ON bff_sessions(expires_at);
CREATE INDEX bff_sessions_binding ON bff_sessions(tenant_id,binding_id);
