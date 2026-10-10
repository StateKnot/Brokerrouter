# Local production deployment verification

This overlay runs the production image, TLS PostgreSQL, restricted database
logins, encrypted results and MCP judge process against an internal zero-cost
provider. Use a dedicated project and freshly generated local secrets. It is
never an overlay for a running production project.

From the repository root, choose a new image tag and an empty absolute secret
directory outside the checkout. The generator refuses to overwrite secrets.
Docker and Python 3.9+ are required; the application image is built with the
repository's pinned Rust toolchain and base images.

```sh
export BROKERROUTER_IMAGE_TAG=local-verification-<source-commit>
export BROKERROUTER_SECRET_DIR=/private/tmp/brokerrouter-verification-secrets
export BROKERROUTER_BIND_PORT=28080
export COMPOSE_PROJECT_NAME=brokerrouter-local-acceptance
export COMPOSE_FILE=compose.production.yaml:compose.verification.yaml

mkdir -m 700 "$BROKERROUTER_SECRET_DIR"
docker build --tag "brokerrouter:$BROKERROUTER_IMAGE_TAG" .
docker run --rm --user 0 --entrypoint sh \
  --mount "type=bind,source=$BROKERROUTER_SECRET_DIR,target=/secrets" \
  --mount "type=bind,source=$PWD/deploy/production,target=/scripts,readonly" \
  "brokerrouter:$BROKERROUTER_IMAGE_TAG" /scripts/generate-secrets.sh /secrets
docker compose up --detach postgres verification-provider
docker compose --profile ops run --rm migrate
docker compose --profile ops run --rm permissions
docker compose --profile mcp up --detach gateway mcp-judge

python3 deploy/verification/probe.py \
  --report /private/tmp/brokerrouter-deployment.json
python3 deploy/verification/probe.py --deadline-only \
  --report /private/tmp/brokerrouter-deadline.json
```

The probe checks UID 10001, runtime database TLS, unauthenticated access,
explicit HTTP/1.1 transport and rejection of HTTP/2 negotiation,
nonstream/SSE replay, service restart, large frames, disconnected-client
settlement, and 20 waves of four unread clients. It checks accepted provider
calls against successful requests, exactly one finalization per request, no
remaining reservation, and tenant spend against the ledger. Defaults fix the
TCP ceiling at 64 and process RSS budget at 384 MiB before the run. The second
run checks idle and incomplete-header connections using the actual deployed
180-second lifetime. JSON evidence contains counts/timings, never virtual keys
or response payloads. Synthetic tenant/model/ledger facts remain in this
isolated database for inspection; the probe does not clean another project.

The production image uses Debian's signed official package sources, with bounded
APT retries/timeouts. A local mirror outage must not silently produce an image
without required runtime packages.

Run all existing quality workflows locally as well: Rust fmt/Clippy/full tests,
the explicitly invoked S3 and real ClamAV/FFmpeg contracts, Console's pinned
Node version/build/unit/Chromium/WebKit/Nginx checks, and Hermes's pinned Python
dependencies and fixed source overlay contract. Keep their raw logs outside
Git and record the image digest/source in the acceptance report. Browser
containers need the sibling `preview` directory, OpenSSL, adequate shared
memory (1 GiB for this verification), and available memory; avoid building
the release image concurrently with browser tests on a small Docker VM.

Local checks permit continued development when GitHub Actions cannot obtain a
runner. They do not establish GitHub branch protection, a paid provider's live
contract, a complete native Hermes session, or target-host capacity. Those
acceptance items remain separate.

The gateway data plane serves HTTP/1.1. The HTTPS edge may terminate HTTP/2 for
clients and proxy HTTP/1.1 with SSE buffering disabled. Native HTTP/2 streams
are deliberately not admitted under the per-socket capacity budget.

To remove only the verification services while preserving evidence/data:

```sh
docker compose --profile mcp --profile ops down
```

Remove the dedicated volume/secrets only after any required recovery exercise
and evidence retention. Never pass `--volumes` against another project.
