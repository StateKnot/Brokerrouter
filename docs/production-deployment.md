# Production deployment

This package covers the core data plane without replacing the host's existing web service. It uses a dedicated PostgreSQL 17 database, private Docker network, persistent volume, least-privilege runtime/operator logins, verified internal TLS, encrypted stored results, resource limits and a loopback-only HTTP listener. The public edge must terminate TLS and strip the deployment prefix before proxying.

The data plane serves explicit HTTP/1.1. An HTTPS edge may accept HTTP/2 from
clients, but must forward HTTP/1.1 and disable SSE response buffering. The
production defaults allow 64 accepted sockets with an absolute 180-second
lifetime, 16 upstream operations and 4 SSE deliveries. Socket capacity includes
keepalive and blocked writes; overload is rejected before HTTP/charging.
Resource limits, alert codes and measured capacity are documented in
[operations](operations.md) and the [local deployment verifier](../deploy/verification/README.md).

It does **not** certify unfinished providers, media pipelines, the console/BFF or external identity system. Provider credentials remain absent until their models, prices and failure semantics pass the live certification checklist. Never reuse credentials pasted into chat or issue trackers.

The [2026-09-24 milestone order](milestones.md) requires each real integration to ship its service deployment during R1–R4, followed by complete production acceptance in R5. This core package is not the full product deployment: console/BFF, identity integration, approvals, media/assets/scanning and the remaining MCP/Skill services still need combined orchestration and operational evidence under [LIVE-25](acceptance-backlog.md). The milestone review did not inspect the current server.

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

The generator also creates a root-only `mcp-judge.env` for the optional semantic worker. On an existing installation, generate just this new secret with `sudo sh deploy/production/create-mcp-judge-env.sh /opt/brokerrouter/secrets`; the command refuses to replace an existing file. It copies the current runtime database URL and result keyring, but no provider credentials. After a result-key rotation, update both environment files together, retaining old keys until stored results expire. The worker uses the existing restricted `brokerrouter_runtime` database role; it has a separate process and Secret file, but not a separate database login.

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

For MCP semantic rules, add only the selected judge endpoint's `credential_env` variable to root-owned `mcp-judge.env` and provision its budget and FX version before enabling the rule. The worker is an optional, unexposed Compose profile: after building the same immutable application image and applying migrations and permissions, start it with `docker compose --file compose.production.yaml --profile mcp up --detach mcp-judge`. Confirm the worker is running, its pending evaluation count drains, and a risk result appears in the approval queue before opening MCP ingress. The core `deploy.sh` does not enable MCP. Stop the worker and disable new MCP semantic admission during rollback; retain its database evaluations, reservations, and historical keyring for reconciliation. The production Compose topology has also been exercised in an isolated local project with verified database TLS and restricted logins; target-host deployment and full MCP service certification remain open.

Do not roll back to a gateway without the output recheck and socket/H1 limits
while exposing the affected routes. If the new release fails, isolate those
routes, drain/reconcile accepted operations and preserve database facts and
the historical decryption keyring before changing images. An old image
reintroducing the reported defects is not a safe rollback.
