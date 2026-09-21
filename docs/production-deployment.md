# Production deployment

This package deploys the certified core data plane without replacing the host's existing web service. It uses a dedicated PostgreSQL 17 database, private Docker network, persistent volume, least-privilege runtime/operator logins, verified internal TLS, encrypted stored results, resource limits and a loopback-only HTTP listener. The public edge must terminate TLS and strip the deployment prefix before proxying.

It does **not** certify unfinished providers, media pipelines, the console/BFF or external identity system. Provider credentials remain absent until their models, prices and failure semantics pass the live certification checklist. Never reuse credentials pasted into chat or issue trackers.

## Server layout

- Release: `/opt/brokerrouter/releases/<commit>`
- Atomic current symlink: `/opt/brokerrouter/current`
- Secrets: `/opt/brokerrouter/secrets` (root-only; excluded from releases and Git)
- Local backups: `/var/backups/brokerrouter`
- Loopback listener: `127.0.0.1:18080`
- Public prefix: `https://stknot.com/ai-gateway/`

Create the secrets exactly once:

```sh
sudo deploy/production/generate-secrets.sh /opt/brokerrouter/secrets
```

Set the non-secret deployment values in a root-owned Compose environment file:

```sh
BROKERROUTER_SECRET_DIR=/opt/brokerrouter/secrets
BROKERROUTER_IMAGE_TAG=<immutable-git-commit>
BROKERROUTER_BIND_PORT=18080
```

Run `deploy/production/deploy.sh`, then verify both loopback and public ready probes. The Caddy snippet belongs inside the existing `stknot.com` route before its static-file fallback. Always write a dated backup, validate the candidate with `caddy validate`, reload rather than restart, and roll the file back if the external probe fails.

The public prefix root serves a minimal, unauthenticated status page so browser visits do not expose an API 404. It contains no tenant, model, usage or credential data and loads only the same-origin `/gateway.css`. Treat `GET /health/ready` as the machine readiness contract; the landing page is not a substitute for readiness monitoring.

`backup.sh` creates a PostgreSQL custom-format dump with mode `0600` and retains 14 days. This same-host backup protects against logical mistakes, not server or disk loss; production acceptance still requires a separately encrypted off-host target and a rehearsed restore. Restores must use a new isolated database first, run `brokerrouter check_schema` through `/health/ready`, and only then switch the loopback upstream.

Provider onboarding is performed through the `operator` profile and a separately injected provider Secret. Apply model/endpoint/FX changes disabled-first, run the live provider certification, and only then activate the endpoint. Restart the gateway after adding or rotating a credential reference because credentials are loaded at process start.
