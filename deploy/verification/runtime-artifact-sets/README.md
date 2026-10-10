# Schema32 → 33 core upgrade acceptance

`upgrade.py` verifies the fixed schema32/schema33 release pair using a fresh Docker project. The repository launcher independently passed ten actual groups in `artifact-phase-c-upgrade-run7` (180.89 seconds, exit0), after the equivalent private launcher passed run6. Both runs used distinct fresh projects and verified cleanup. The executed launcher and all four reused fixture implementations retain their exact tested bytes. This README and the descriptive status/README digest in `inputs.json` were updated after run7; its original input and source digests remain in the [scope evidence](../../../docs/evidence/runtime-artifact-c-2026-10-11.json). This certifies the stated local core/schema/roles upgrade scope, not complete production or Hermes acceptance.

Only this launcher, `inputs.json` and this README belong in the repository. Do not add source snapshots, generated certificates, credentials, state directories or Docker data. The launcher imports the existing `final-publications/acceptance.py`, `native_acceptance.py` and `identity/acceptance.py` and mounts the existing `final-publications/provider.js`. It checks all four exact source digests before using them. No fixture implementation is duplicated. It borrows NativeFixture's strict CA/leaf and schema bootstrap methods; it does not invoke Hermes, its SDK, a browser or an IdP.

The existing schema32 image is `sha256:f91fefbc1b5dec597983bc26cd5ad6fdc2493fda737075a7bffcedfce719efdc`; schema33 is `sha256:10d188d816b29cb0f9228dfa2b695d6a9099fa9351034351d99e5385426fa396`. The twenty-purpose provisioner is `sha256:2483917b35fd9b6941d2a3b45a95659ec443d96770cc47201d9b7e347ee42847`. All images, including the fixed PG/Node/NGINX/MinIO fixtures, must exist locally under their immutable references. The script performs no pulls, builds or Cargo commands. The supplied external old/new deployment and compiled-input manifests must match the safety digests in `inputs.json`, and every listed source file and already-applied migration checksum must match. External source roots contain source only, never caches or Secrets.

```sh
python3 deploy/verification/runtime-artifact-sets/upgrade.py \
  --old-source "$UPGRADE_SCHEMA32_SOURCE" \
  --old-source-manifest "$UPGRADE_SCHEMA32_DEPLOY_MANIFEST" \
  --old-core-input-manifest "$UPGRADE_SCHEMA32_COMPILED_MANIFEST" \
  --image sha256:10d188d816b29cb0f9228dfa2b695d6a9099fa9351034351d99e5385426fa396 \
  --source "$UPGRADE_SCHEMA33_SOURCE" \
  --source-manifest "$UPGRADE_SCHEMA33_DEPLOY_MANIFEST" \
  --core-input-manifest "$UPGRADE_SCHEMA33_COMPILED_MANIFEST" \
  --evidence-dir "$UPGRADE_FRESH_EVIDENCE" \
  --preflight-only
```

Set these task-specific paths explicitly. The fixture root defaults to this repository and can be supplied with `--fixture-source`. Source preflight starts no Docker resources. Run the same command without `--preflight-only` for actual acceptance, using a new, nonexistent evidence directory.

The actual phases require all of the following:

- Separate strict TLS CAs and signed Human/approval purposes, schema32 migration plus matching canonical permissions, and independently provisioned twenty-purpose LOGINs. Three different native sessions have three live owner turns, each recording real Tool and Final execution/source facts. The Final source UUID comes from the real zero-cost provider HTTP roundtrip's gateway header. Approved and pending cases use the real full-text workbench. The first source/result expires naturally through the server's60-second TTL and PostgreSQL clock; immutable data and triggers are never altered to fake expiry.
- The schema33 binary must exit unsuccessfully on schema32. Actual migration33, matching permissions, repeated migration/permissions and repeated purpose provisioning must preserve ten table digests, including turn, ciphertext, source, hash, approval, review, grant, event, ledger and expiry facts. LOGIN verifier digests must remain unchanged and historical artifact-set/hash pairs remainNULL. Three historical turns remain active; ready/pending JSON stays exact and expired Tool/Final releases return410 `result_unavailable` without text. Repeated records return the same metadata and provider calls stay3.
- All twenty registered LOGINs actually authenticate through verify-full PostgreSQL TLS. A separate RuntimeAssets HTTPS service and task-only TLS MinIO reserve, upload, hash and seal two quarantined candidates. Ordinary Runtime derives the complete two-file set and returns `waiting_artifacts` / `checking_scan` without text. Narrow source/asset metadata reads succeed as the intended purposes; Runtime raw native metadata remains0. Cipher/chunk/producer/grant/evaluation escalation checks require SQLSTATE42501, not an arbitrary error.
- Service logs contain no generated credentials, signed assertions, request input or provider/final plaintext. Only safe metadata and digests are recorded. The fresh project uses tmpfs PG/S3 storage and task labels; short-lived probes have resource limits. Cleanup removes only that project's containers/network and its private Secret directory, then verifies their absence. There are no shared volumes, sockets, databases, Keycloak resources or global prune operations.

This is core/schema/roles upgrade acceptance. The two small byte candidates do not certify playable MP4, producer execution, completed scanning, exact file grants, binary delivery, Hermes/native publication or external IdP authentication. Those remain separate acceptance scopes.

The historical schema32 deployment manifest also lists three old PKI scripts: `generate-secrets.sh`, `verify-control-plane.py` and `test-control-plane-release.py`. Their old digests differ from today's corrected scripts, but neither launcher executes any of them. They participate only in source provenance checks. Actual CA/leaf generation uses the frozen NativeFixture methods with critical CA keyCertSign/cRLSign, leaf digitalSignature/keyEncipherment, serverAuth, SKI/AKI and SAN. It does not disable certificate, hostname or purpose verification. The new deployment source needs only the105 compiled inputs plus six matching deployment helpers; it does not import those old PKI methods.
