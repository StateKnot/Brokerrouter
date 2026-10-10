# Existing control-plane production composition

`compose.control-plane.yaml` overlays the unchanged core `compose.production.yaml`. It adds the 11 existing APIs, 3 existing workers, a fixed private TLS edge, and separate console/preview static images. It does not deploy a fabricated production IdP, trusted human turn ingress, private object store or security-node scanner. Those integrations require their real external configuration and acceptance before R1.3 is complete.

Prerequisites: Docker Engine/Compose **2.24.4+** (`!override`), Python **3.9+**, OpenSSL, root-owned external Linux deployment Secrets, and the current canonical migrations/`deploy/permissions.sql`. Local fixture verification may use the current host UID; a Linux application TLS master/startup process must be able to read its 0600 Secret before dropping privileges. Keep production Secret state owned by root. The ops workload runs as the specified Secret owner and has no capabilities.

## Artifacts and networks

Build without mounting deployment Secrets. The root binary image must contain the current matching migrations; an older successful readiness probe cannot certify newly changed source.

```sh
df -h .
docker build -f deploy/production/frontend.Dockerfile --target console -t brokerrouter-console:RELEASE .
docker build -f deploy/production/frontend.Dockerfile --target preview \
  --build-arg BROKERROUTER_CONSOLE_ORIGIN=https://YOUR_CONSOLE_DOMAIN \
  -t brokerrouter-preview:RELEASE .
docker build -f deploy/production/provisioner.Dockerfile -t brokerrouter-provisioner:RELEASE .
```

The frontend Dockerfile uses fixed Node 24.19.0/Nginx digests and `npm ci --ignore-scripts`; only compiled console files or the three preview files reach their final image. Preview's exact parent origin is immutable and recorded as an image label. Use immutable `repository@sha256:...` references for all three new release artifacts, with recorded build/source provenance. The ops image is fixed official PostgreSQL 17.11 on Debian 13; Python comes from repositories signed by the Debian archive keyring. Its only project payload is the provisioning helper. Record the produced image digest and installed package versions, and rebuild a controlled release when upstream security updates require it.

The backend and preview networks are internal. Each TLS edge additionally has its own ingress bridge because Docker cannot publish ports from an internal-only bridge. These ingress bridges are distinct; preview never joins backend or the BFF's networks. Port publications remain `127.0.0.1` only. Route the selected public console and separate preview domains through the site's existing controlled ingress, and route the private control/asset/media/scan/delivery names only through the intended private ingress. No scanner Docker socket or IdP admin identity is mounted into these services.

The private TLS certificate must cover these fixed SANs:

```text
control.brokerrouter.internal     assets.brokerrouter.internal
media-api.brokerrouter.internal   output-scan.brokerrouter.internal
output-delivery.brokerrouter.internal
console.brokerrouter.internal     catalog.brokerrouter.internal
settings.brokerrouter.internal    approvals.brokerrouter.internal
media-review.brokerrouter.internal
```

These names resolve to `control-tls` inside backend; the BFF's five HTTPS targets are exactly `https://<purpose>.brokerrouter.internal:8443`. They terminate TLS there and reach each separate private HTTP listener. The security node needs its own reachable HTTPS scan origin and controlled route to `output-scan`; the Docker alias alone is not remote DNS provisioning.

## External Secrets

Use an absolute canonical external root such as `/srv/brokerrouter/secrets`, mode 0700. Preserve the existing core credentials and result keyring. Run the independent generator and create dedicated ops service/passfile references without rotating the existing migration identity:

```sh
sh deploy/production/create-purpose-secrets.sh /srv/brokerrouter/secrets
python3 deploy/production/prepare-purpose-admin.py /srv/brokerrouter/secrets
```

`prepare-purpose-admin.py` reads the existing 64-hex `postgres-migration-password` in 0400/0600, then publishes `purpose-admin.service`/`purpose-admin.passfile` in 0600 under the same lock. It never replaces a conflicting reference or writes the existing password. The service selects `host=postgres`, database `brokerrouter`, user `brokerrouter_migration`, TLS `verify-full`, `/run/secrets/pg_ca`, and the separate passfile; neither the password nor SQL is on a command line. Only the one-shot ops workload receives this administrator identity.

Provide these additional files; key material remains external:

| File | Reader | Required mode |
| --- | --- | --- |
| `bff-ca.crt` | BFF additional roots / release preflight | 0444 or 0644 |
| `asset-s3-ca.crt` | asset/output readers and importer additional roots | 0444 or 0644 |
| `control-plane-fullchain.pem`, `control-plane-privkey.pem` | private TLS edge | 0600 |
| `console-fullchain.pem`, `console-privkey.pem` | console TLS edge | 0600 |
| `preview-fullchain.pem`, `preview-privkey.pem` | preview TLS edge | 0600 |

The existing `pg-ca.crt` stays public/readable after UID drop. CA files add roots for their explicit uses while preserving the existing platform trust behavior; do not substitute MCP/PG CA variables. Read [the CA contract](../../docs/r1-purpose-scoped-ca.md). Main/preview certificates need their actual public trust chains. The release preflight checks pairing, names and seven days of remaining leaf validity; browser/public-chain and renewal acceptance are additional deployment checks.

Prepare **external** JSON with all 14 purpose objects. Each object maps its explicit allowed `BROKERROUTER_*` names to nonempty UTF-8 strings. The authoritative required/optional names are `REQUIRED`/`OPTIONAL` in `control-plane-config.py`; missing, duplicate or unknown names fail. Do not create test IdP values in a production file. The generator prints no values:

```sh
python3 deploy/production/control-plane-config.py \
  /srv/brokerrouter/config/control-plane-input.json /srv/brokerrouter/secrets
python3 deploy/production/render-control-edge.py \
  https://YOUR_CONSOLE_DOMAIN https://YOUR_SEPARATE_PREVIEW_DOMAIN /srv/brokerrouter/config
```

Source JSON is 0600, directories 0700, and generated component files are 0600. Each component file uses canonical `NAME=base64(UTF8_VALUE)` lines. The runtime loader enforces the same fixed purpose whitelist, required names, duplicates, canonical base64, valid UTF-8/no NUL, 128 KiB line/1 MiB file bounds, and exact generated per-purpose TLS DATABASE_URL. Values are exported as data; there is no `eval`/shell sourcing. RSA PEM newlines, including trailing newlines, are preserved. Component files cannot set DATABASE_URL, entrypoint controls, HTTP bypass, proxy, CA-path or pool variables. Pool sizes/CA mounts are fixed by Compose. Publishing is locked and atomic; identical reruns verify, conflicting or invalid partial releases refuse replacement. For a configuration change publish a new protected release directory and restart the affected workloads deliberately.

Deploy the preview on a separate registrable domain, confirmed against the current Public Suffix List. The renderer additionally rejects same last-two DNS labels, parent/child names, noncanonical HTTPS names and ports; this is a conservative structural guard, not a full Public Suffix List implementation. Keep rendered main CSP, preview `frame-ancestors`, immutable preview parent label and BFF preview origin identical. It is not sufficient to replace only a hostname in Nginx.

| Purpose | Required external trust/configuration |
| --- | --- |
| control | HUMAN issuer/audience/public key map; existing governed-result keyring |
| console, catalog, settings | separate purpose issuer/audience/public keys; catalog connection references where used |
| approvals | independent approvals verifier; governed-result keyring |
| media-review | independent review verifier; governed-result keyring; read-only private S3 identity |
| bff | real OIDC issuer/allowed origins/client ID+Secret; independent BFF AES keyring; five independent RSA signing keys/kids/audiences and fixed HTTPS targets; exact main/preview origins |
| assets | governed-result keyring; private S3 endpoint/bucket/region and upload/read/delete identity |
| output-worker | same result/asset key history and private S3 namespace; importer identity with the needed writes/reads |
| output-scan, output-delivery | result/asset key history and private S3 read identity; delivery also receives HUMAN public verifier |
| media-api, media-worker, mcp-judge | governed-result keyring; only selected provider Secrets |

The initial explicit provider aliases are `BROKERROUTER_PROVIDER_PRIMARY_KEY` and `BROKERROUTER_PROVIDER_SECONDARY_KEY`; published endpoint `credential_env` must select the supplied alias. Expanding that deployment whitelist requires an explicit reviewed deployment change. Do not inject arbitrary provider/Agent environments into a service. S3 credentials can differ by workload IAM; endpoint/bucket/region and encryption key history must stay compatible. Define exact bucket-prefix IAM and exercise each permitted/forbidden operation against the real store. BFF AES must not reuse result AES, and five BFF purpose RSA keys must be distinct. HUMAN ingress signing remains outside these workloads; its public keys must not reuse BFF signing authority. No HUMAN private key is generated by this package.

## Fresh install or upgrade

Set references only; do not put credentials in these shell variables:

```sh
export BROKERROUTER_SECRET_DIR=/srv/brokerrouter/secrets
export BROKERROUTER_CONTROL_CONFIG_DIR=/srv/brokerrouter/config/control-edge
export BROKERROUTER_IMAGE_TAG=RELEASE
export BROKERROUTER_CONSOLE_IMAGE=REGISTRY/brokerrouter-console@sha256:IMMUTABLE_DIGEST
export BROKERROUTER_PREVIEW_IMAGE=REGISTRY/brokerrouter-preview@sha256:IMMUTABLE_DIGEST
export BROKERROUTER_PROVISIONER_IMAGE=REGISTRY/brokerrouter-provisioner@sha256:IMMUTABLE_DIGEST
export BROKERROUTER_DEPLOY_UID=0 BROKERROUTER_DEPLOY_GID=0
python3 deploy/production/verify-control-plane.py \
  "$BROKERROUTER_SECRET_DIR" "$BROKERROUTER_CONTROL_CONFIG_DIR" \
  "$BROKERROUTER_CONSOLE_IMAGE" "$BROKERROUTER_PREVIEW_IMAGE" "$BROKERROUTER_PROVISIONER_IMAGE"
docker compose -f compose.production.yaml -f compose.control-plane.yaml config --quiet
docker compose -f compose.production.yaml -f compose.control-plane.yaml up -d postgres
docker compose -f compose.production.yaml -f compose.control-plane.yaml --profile ops run --rm migrate
docker compose -f compose.production.yaml -f compose.control-plane.yaml --profile ops run --rm permissions
docker compose -f compose.production.yaml -f compose.control-plane.yaml --profile ops run --rm purpose-provisioner
docker compose -f compose.production.yaml -f compose.control-plane.yaml up -d --wait --wait-timeout 180
```

Core initialization still uses the existing script. On upgrades, back up and sequence migration/permission/registration operations; do not run concurrent migrations/operators beside live workload pools without a separate budget. Purpose registration refuses undeclared same-name LOGINs, recursive cross-use membership, critical privilege drift, password/registration conflicts, missing roles and extra direct ACLs. A failed batch rolls back. Preserve Secret/registry evidence before retrying; never repair a conflict by deleting credentials or silently granting broader groups. Read [the detailed registration contract](purpose-logins.md).

| Workload | CLI/listener | Purpose login | Pool limit |
| --- | --- | --- | --- |
| control | `serve-control` 8081 | control | 2 |
| assets | `serve-assets` 8082 | assets | 2 |
| media-api | `serve-media` 8083 | media-api | 2 |
| output-scan | `serve-output-scans` 8084 | output-scan | 2 |
| output-delivery | `serve-output-delivery` 8085 | output-delivery | 2 |
| console | `serve-console` 8086 | console | 2 |
| bff | `serve-bff` 8087 | bff | 2 |
| catalog | `serve-catalog` 8088 | catalog | 2 |
| settings | `serve-settings` 8089 | settings | 2 |
| approvals | `serve-approvals` 8090 | approvals | 2 |
| media-review | `serve-media-review` 8091 | media-review | 2 |
| media-worker | `media-worker --concurrency 4` | media-worker | 2 |
| output-worker | `output-worker --concurrency 2` | output-worker | 2 |
| mcp-judge | `mcp-judge-worker --concurrency 4` | mcp-judge | 2 |
| existing gateway | `serve` 8080 | existing runtime login | 20 |

The overlay replaces the **entire** old optional mcp-judge definition with the isolated login/config and enables this existing semantic worker. It never merges the old root mcp-judge Secret. Default active pools are `14 × 2 + 20 = 48`; PostgreSQL `max_connections=80` leaves **32 total** for reserved connections, probes, controlled operations and headroom. This is an explicit connection budget, not a load/capacity certification. Each new LOGIN's connection limit is 8. The new optional connect variable accepts 1..64; other installations retain default 20. Four MCP role foundations and reconciler are registered but no R3 services are started; add their pool/capacity/security acceptance before enabling those workloads.

All APIs expose schema-aware readiness; workers retain durable leases and their existing retry semantics. Worker shutdown budgets are 15m media/11m importer/3m semantic, matching their existing bounded task deadlines. Nginx uses root only to read its isolated TLS material and prepare bounded `/tmp` buffers, then UID101 workers; its only capabilities are CHOWN/SETGID/SETUID. Filesystems are read-only, temp buffers and resources bounded, request/body/query logs suppressed as in the existing edge contract. Readiness alone does not prove OIDC tenant membership, correct human-channel trust, scanner progress, private asset release or paid-provider recovery.

## Local acceptance and remaining integration

The following commands use only task-owned ephemeral state and never existing Docker data volumes:

```sh
df -h .
PYTHONDONTWRITEBYTECODE=1 python3 deploy/production/test-purpose-logins.py
BROKERROUTER_PROVISIONER_TEST_IMAGE=brokerrouter-provisioner:r1-local-verification \
  PYTHONDONTWRITEBYTECODE=1 python3 deploy/production/test-purpose-logins.py \
  ProductionPurposeLogins.test_isolated_ops_image_fresh_upgrade_and_conflict
PYTHONDONTWRITEBYTECODE=1 python3 deploy/production/test-control-plane-config.py
PYTHONDONTWRITEBYTECODE=1 python3 deploy/production/test-control-edge.py
PYTHONDONTWRITEBYTECODE=1 python3 deploy/production/test-control-plane-release.py
```

Image-based tests require their `r1-local-verification` artifacts built with the console parent `https://console.fixture.invalid`; the edge/config tests accept their named test image overrides. Keep logs outside build caches before cleanup. The 33 original role/script scenarios, 19 actual TLS/SCRAM logins and role/table denial matrix remain; the isolated ops image additionally exercises actual fresh/upgrade registration, recursive conflict and plaintext rejection. Actual shell tests cover all 14 purpose exports and malformed/cross-use/entry-control/duplicate/size/NUL/UTF-8/CA-path inputs. Real TLS tests use synthetic PKI and the actual final static images, verify missing/wrong CA/Host/SNI denial, fixed private routes, BFF header preservation, preview sandbox, UID/read-only behavior, missing TLS startup denial and absence of fixture tokens in logs. They do not substitute for real IdP or provider acceptance.

Next integration acceptance needs: a reachable independent IdP with interactive OIDC authorization-code/PKCE and tenant enrollment; separate authenticated HUMAN channel ingress that binds real session/message/generation before minting turn claims; real private S3 IAM; dedicated scanner node/binary, trusted official engine/signature manifest, private HTTPS scan routing and current-token revocation; approval/review/output release after restart; and workload/pool/slow-client load measurements. No Agent may hold human approval or IdP administrative signing authority. A local independent provider can certify the same OIDC contract but must be labeled local identity acceptance, not the user's production IdP.
