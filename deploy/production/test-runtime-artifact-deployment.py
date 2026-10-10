#!/usr/bin/env python3
"""Immutable 19→20 Secret release and optional private capture deployment tests."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HERE=Path(__file__).resolve().parent
REPO=HERE.parents[1]
spec=importlib.util.spec_from_file_location("purpose_helper",HERE/"purpose-logins.py")
helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)

class ArtifactPurposeUpgrade(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory(prefix="brokerrouter-artifact-purpose-")
        self.addCleanup(self.tmp.cleanup)
        self.root=Path(self.tmp.name).resolve();self.root.chmod(0o700)
    def legacy(self):
        target=self.root/"purpose-logins";target.mkdir(mode=0o700)
        entries=[]
        for purpose,group in helper.GROUPS.items():
            if purpose=="runtime-assets":continue
            password=os.urandom(32).hex().encode()
            env=helper.env_content(purpose,password);scram=helper.scram(password)
            for suffix,body in [("password",password),("env",env),("scram",scram.encode())]:
                path=target/(purpose+"."+suffix);path.write_bytes(body);path.chmod(0o600)
            entries.append({"purpose":purpose,"group":group,"login":helper.LOGINS[purpose],"password_sha256":helper.digest(password),"env_sha256":helper.digest(env),"scram":scram})
        manifest=target/"manifest.json";manifest.write_text(json.dumps({"version":1,"roles":entries},sort_keys=True,indent=2)+"\n");manifest.chmod(0o600)
        self.assertEqual(len(entries),19)
        return target
    def snapshot(self,root):return {p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in root.iterdir()}
    def call(self,*arguments,success=True):
        r=subprocess.run([sys.executable,str(HERE/"purpose-logins.py"),*arguments],stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=10)
        self.assertEqual(r.returncode==0,success,"provisioning command returned unexpected safe status")
        return r
    def test_explicit_upgrade_preserves_all_19_bytes_and_is_idempotent(self):
        legacy=self.legacy();before=self.snapshot(legacy)
        self.call("create",str(self.root));self.assertEqual(self.snapshot(legacy),before)
        self.call("upgrade-runtime-artifacts",str(self.root))
        release=self.root/"purpose-logins-v2";new=helper.validate(release)
        self.assertEqual(new["version"],2);self.assertEqual(len(new["roles"]),20)
        self.assertEqual(self.snapshot(legacy),before)
        for name in before:
            if name!="manifest.json":self.assertEqual((release/name).read_bytes(),(legacy/name).read_bytes())
        prior=json.loads((legacy/"manifest.json").read_text())
        self.assertEqual([e for e in new["roles"] if e["purpose"]!="runtime-assets"],prior["roles"])
        self.assertEqual(helper.select_manifest(self.root,False)["roles"],prior["roles"])
        self.assertEqual(helper.select_manifest(self.root,True),new)
        saved=self.snapshot(release);self.call("upgrade-runtime-artifacts",str(self.root));self.assertEqual(self.snapshot(release),saved)
        self.call("create",str(self.root));self.assertEqual(self.snapshot(legacy),before)
    def test_real_sigkill_new_secret_resumes_without_regenerating_or_mutating_old_release(self):
        legacy=self.legacy();before=self.snapshot(legacy)
        code='''import importlib.util,os,signal,sys\nfrom pathlib import Path\ns=importlib.util.spec_from_file_location("helper",sys.argv[1]);m=importlib.util.module_from_spec(s);s.loader.exec_module(m)\noriginal=m.atomic_new\ndef fault(path,data):\n original(path,data)\n if path.name=="runtime-assets.password":os.kill(os.getpid(),signal.SIGKILL)\nm.atomic_new=fault\nwith m.locked_root(Path(sys.argv[2])) as root:m.upgrade_runtime_artifacts(root)\n'''
        child=subprocess.run([sys.executable,"-c",code,str(HERE/"purpose-logins.py"),str(self.root)],stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=10)
        self.assertEqual(child.returncode,-9)
        password=(self.root/".purpose-logins-v2.pending/runtime-assets.password").read_bytes()
        self.assertFalse((self.root/"purpose-logins-v2").exists())
        self.assertEqual(self.snapshot(legacy),before)
        self.call("upgrade-runtime-artifacts",str(self.root))
        self.assertEqual((self.root/"purpose-logins-v2/runtime-assets.password").read_bytes(),password)
        self.assertEqual(self.snapshot(legacy),before)
        helper.select_manifest(self.root,True)
    def test_partial_or_tampered_upgrade_is_not_adopted(self):
        legacy=self.legacy();self.call("upgrade-runtime-artifacts",str(self.root))
        release=self.root/"purpose-logins-v2";before=self.snapshot(legacy)
        password=release/"console.password";password.write_bytes(b"9"*64)
        self.call("upgrade-runtime-artifacts",str(self.root),success=False)
        with self.assertRaises(ValueError):helper.select_manifest(self.root,True)
        self.assertEqual(self.snapshot(legacy),before)
    def test_complete_legacy_pending_release_recovers_19_without_regenerating(self):
        legacy=self.legacy();before=self.snapshot(legacy)
        legacy.rename(self.root/".purpose-logins.pending")
        self.call("create",str(self.root))
        self.assertEqual(self.snapshot(self.root/"purpose-logins"),before)
        self.assertEqual(len(helper.select_manifest(self.root,False)["roles"]),19)
    def test_published_upgrade_with_partial_state_cannot_be_applied(self):
        self.legacy();self.call("upgrade-runtime-artifacts",str(self.root))
        (self.root/".purpose-logins-v2.pending").mkdir(mode=0o700)
        with self.assertRaises(ValueError):helper.select_manifest(self.root,True)

    def test_fresh_version2_default_uses_exact_20_set(self):
        self.call("create",str(self.root));manifest=helper.select_manifest(self.root,False)
        self.assertEqual(manifest["version"],2);self.assertEqual(len(manifest["roles"]),20)
        self.assertEqual(helper.select_manifest(self.root,True),manifest)
        self.call("upgrade-runtime-artifacts",str(self.root))
        self.assertFalse((self.root/"purpose-logins-v2").exists())

class OptionalArtifactDeployment(unittest.TestCase):
    def test_actual_compose_profile_is_separate_and_default_service_budget_unchanged(self):
        env = {name:value for name,value in os.environ.items() if not name.startswith("BROKERROUTER_")}
        env.update({"BROKERROUTER_SECRET_DIR":"/private/tmp/artifact-compose-fixture", "BROKERROUTER_IMAGE_TAG":"fixture",
            "BROKERROUTER_CONTROL_CONFIG_DIR":"/private/tmp/artifact-compose-fixture/control-edge",
            "BROKERROUTER_CONSOLE_IMAGE":"brokerrouter-console:fixture", "BROKERROUTER_PREVIEW_IMAGE":"brokerrouter-preview:fixture",
            "BROKERROUTER_PROVISIONER_IMAGE":"brokerrouter-provisioner:fixture", "BROKERROUTER_DEPLOY_UID":str(os.geteuid()),"BROKERROUTER_DEPLOY_GID":str(os.getegid()),
            "BROKERROUTER_RUNTIME_ARTIFACT_PURPOSE_FILE":"/private/tmp/fixture/purpose-logins-v2/runtime-assets.env",
            "BROKERROUTER_RUNTIME_ARTIFACT_CONFIG_FILE":"/private/tmp/fixture/runtime-config/component-config/runtime-assets.env"})
        base=["docker","compose","-f",str(REPO/"compose.production.yaml"),"-f",str(REPO/"compose.control-plane.yaml")]
        extra=base+["-f",str(REPO/"compose.runtime-artifacts.yaml")]
        def render(arguments):
            result=subprocess.run(arguments+["config","--format","json"],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=30)
            self.assertEqual(result.returncode,0,"actual Compose profile rejected")
            return json.loads(result.stdout)
        original=render(base)
        default=render(extra)
        self.assertEqual(default["services"],original["services"])
        active=render(extra+["--profile","runtime-artifacts"])
        service=active["services"]["runtime-assets"]
        self.assertEqual(set(active["services"]),set(original["services"])|{"runtime-assets"})
        self.assertEqual(service["command"],["serve-assets","--runtime-output","--bind","0.0.0.0:8082"])
        self.assertEqual(service["environment"]["BROKERROUTER_COMPONENT"],"runtime-assets")
        self.assertEqual(service["environment"]["BROKERROUTER_DATABASE_MAX_CONNECTIONS"],"2")
        self.assertEqual({s["source"] for s in service["secrets"]},{"pg_ca","asset_s3_ca","runtime_artifact_purpose","runtime_artifact_config"})
        self.assertEqual(set(service["networks"]),{"backend","egress"})
        self.assertNotIn("ports",service);self.assertTrue(service["read_only"])
        self.assertEqual(service["tmpfs"],["/tmp:size=16m,mode=1777"])
        self.assertEqual(int(service["mem_limit"]),512*1024*1024)
        self.assertTrue(active["networks"]["backend"]["internal"])
        self.assertEqual(active["secrets"]["runtime_artifact_purpose"]["file"],env["BROKERROUTER_RUNTIME_ARTIFACT_PURPOSE_FILE"])
    def test_runtime_config_is_independent_and_rejects_other_purpose_material(self):
        cfgspec=importlib.util.spec_from_file_location("artifact_cfg",HERE/"control-plane-config.py")
        cfg=importlib.util.module_from_spec(cfgspec);cfgspec.loader.exec_module(cfg)
        self.assertEqual(len(cfg.REQUIRED),14)
        values={name:"synthetic" for name in cfg.RUNTIME_REQUIRED["runtime-assets"]}
        values["BROKERROUTER_ASSET_S3_ENDPOINT"]="https://storage.fixture.invalid"
        body=cfg.encode("runtime-assets",values)
        for name in ("BROKERROUTER_HUMAN_PUBLIC_KEYS","BROKERROUTER_PROVIDER_PRIMARY_KEY","DATABASE_URL","BROKERROUTER_ASSET_S3_CA_FILE"):
            with self.assertRaises(ValueError):cfg.encode("runtime-assets",values|{name:"synthetic"})
        with tempfile.TemporaryDirectory(prefix="runtime-artifact-config-") as temporary:
            root=Path(temporary).resolve();root.chmod(0o700)
            source=root/"input.json";source.write_text(json.dumps({"runtime-assets":values}));source.chmod(0o600)
            command=[sys.executable,str(HERE/"control-plane-config.py"),str(source),str(root)]
            result=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=10)
            self.assertEqual(result.returncode,0)
            self.assertEqual({p.name for p in (root/"component-config").iterdir()},{"runtime-assets.env"})
            self.assertEqual((root/"component-config/runtime-assets.env").read_bytes(),body)

if __name__=="__main__":unittest.main(verbosity=2)
