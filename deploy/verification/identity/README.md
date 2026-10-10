# Independent local OIDC and own-approval acceptance

This is a reproducible integration test, not an IdP emulator or a production identity enrollment shortcut. It starts an official immutable Keycloak image in optimized production mode, its own TLS/SCRAM PostgreSQL, a separate Brokerrouter PostgreSQL, gateway/control/Console/approvals/BFF, and the deployed Console TLS edge. Three imported accounts are explicitly **synthetic**. It never operates an existing IdP, application container, network or data volume.

The production deployment remains `compose.production.yaml` plus `compose.control-plane.yaml`. This test intentionally enables only the identity and approval path; absent media, private storage, scanner and delivery services are not certified by it. The existing core root-managed entrypoint loads the fixture's generated, shell-quoted environment files. Production full control-plane configuration continues to use its verified allowlisted base64 loader. There is no Agent-side HUMAN or IdP administrator key.

## Prerequisites and reproducible images

Run `df -h .` first and follow the workspace cache/space rules. Required host tools: Python 3.9+, Docker Compose, OpenSSL. Provide an immutable release image and the **matching immutable source snapshot** for canonical grants and purpose-login helpers. A future migration in a live checkout must never be applied to an older release binary. The actual schema-aware health checks enforce this too.

The browser Dockerfile uses the official fixed Node 24.19.0 image, the CI lockfile (Playwright 1.63.0), actual Chromium and Linux NSS. The official Node bundled public roots bootstrap Debian ca-certificates over verified HTTPS; APT signature checks remain enabled. Prepare a tiny source-only build context; never copy an existing `node_modules`. The existing Linux npm-ci cache can still be reused as a read-only runtime bind. Build both test images without credentials:

```sh
docker build -f deploy/verification/identity/Keycloak.Dockerfile \
  -t brokerrouter-local-keycloak:optimized deploy/verification/identity

mkdir -p /absolute/task/identity-browser-source
cp console/package.json console/package-lock.json /absolute/task/identity-browser-source/
cp deploy/verification/identity/Browser.Dockerfile /absolute/task/identity-browser-source/Dockerfile
docker build -t brokerrouter-local-identity-browser:verified /absolute/task/identity-browser-source
```

The Keycloak source is the official 26.8.0 content digest in its Dockerfile; it runs `kc.sh build` without secrets. The browser downloads the locked Playwright Chromium build and uses signed Debian repositories for its native libraries and NSS tool. Use each final immutable image ID or RepoDigest below, never a mutable tag. Console and provisioner images come from their verified production Dockerfiles. No application dependency is added.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 deploy/verification/identity/acceptance.py \
  --image sha256:YOUR_RELEASE_IMAGE_ID \
  --source /absolute/path/to/matching/source-snapshot \
  --node-modules /absolute/path/to/existing/linux/console/node_modules \
  --evidence-dir /absolute/path/outside/repository/evidence \
  --keycloak-image sha256:YOUR_OPTIMIZED_KEYCLOAK_IMAGE_ID \
  --browser-image sha256:YOUR_VERIFIED_BROWSER_IMAGE_ID \
  --console-image sha256:YOUR_VERIFIED_CONSOLE_IMAGE_ID \
  --provisioner-image sha256:YOUR_VERIFIED_PROVISIONER_IMAGE_ID
```

The pinned official PostgreSQL and Nginx images are in the script. It validates the exact Playwright version and immutable references, chooses a fresh random project namespace, and refuses to reuse an existing project. All DB data is tmpfs. Secret directories are outside the repository, mode 0700, files 0600. Actual container bind ownership is measured for the one-shot purpose provisioner; Docker Desktop can map files to UID0 while a Linux host retains the host UID. This affects only the administrator provisioning container; all workload processes still drop to their own application UID.

Keycloak's start shim copies only its individual configuration/import/TLS files to bounded tmpfs, then `chroot --userspec=1000:0 /` executes Keycloak. The live provider is UID1000 with a dedicated non-superuser DB LOGIN and pool minimum/initial 2, maximum 4, connection limit 10. It has no core migration credential. Its hostname is fixed HTTPS, HTTP disabled, confidential code client has exact callback URI and mandatory S256. Explicit default client scopes include `basic`, required for immutable `sub` and `auth_time`; optional scopes are empty, so offline access is not enabled. Browser trust uses a task-only public CA in NSS and Node's explicit additional root; it never disables hostname or certificate verification.

## Identity and verified behavior

The BFF governance user issuer equals `https://console.fixture.invalid`; its operator-managed binding uses the actual Keycloak realm issuer and immutable imported user UUID. The trusted fixture HUMAN controller uses that same internal issuer/subject but a **different RSA key, JWT type and audience**. Its private key remains on the host and never enters the browser, BFF, gateway or Agent. This certifies the existing request-bound HUMAN contract; the missing production OIDC-to-Hermes authenticated channel ingress remains a separate implementation.

The test performs real interactive OIDC from the deployed Console and verifies code/PKCE/state/nonce, fresh authentication and immutable owner mapping. It checks unbound-user and cross-tenant login denial; another bound user's empty inbox and review/decision 404; CSRF/wrong-origin/Console-permission 403; opaque Secure HttpOnly cookies and no upstream tokens in browser storage; consumed callback replay; real source review, version selection, scope acknowledgement and two-step owner confirmation; original runtime re-check readiness; logout and binding revocation; and rejection of approvals-purpose signatures as HUMAN assertions and actual Agent keys as approvals credentials. It also verifies actual five workload LOGIN pools <=2 and TLS, IdP UID/role/pool, one owner decision/grant, and zero tool/provider requests or ledger entries.

Service logs are checked for generated credentials, HUMAN assertions, virtual keys and OAuth codes/tokens before export. No browser trace, HAR, screenshot, request body or token is exported. Normal completion and failure remove only this fixture's containers/networks/generated credentials. `--keep` is for diagnosis only: it leaves a protected private checkpoint; `--resume /private/tmp/brokerrouter-identity-secrets-...` reuses only that task checkpoint. After diagnosis, a fresh run supplies the final evidence. Source, public test image metadata and logs survive cleanup.

Official primary contracts: [Keycloak optimized container startup](https://www.keycloak.org/server/containers), [hostname configuration](https://www.keycloak.org/server/hostname), [realm import](https://www.keycloak.org/server/importExport). Local image `--help` evidence confirms the actual fixed version's configuration-file interface. A green local result does not claim the user's production IdP, DNS/certificate operations, scanner/vendor certification or workload capacity is accepted.

[Keycloak primary migration note for basic scope and sub/auth_time](https://github.com/keycloak/keycloak/blob/main/docs/documentation/upgrading/topics/changes/changes-25_0_0.adoc) explains why clearing optional scopes must also keep the required default basic scope. Missing required identity claims fail closed in the BFF.
