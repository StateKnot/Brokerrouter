#!/usr/bin/env python3
"""Offline release preflight with ephemeral PKI/RSA and actual immutable image metadata."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
HERE = ROOT / "deploy/production"
spec = importlib.util.spec_from_file_location("cfg", HERE / "control-plane-config.py")
cfg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cfg)


def command(*arguments, data=None):
    return subprocess.run(arguments, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)


class ReleaseTest(unittest.TestCase):
    def test_actual_release_preflight(self):
        with tempfile.TemporaryDirectory(prefix="brokerrouter-release-test-") as temporary:
            root = Path(temporary).resolve()
            os.chmod(root, 0o700)
            private = lambda: command("openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048").stdout.decode()
            pairs = {purpose: private() for purpose in ("console", "catalog", "settings", "approvals", "media-review", "human")}
            public = {purpose: command("openssl", "pkey", "-pubout", data=pem.encode()).stdout.decode() for purpose, pem in pairs.items()}
            values = {purpose: {name: "fixture-config" for name in names} for purpose, names in cfg.REQUIRED.items()}
            for purpose, entries in values.items():
                for name in entries:
                    if name.endswith(("_ORIGIN", "_ISSUER", "_S3_ENDPOINT")):
                        entries[name] = "https://identity.fixture.invalid/realm" if name.endswith("_ISSUER") else "https://storage.fixture.invalid"
                if "BROKERROUTER_RESULT_KEYS" in entries:
                    entries["BROKERROUTER_RESULT_KEYS"] = json.dumps({"result-v1": "a" * 64})
                    entries["BROKERROUTER_ACTIVE_RESULT_KEY"] = "result-v1"
            bff = values["bff"]
            bff["BROKERROUTER_BFF_ORIGIN"] = "https://console.fixture.invalid"
            bff["BROKERROUTER_BFF_MEDIA_PREVIEW_ORIGIN"] = "https://review.fixture-media.invalid"
            bff["BROKERROUTER_OIDC_ALLOWED_ORIGINS"] = '["https://identity.fixture.invalid"]'
            bff["BROKERROUTER_BFF_KEYS"] = json.dumps({"bff-v1": "b" * 64})
            bff["BROKERROUTER_BFF_ACTIVE_KEY"] = "bff-v1"
            for purpose in ("console", "catalog", "settings", "approvals", "media-review"):
                prefix = purpose.replace("-", "_").upper()
                signer = "BFF" if purpose == "console" else "BFF_" + prefix
                kid = purpose + "-v1"
                bff["BROKERROUTER_BFF_" + prefix + "_ORIGIN"] = "https://" + purpose + ".brokerrouter.internal:8443"
                bff["BROKERROUTER_" + prefix + "_AUDIENCE"] = purpose + "-audience"
                bff["BROKERROUTER_" + signer + "_SIGNING_KID"] = kid
                bff["BROKERROUTER_" + signer + "_SIGNING_PRIVATE_KEY"] = pairs[purpose]
                values[purpose].update({"BROKERROUTER_" + prefix + "_ISSUER": bff["BROKERROUTER_BFF_ORIGIN"],
                                        "BROKERROUTER_" + prefix + "_AUDIENCE": purpose + "-audience",
                                        "BROKERROUTER_" + prefix + "_PUBLIC_KEYS": json.dumps({kid: public[purpose]})})
            for purpose in ("control", "output-delivery"):
                values[purpose]["BROKERROUTER_HUMAN_PUBLIC_KEYS"] = json.dumps({"human-v1": public["human"]})
            source = root / "configuration.json"
            source.write_text(json.dumps(values))
            source.chmod(0o600)
            self.assertEqual(command("sh", str(HERE / "create-purpose-secrets.sh"), str(root)).returncode, 0)
            (root / "postgres-migration-password").write_text("c" * 64)
            (root / "postgres-migration-password").chmod(0o400)
            migration_original = (root / "postgres-migration-password").read_bytes()
            for _ in range(2):
                self.assertEqual(command("python3", str(HERE / "prepare-purpose-admin.py"), str(root)).returncode, 0)
            self.assertEqual((root / "postgres-migration-password").read_bytes(), migration_original)
            self.assertEqual(command("python3", str(HERE / "control-plane-config.py"), str(source), str(root)).returncode, 0)
            self.assertEqual(command("python3", str(HERE / "render-control-edge.py"), bff["BROKERROUTER_BFF_ORIGIN"], bff["BROKERROUTER_BFF_MEDIA_PREVIEW_ORIGIN"], str(root)).returncode, 0)
            ca_key, ca_cert = root / "ca.key", root / "bff-ca.crt"
            self.assertEqual(command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "14", "-subj", "/CN=task-only-ca", "-keyout", str(ca_key), "-out", str(ca_cert)).returncode, 0)
            ca_cert.chmod(0o444)
            for filename in ("pg-ca.crt", "asset-s3-ca.crt"):
                (root / filename).write_bytes(ca_cert.read_bytes())
                (root / filename).chmod(0o444)
            private_hosts = [purpose + ".brokerrouter.internal" for purpose in ("control", "assets", "media-api", "output-scan", "output-delivery", "console", "catalog", "settings", "approvals", "media-review")]
            for purpose, hosts in (("control-plane", private_hosts), ("console", ["console.fixture.invalid"]), ("preview", ["review.fixture-media.invalid"])):
                key, csr, certificate = root / (purpose + "-privkey.pem"), root / (purpose + ".csr"), root / (purpose + "-fullchain.pem")
                extensions = root / (purpose + ".ext")
                extensions.write_text("subjectAltName=" + ",".join("DNS:" + host for host in hosts) + "\n")
                self.assertEqual(command("openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=" + hosts[0], "-keyout", str(key), "-out", str(csr)).returncode, 0)
                self.assertEqual(command("openssl", "x509", "-req", "-days", "14", "-in", str(csr), "-CA", str(ca_cert), "-CAkey", str(ca_key), "-CAcreateserial", "-extfile", str(extensions), "-out", str(certificate)).returncode, 0)
                key.chmod(0o600)
                certificate.chmod(0o600)
            images = []
            for name in ("brokerrouter-console", "brokerrouter-preview", "brokerrouter-provisioner"):
                result = command("docker", "image", "inspect", name + ":r1-local-verification", "--format", "{{index .RepoDigests 0}}")
                self.assertEqual(result.returncode, 0)
                images.append(result.stdout.decode().strip())
            args = ["python3", str(HERE / "verify-control-plane.py"), str(root), str(root / "control-edge"), *images]
            self.assertEqual(command(*args).returncode, 0)
            print("PASS: full protected release preflight, 19 login manifest, five independent purpose RSA pairs, TLS SANs and immutable preview label", flush=True)
            print("PASS: ops administrator references preserve existing migration Secret on identical rerun", flush=True)
            path = root / "component-config/bff.env"
            good = path.read_bytes()
            for name, value in (("BROKERROUTER_BFF_KEYS", json.dumps({"bff-v1": "a" * 64})),
                                ("BROKERROUTER_BFF_APPROVALS_SIGNING_PRIVATE_KEY", pairs["console"]),
                                ("BROKERROUTER_BFF_CONSOLE_ORIGIN", "https://wrong.brokerrouter.internal:8443"),
                                ("BROKERROUTER_BFF_MEDIA_PREVIEW_ORIGIN", "https://wrong.fixture.invalid")):
                bad = dict(bff, **{name: value})
                path.write_bytes(cfg.encode("bff", bad))
                self.assertNotEqual(command(*args).returncode, 0)
                print("PASS: release rejects " + name + " trust drift", flush=True)
            path.write_bytes(good)
            self.assertNotEqual(command(*args[:-1], "brokerrouter-provisioner:r1-local-verification").returncode, 0)
            print("PASS: mutable artifact tag rejected", flush=True)
            (root / "purpose-admin.passfile").write_text("conflict")
            self.assertNotEqual(command("python3", str(HERE / "prepare-purpose-admin.py"), str(root)).returncode, 0)
            self.assertEqual((root / "purpose-admin.passfile").read_text(), "conflict")
            print("PASS: ops administrator reference conflict is not overwritten", flush=True)


if __name__ == "__main__":
    unittest.main()
