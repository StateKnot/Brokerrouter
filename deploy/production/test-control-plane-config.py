#!/usr/bin/env python3
"""Actual container-shell tests for purpose-scoped configuration, without production state."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("cfg", ROOT / "deploy/production/control-plane-config.py")
cfg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cfg)


def run(*args, **kwargs):
    return subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=30, **kwargs)


class ConfigurationTest(unittest.TestCase):
    def test_actual_shell_and_offline_publishing(self):
        with tempfile.TemporaryDirectory(prefix="brokerrouter-config-test-") as directory:
            root = Path(directory).resolve()
            os.chmod(root, 0o700)
            files = root / "secrets"
            files.mkdir(mode=0o700)
            values = {purpose: {name: "synthetic 'data'\n$(touch /tmp/EXECUTED)\n\n"
                               for name in names} for purpose, names in cfg.REQUIRED.items()}
            for purpose, entries in values.items():
                for name in entries:
                    if name.endswith(("_ORIGIN", "_ISSUER", "_S3_ENDPOINT")):
                        entries[name] = "https://identity.fixture.invalid" if name.endswith("_ISSUER") else "https://backend.fixture.invalid"
            source = root / "config.json"
            source.write_text(json.dumps(values))
            os.chmod(source, 0o600)
            command = ["python3", str(ROOT / "deploy/production/control-plane-config.py"), str(source), str(root)]
            self.assertEqual(run(*command).returncode, 0)
            snapshot = {entry.name: entry.read_bytes() for entry in (root / "component-config").iterdir()}
            self.assertEqual(run(*command).returncode, 0)
            self.assertEqual(snapshot, {entry.name: entry.read_bytes() for entry in (root / "component-config").iterdir()})
            print("PASS: complete offline publishing and identical rerun without overwrites", flush=True)
            bad = dict(values)
            bad["console"] = dict(bad["console"], DATABASE_URL="must-never-override")
            source.write_text(json.dumps(bad))
            self.assertNotEqual(run(*command).returncode, 0)
            self.assertEqual(snapshot, {entry.name: entry.read_bytes() for entry in (root / "component-config").iterdir()})
            print("PASS: unknown cross-purpose/database configuration fails without changing release", flush=True)
            source.write_text(json.dumps(values))
            (root / "component-config" / "console.env").write_text("wrong")
            self.assertNotEqual(run(*command).returncode, 0)
            print("PASS: an existing conflicting release is never repaired or overwritten", flush=True)
            # Actual Linux /bin/sh/base64/iconv/export semantics, using the production runtime image.
            stub = root / "stub.sh"
            stub.write_text("#!/bin/sh\nset -eu\n[ -z \"${BROKERROUTER_ENV_FILE:-}\" ]\n[ ! -e /tmp/EXECUTED ]\n. /fixtures/assertions.sh\necho verified\n")
            os.chmod(stub, 0o755)
            assertion = root / "assertions.sh"
            assertion.write_text("exit 99\n")
            container = "brokerrouter-config-test-" + uuid.uuid4().hex[:12]
            try:
                result = run("docker", "run", "--detach", "--name", container, "--network", "none", "--read-only",
                             "--memory", "64m", "--cpus", "0.5", "--pids-limit", "32", "--tmpfs", "/tmp:size=8m,mode=1777",
                             "--mount", "type=bind,src=" + str(files) + ",dst=/run/secrets,readonly",
                             "--mount", "type=bind,src=" + str(root) + ",dst=/fixtures,readonly",
                             "--mount", "type=bind,src=" + str(stub) + ",dst=/usr/local/bin/container-entrypoint,readonly",
                             "--mount", "type=bind,src=" + str(ROOT / "deploy/production/control-plane-entrypoint.sh") + ",dst=/loader,readonly",
                             "--entrypoint", "sleep", os.environ.get("BROKERROUTER_CONFIG_TEST_IMAGE", "brokerrouter:a633eef"), "180")
                self.assertEqual(result.returncode, 0, "task-only runtime fixture failed to start")
                def load(purpose, body, expected=True):
                    role = "brokerrouter_" + purpose.replace("-", "_") + "_login"
                    (files / "purpose_env").write_text("DATABASE_URL='postgres://" + role + ":" + "a" * 64 + "@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca'\n")
                    (files / "component_config").write_bytes(body)
                    result = run("docker", "exec", "--env", "BROKERROUTER_COMPONENT=" + purpose,
                                 "--env", "BROKERROUTER_ENV_FILE=/must-not-be-sourced", container, "sh", "/loader")
                    if expected:
                        self.assertEqual((result.returncode, result.stdout), (0, "verified\n"), result.stderr)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertNotIn("synthetic", result.stderr)
                for purpose, entries in values.items():
                    assertion.write_text("\n".join("[ \"$(printf '%s' \"$" + name + "\" | sha256sum | cut -d' ' -f1)\" = " + hashlib.sha256(value.encode()).hexdigest() + " ]" for name, value in entries.items()) + "\n")
                    load(purpose, cfg.encode(purpose, entries))
                print("PASS: all 14 purpose loaders preserve UTF-8 multiline/trailing-newline values and never execute shell text", flush=True)
                runtime = dict(values["assets"])
                runtime["BROKERROUTER_RUNTIME_ARTIFACT_TTL_SECONDS"] = "86400"
                assertion.write_text("\n".join("[ \"$(printf '%s' \"$" + name + "\" | sha256sum | cut -d' ' -f1)\" = " + hashlib.sha256(value.encode()).hexdigest() + " ]" for name, value in runtime.items()) + "\n")
                load("runtime-assets", cfg.encode("runtime-assets", runtime))
                for forbidden in ("BROKERROUTER_HUMAN_PUBLIC_KEYS", "BROKERROUTER_PROVIDER_PRIMARY_KEY", "BROKERROUTER_ASSET_S3_CA_FILE"):
                    load("runtime-assets", cfg.encode("runtime-assets", runtime) + forbidden.encode() + b"=c3ludGhldGlj\n", False)
                print("PASS: independent runtime-assets loader permits only S3/result/TTL and rejects approval/provider/CA override material", flush=True)
                purpose = "bff"
                assertion.write_text(":\n")
                good = cfg.encode(purpose, values[purpose])
                name = good.splitlines()[0].split(b"=", 1)[0].decode()
                cases = {
                    "unknown variable": good + b"DATABASE_URL=c3ludGhldGlj\n",
                    "entry control override": good + b"BROKERROUTER_ENV_FILE=c3ludGhldGlj\n",
                    "cross-purpose CA": good + b"BROKERROUTER_MCP_CA_FILE=L3ByaXZhdGU=\n",
                    "duplicate": good + good.splitlines(keepends=True)[0],
                    "missing required": b"\n".join(good.splitlines()[1:]) + b"\n",
                    "blank line": good + b"\n",
                    "invalid alphabet": good + name.encode() + b"=%%%\n",
                    "noncanonical padding bits": good.replace(good.splitlines()[0], name.encode() + b"=Zh=="),
                    "NUL value": good.replace(good.splitlines()[0], name.encode() + b"=AA=="),
                    "invalid UTF8": good.replace(good.splitlines()[0], name.encode() + b"=/w=="),
                    "empty value": good.replace(good.splitlines()[0], name.encode() + b"="),
                    "oversized line": good + name.encode() + b"=" + b"A" * cfg.LINE_LIMIT + b"\n",
                    "oversized file": b"A" * (cfg.LIMIT + 1),
                }
                for case, body in cases.items():
                    load(purpose, body, False)
                    print("PASS: loader rejects " + case, flush=True)
                (files / "purpose_env").write_text("DATABASE_URL='postgres://different:password@postgres/db'\n")
                (files / "component_config").write_bytes(good)
                result = run("docker", "exec", "--env", "BROKERROUTER_COMPONENT=bff", container, "sh", "/loader")
                self.assertNotEqual(result.returncode, 0)
                print("PASS: wrong purpose/TLS DSN is rejected before exports", flush=True)
                (files / "component_config").unlink()
                (files / "component_config").symlink_to("purpose_env")
                result = run("docker", "exec", "--env", "BROKERROUTER_COMPONENT=bff", container, "sh", "/loader")
                self.assertNotEqual(result.returncode, 0)
                print("PASS: configuration symlink is rejected", flush=True)
            finally:
                run("docker", "rm", "--force", container)


if __name__ == "__main__":
    unittest.main()
