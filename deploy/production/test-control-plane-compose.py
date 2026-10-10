#!/usr/bin/env python3
"""Actual Compose merge/role-secret/network/capacity contract; never starts services."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("cfg", ROOT / "deploy/production/control-plane-config.py")
cfg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cfg)


class ComposeTest(unittest.TestCase):
    def test_actual_merged_definition(self):
        environment = {name: value for name, value in os.environ.items() if not name.startswith("BROKERROUTER_")}
        environment.update({"BROKERROUTER_SECRET_DIR": "/private/tmp/brokerrouter-compose-fixture",
                            "BROKERROUTER_IMAGE_TAG": "fixture",
                            "BROKERROUTER_CONTROL_CONFIG_DIR": "/private/tmp/brokerrouter-compose-fixture/control-edge",
                            "BROKERROUTER_CONSOLE_IMAGE": "brokerrouter-console:fixture",
                            "BROKERROUTER_PREVIEW_IMAGE": "brokerrouter-preview:fixture",
                            "BROKERROUTER_PROVISIONER_IMAGE": "brokerrouter-provisioner:fixture",
                            "BROKERROUTER_DEPLOY_UID": str(os.geteuid()), "BROKERROUTER_DEPLOY_GID": str(os.getegid())})
        args = ["docker", "compose", "-f", str(ROOT / "compose.production.yaml"), "-f", str(ROOT / "compose.control-plane.yaml")]
        result = subprocess.run(args + ["config", "--format", "json"], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        self.assertEqual(result.returncode, 0, "fixed overlay failed validation")
        merged = json.loads(result.stdout)
        services = merged["services"]
        self.assertEqual(set(services), set(cfg.REQUIRED) | {"postgres", "gateway", "console-edge", "preview-edge", "control-tls"})
        budget = 0
        for purpose in cfg.REQUIRED:
            service = services[purpose]
            p = purpose.replace("-", "_")
            sources = {entry["source"] for entry in service["secrets"]}
            expected = {"pg_ca", "purpose_" + p, "config_" + p}
            if purpose == "bff":
                expected.add("bff_ca")
            if purpose in ("assets", "output-scan", "output-worker", "output-delivery", "media-review"):
                expected.add("asset_s3_ca")
            self.assertEqual(sources, expected)
            self.assertTrue(service["read_only"])
            self.assertEqual(service["environment"]["BROKERROUTER_COMPONENT"], purpose)
            self.assertEqual(service["environment"]["BROKERROUTER_DATABASE_MAX_CONNECTIONS"], "2")
            self.assertNotIn("ports", service)
            budget += 2
        self.assertNotIn("profiles", services["mcp-judge"])
        self.assertEqual(services["mcp-judge"]["entrypoint"], ["/usr/local/bin/control-plane-entrypoint"])
        self.assertNotIn("mcp_judge_env", {entry["source"] for entry in services["mcp-judge"]["secrets"]})
        self.assertEqual(services["gateway"]["environment"]["BROKERROUTER_DATABASE_MAX_CONNECTIONS"], "20")
        budget += 20
        self.assertEqual(budget, 48)
        self.assertIn("max_connections=80", services["postgres"]["command"])
        self.assertEqual(set(services["preview-edge"]["networks"]), {"preview", "preview_ingress"})
        self.assertTrue(merged["networks"]["preview"]["internal"])
        self.assertTrue(merged["networks"]["backend"]["internal"])
        for name in ("control-tls", "console-edge", "preview-edge"):
            self.assertEqual(services[name]["ports"][0]["host_ip"], "127.0.0.1")
            self.assertEqual(set(services[name]["cap_add"]), {"CHOWN", "SETGID", "SETUID"})
        print("PASS: actual default merge has 11 isolated APIs/3 workers, exact purpose Secrets, 48/80 pool budget and separate loopback TLS ingress", flush=True)
        ops = subprocess.run(args + ["--profile", "ops", "config", "--format", "json"], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        self.assertEqual(ops.returncode, 0)
        provisioner = json.loads(ops.stdout)["services"]["purpose-provisioner"]
        self.assertEqual({entry["source"] for entry in provisioner["secrets"]}, {"pg_ca", "purpose_admin_service", "purpose_admin_passfile"})
        self.assertEqual(provisioner["cap_drop"], ["ALL"])
        self.assertNotIn("cap_add", provisioner)
        self.assertEqual(provisioner["user"], str(os.geteuid()) + ":" + str(os.getegid()))
        self.assertEqual(len(provisioner["volumes"]), 2)
        self.assertTrue(any(entry["target"] == "/run/purpose-secrets/purpose-logins" and entry["read_only"] for entry in provisioner["volumes"]))
        self.assertTrue(any(entry["target"] == "/run/purpose-secrets/.purpose-logins.lock" and not entry.get("read_only", False) for entry in provisioner["volumes"]))
        self.assertNotIn("ports", provisioner)
        print("PASS: ops-only isolated provisioner mounts no runtime/operator/Agent/IdP key material", flush=True)
        for variable in ("BROKERROUTER_SECRET_DIR", "BROKERROUTER_CONSOLE_IMAGE", "BROKERROUTER_PREVIEW_IMAGE", "BROKERROUTER_PROVISIONER_IMAGE", "BROKERROUTER_DEPLOY_UID"):
            missing = environment.copy()
            del missing[variable]
            result = subprocess.run(args + ["config", "--quiet"], env=missing, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
            self.assertNotEqual(result.returncode, 0)
        print("PASS: missing mandatory release references fail before service startup", flush=True)


if __name__ == "__main__":
    unittest.main()
