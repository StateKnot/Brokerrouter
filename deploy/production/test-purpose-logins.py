#!/usr/bin/env python3
"""Real script/role checks on an isolated, TLS-enabled tmpfs PostgreSQL 17."""
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import re
import signal
import subprocess
import tempfile
import time
import unittest

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
IMAGE = "postgres@sha256:67f41722b7a8cbdb868a44a4995c846eddfdc2973bccb291ce937dce88ad5675"
spec = importlib.util.spec_from_file_location("purpose_logins", HERE / "purpose-logins.py")
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class ProductionPurposeLogins(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="brokerrouter-purpose-logins-")
        self.root = Path(self.temporary.name).resolve()
        self.name = "brokerrouter-purpose-test-" + secrets.token_hex(6)
        self.outputs = []
        self.database_started = False
        self.addCleanup(self.cleanup)
        self.secret_dir = self.root / "secrets"
        self.secret_dir.mkdir(mode=0o700)
        self.passwords = {name: secrets.token_hex(32) for name in ["migration", "runtime", "operator"]}
        for name, password in self.passwords.items():
            self.write(self.secret_dir / (name + ".env"), "legacy state " + password)
        self.core_files = self.snapshot(self.secret_dir)
        self.openssl(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Purpose Test CA", "-keyout", "ca.key", "-out", "ca.crt"])
        self.openssl(["req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=postgres", "-keyout", "server.key", "-out", "server.csr"])
        self.write(self.root / "server.ext", "subjectAltName=DNS:localhost,DNS:postgres\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n")
        self.openssl(["x509", "-req", "-in", "server.csr", "-CA", "ca.crt", "-CAkey", "ca.key", "-CAcreateserial", "-days", "1", "-extfile", "server.ext", "-out", "server.crt"])
        self.write(self.root / "migration.password", self.passwords["migration"])
        self.write(self.root / "pg_hba.conf", (HERE / "pg_hba.conf").read_text())
        self.write(self.root / "admin.pgpass", "localhost:5432:brokerrouter:brokerrouter_migration:" + self.passwords["migration"] + "\n")
        self.service = self.root / "admin.service"
        self.write(self.service, "[admin]\nhost=localhost\nport=5432\ndbname=brokerrouter\nuser=brokerrouter_migration\nsslmode=verify-full\nsslrootcert=" + str(self.root / "ca.crt") + "\npassfile=" + str(self.root / "admin.pgpass") + "\n")
        shim = self.root / "bin"
        shim.mkdir(mode=0o700)
        self.write(shim / "psql", "#!/bin/sh\nexec docker exec -i -e PGSERVICEFILE=\"$PGSERVICEFILE\" " + self.name + " psql \"$@\"\n")
        (shim / "psql").chmod(0o700)
        self.environment = os.environ.copy()
        self.environment["PATH"] = str(shim) + os.pathsep + self.environment["PATH"]
        startup = "mkdir -p /tls; cp \"$1/server.key\" /tls/server.key; cp \"$1/server.crt\" /tls/server.crt; chmod 0600 /tls/server.key; chown 999:999 /tls/*; exec /usr/local/bin/docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tls/server.crt -c ssl_key_file=/tls/server.key -c hba_file=\"$1/pg_hba.conf\" -c password_encryption=scram-sha-256 -c log_statement=none -c log_min_duration_statement=-1 -c log_min_error_statement=panic"
        self.call(["docker", "run", "--detach", "--name", self.name, "--hostname", "postgres", "--add-host", "postgres:127.0.0.1", "--network", "none", "--read-only",
                   "--tmpfs", "/var/lib/postgresql/data:rw,size=256m", "--tmpfs", "/tls:rw,size=8m", "--tmpfs", "/var/run/postgresql:rw,size=8m", "--tmpfs", "/tmp:rw,size=16m",
                   "--memory", "512m", "--cpus", "1", "--pids-limit", "128", "--mount", "type=bind,src=" + str(self.root) + ",dst=" + str(self.root) + ",readonly",
                   "--mount", "type=bind,src=" + str(self.root / "ca.crt") + ",dst=/run/secrets/pg_ca,readonly",
                   "--env", "POSTGRES_USER=brokerrouter_migration", "--env", "POSTGRES_DB=brokerrouter", "--env", "POSTGRES_PASSWORD_FILE=" + str(self.root / "migration.password"),
                   "--env", "POSTGRES_INITDB_ARGS=--auth-host=scram-sha-256 --auth-local=trust", "--entrypoint", "sh", IMAGE, "-ec", startup, "fixture", str(self.root)])
        self.database_started = True
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            ready = subprocess.run(["docker", "exec", self.name, "pg_isready", "-U", "brokerrouter_migration", "-d", "brokerrouter"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if ready.returncode == 0:
                break
            time.sleep(0.2)
        else:
            self.fail("isolated PostgreSQL did not become ready")
        self.admin("CREATE TABLE _sqlx_migrations(version bigint PRIMARY KEY);")
        for migration in sorted((REPO / "migrations").glob("*.sql")):
            self.admin(migration.read_text())
        self.admin((REPO / "deploy/permissions.sql").read_text())
        self.admin("REVOKE ALL ON DATABASE brokerrouter FROM PUBLIC; GRANT CONNECT ON DATABASE brokerrouter TO brokerrouter_migration;")
        for name in ["runtime", "operator"]:
            self.admin("CREATE ROLE brokerrouter_" + name + "_login LOGIN PASSWORD '" + helper.scram(self.passwords[name].encode()) + "'; GRANT brokerrouter_" + name + " TO brokerrouter_" + name + "_login;")
        self.core_db = self.admin("SELECT rolname,encode(sha256(convert_to(rolpassword,'UTF8')),'hex') FROM pg_authid WHERE rolname IN('brokerrouter_migration','brokerrouter_runtime_login','brokerrouter_operator_login') ORDER BY rolname;").stdout

    def cleanup(self):
        if self.database_started:
            logs = subprocess.run(["docker", "logs", self.name], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.outputs.extend([logs.stdout, logs.stderr])
            subprocess.run(["docker", "rm", "--force", self.name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.temporary.cleanup()

    def write(self, path, content):
        path.write_text(content)
        path.chmod(0o600)

    def call(self, arguments, expect=0, **options):
        result = subprocess.run(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60, **options)
        self.outputs.extend([result.stdout, result.stderr])
        codes = re.findall(rb"SQLSTATE=([A-Z0-9]{5})", result.stderr)
        self.assertEqual(result.returncode, expect, "command exit status differs; safe SQLSTATE=" + (codes[-1].decode() if codes else "unavailable"))
        return result

    def openssl(self, arguments):
        self.call(["openssl"] + arguments, cwd=self.root)

    def psql(self, sql, service="admin", service_file=None, expect=0):
        environment = self.environment.copy()
        environment["PGSERVICEFILE"] = str(service_file or self.service)
        return self.call(["psql", "service=" + service, "-X", "-q", "-A", "-t", "-w", "--set=ON_ERROR_STOP=1"], input=sql.encode(), env=environment, expect=expect)

    def admin(self, sql):
        return self.psql(sql)

    def create(self, directory=None, expect=0):
        return self.call(["sh", str(HERE / "create-purpose-secrets.sh"), str(directory or self.secret_dir)], env=self.environment, expect=expect)

    def apply(self, expect=0):
        return self.call(["sh", str(HERE / "apply-purpose-logins.sh"), str(self.secret_dir), str(self.service), "admin"], env=self.environment, expect=expect)

    def snapshot(self, directory):
        return {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in directory.iterdir() if path.is_file()}

    def count_logins(self):
        names = ",".join(helper.literal(name) for name in helper.LOGINS.values())
        return int(self.admin("SELECT count(*) FROM pg_roles WHERE rolname IN(" + names + ");").stdout)

    def passed(self, label):
        print("purpose-login scenario: " + label + " passed", flush=True)

    def provision_image(self, expect=0, with_runtime_artifacts=False, plaintext_admin=False):
        image = os.environ.get("BROKERROUTER_PROVISIONER_TEST_IMAGE")
        service = self.root / "purpose-admin.service"
        passfile = self.root / "purpose-admin.passfile"
        self.write(passfile, "postgres:5432:brokerrouter:brokerrouter_migration:" + self.passwords["migration"] + "\n")
        self.write(service, "[purpose]\nhost=postgres\nhostaddr=127.0.0.1\nport=5432\ndbname=brokerrouter\nuser=brokerrouter_migration\nsslmode=verify-full\nsslrootcert=/run/secrets/pg_ca\npassfile=/run/secrets/purpose_admin_passfile\nconnect_timeout=5\n")
        if plaintext_admin:
            self.write(service, service.read_text().replace("sslmode=verify-full", "sslmode=disable"))
        command = ["docker", "run", "--rm", "--network", "container:" + self.name,
                              "--user", str(os.geteuid()) + ":" + str(os.getegid()), "--read-only", "--cap-drop", "ALL",
                              "--security-opt", "no-new-privileges", "--memory", "128m", "--cpus", "0.5", "--pids-limit", "32",
                              "--tmpfs", "/run/purpose-secrets:size=1m,mode=0700,uid=" + str(os.geteuid()) + ",gid=" + str(os.getegid()),
                              "--tmpfs", "/tmp:size=8m,mode=1777",
                              "--mount", "type=bind,src=" + str(self.secret_dir / "purpose-logins") + ",dst=/run/purpose-secrets/purpose-logins,readonly",
                              "--mount", "type=bind,src=" + str(self.secret_dir / ".purpose-logins.lock") + ",dst=/run/purpose-secrets/.purpose-logins.lock",
                              "--mount", "type=bind,src=" + str(service) + ",dst=/run/secrets/purpose_admin_service,readonly",
                              "--mount", "type=bind,src=" + str(passfile) + ",dst=/run/secrets/purpose_admin_passfile,readonly",
                              "--mount", "type=bind,src=" + str(self.root / "ca.crt") + ",dst=/run/secrets/pg_ca,readonly"]
        if with_runtime_artifacts:
            command += ["--mount", "type=bind,src=" + str(self.secret_dir / "purpose-logins-v2") + ",dst=/run/purpose-secrets/purpose-logins-v2,readonly"]
        command.append(image)
        if with_runtime_artifacts:
            command.append("--with-runtime-artifacts")
        return self.call(command, expect=expect)

    @unittest.skipUnless(os.environ.get("BROKERROUTER_PROVISIONER_TEST_IMAGE"), "set independent provisioner test image after building it")
    def test_isolated_ops_image_fresh_upgrade_and_conflict(self):
        self.create()
        provision = self.provision_image
        original = self.snapshot(self.secret_dir / "purpose-logins")
        provision()
        self.assertEqual(self.count_logins(), len(helper.GROUPS))
        provision()
        self.assertEqual(self.snapshot(self.secret_dir / "purpose-logins"), original)
        self.assertEqual(self.admin("SELECT rolname,encode(sha256(convert_to(rolpassword,'UTF8')),'hex') FROM pg_authid WHERE rolname IN('brokerrouter_migration','brokerrouter_runtime_login','brokerrouter_operator_login') ORDER BY rolname;").stdout, self.core_db)
        self.passed("isolated ops image actual fresh registration and upgrade rerun preserve core Secrets")
        self.admin("GRANT brokerrouter_approvals TO brokerrouter_console;")
        provision(expect=1)
        self.admin("REVOKE brokerrouter_approvals FROM brokerrouter_console;")
        self.passed("isolated ops image rejects recursive role conflict without partial mutation")
        provision(expect=1, plaintext_admin=True)
        self.passed("isolated ops image refuses administrator plaintext configuration")

    def create_legacy_19(self):
        directory = self.secret_dir / "purpose-logins"
        directory.mkdir(mode=0o700)
        entries = []
        for purpose, group in helper.GROUPS.items():
            if purpose == "runtime-assets":
                continue
            password = secrets.token_hex(32).encode("ascii")
            environment, verifier = helper.env_content(purpose, password), helper.scram(password)
            for suffix, body in ((".password", password), (".env", environment), (".scram", verifier.encode())):
                self.write(directory / (purpose + suffix), body.decode())
            entries.append({"purpose":purpose,"login":helper.LOGINS[purpose],"group":group,
                "password_sha256":helper.digest(password),"env_sha256":helper.digest(environment),"scram":verifier})
        self.write(directory / "manifest.json", json.dumps({"version":1,"roles":entries},sort_keys=True,indent=2)+"\n")
        # The actual old creator leaves this deployment-owned lock beside its
        # immutable release; the image must share it without writable Secrets.
        self.write(self.secret_dir / ".purpose-logins.lock", "")
        return directory

    @unittest.skipUnless(os.environ.get("BROKERROUTER_PROVISIONER_TEST_IMAGE"), "set independent provisioner test image after building it")
    def test_isolated_ops_image_legacy_19_to_version2_extension(self):
        directory = self.create_legacy_19()
        original = self.snapshot(directory)
        self.provision_image()
        self.assertEqual(self.count_logins(), 19)
        registered = self.admin("SELECT login,password_sha256 FROM brokerrouter_deployment.purpose_logins ORDER BY login;").stdout
        self.call(["sh", str(HERE / "upgrade-runtime-artifact-secrets.sh"), str(self.secret_dir)], env=self.environment)
        extension = self.snapshot(self.secret_dir / "purpose-logins-v2")
        self.provision_image()
        self.assertEqual(self.count_logins(), 19)
        self.provision_image(with_runtime_artifacts=True)
        self.assertEqual(self.count_logins(), 20)
        self.provision_image(with_runtime_artifacts=True)
        self.assertEqual(self.admin("SELECT login,password_sha256 FROM brokerrouter_deployment.purpose_logins WHERE purpose<>'runtime-assets' ORDER BY login;").stdout, registered)
        self.assertEqual(self.snapshot(directory), original)
        self.assertEqual(self.snapshot(self.secret_dir / "purpose-logins-v2"), extension)
        self.assertEqual(self.admin("SELECT rolname,encode(sha256(convert_to(rolpassword,'UTF8')),'hex') FROM pg_authid WHERE rolname IN('brokerrouter_migration','brokerrouter_runtime_login','brokerrouter_operator_login') ORDER BY rolname;").stdout, self.core_db)
        self.passed("isolated ops image explicit version 2 read-only mount upgrades 19→20 without Secret rotation")

    def test_actual_legacy_19_registration_and_explicit_version2_upgrade(self):
        directory = self.create_legacy_19()
        original = self.snapshot(directory)
        self.apply()
        self.assertEqual(self.count_logins(),19)
        registered = self.admin("SELECT login,password_sha256 FROM brokerrouter_deployment.purpose_logins ORDER BY login;").stdout
        self.call(["sh",str(HERE/"upgrade-runtime-artifact-secrets.sh"),str(self.secret_dir)],env=self.environment)
        self.assertEqual(self.snapshot(directory),original)
        self.apply()  # default cannot silently register the extension
        self.assertEqual(self.count_logins(),19)
        self.call(["sh",str(HERE/"apply-purpose-logins.sh"),"--with-runtime-artifacts",str(self.secret_dir),str(self.service),"admin"],env=self.environment)
        self.assertEqual(self.count_logins(),20)
        unchanged = self.admin("SELECT login,password_sha256 FROM brokerrouter_deployment.purpose_logins WHERE purpose<>'runtime-assets' ORDER BY login;").stdout
        self.assertEqual(registered,unchanged)
        self.assertEqual(self.snapshot(directory),original)
        self.call(["sh",str(HERE/"apply-purpose-logins.sh"),"--with-runtime-artifacts",str(self.secret_dir),str(self.service),"admin"],env=self.environment)
        self.assertEqual(self.count_logins(),20)
        self.assertEqual(self.admin("SELECT rolname,encode(sha256(convert_to(rolpassword,'UTF8')),'hex') FROM pg_authid WHERE rolname IN('brokerrouter_migration','brokerrouter_runtime_login','brokerrouter_operator_login') ORDER BY rolname;").stdout,self.core_db)
        self.passed("actual TLS 19→20 extension preserves every original login and Secret without rotation")

    def test_real_scripts_and_permissions(self):
        self.create()
        directory = self.secret_dir / "purpose-logins"
        manifest = helper.validate(directory)
        original = self.snapshot(directory)
        self.create()
        self.assertEqual(self.snapshot(directory), original)
        self.assertEqual({name: self.snapshot(self.secret_dir)[name] for name in self.core_files}, self.core_files)
        self.passed("fresh secrets and idempotent no-overwrite")

        self.admin("GRANT brokerrouter_approvals TO brokerrouter_console;")
        self.apply(expect=1)
        self.assertEqual(self.count_logins(), 0)
        self.admin("REVOKE brokerrouter_approvals FROM brokerrouter_console;")
        self.passed("recursive cross-purpose membership blocks and rolls back")

        self.admin("GRANT SELECT ON asset_chunks TO brokerrouter_approvals;")
        self.apply(expect=1)
        self.assertEqual(self.count_logins(), 0)
        self.admin("REVOKE SELECT ON asset_chunks FROM brokerrouter_approvals;")
        self.passed("critical existing group privilege drift blocks")

        self.admin("CREATE ROLE brokerrouter_console_login LOGIN;")
        self.apply(expect=1)
        self.assertEqual(self.count_logins(), 1)
        self.admin("DROP ROLE brokerrouter_console_login;")
        self.passed("unregistered same-name role is not adopted; prior creates roll back")

        self.admin("CREATE SCHEMA brokerrouter_deployment; CREATE TABLE brokerrouter_deployment.purpose_logins(login text PRIMARY KEY,purpose text UNIQUE NOT NULL,purpose_group text NOT NULL,password_sha256 text NOT NULL,version integer NOT NULL CHECK(version=1)); CREATE FUNCTION brokerrouter_deployment.pause() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(30); RETURN NEW; END $$; CREATE TRIGGER pause BEFORE INSERT ON brokerrouter_deployment.purpose_logins FOR EACH ROW EXECUTE FUNCTION brokerrouter_deployment.pause();")
        process = subprocess.Popen(["sh", str(HERE / "apply-purpose-logins.sh"), str(self.secret_dir), str(self.service), "admin"], env=self.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                pid = self.admin("SELECT pid FROM pg_stat_activity WHERE wait_event='PgSleep' AND datname='brokerrouter';").stdout.strip()
                if pid:
                    break
                time.sleep(0.05)
            else:
                self.fail("registration transaction did not reach interruption fixture")
            self.admin("SELECT pg_terminate_backend(" + pid.decode("ascii") + ");")
            stdout, stderr = process.communicate(timeout=10)
            self.outputs.extend([stdout, stderr])
            self.assertNotEqual(process.returncode, 0)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
        self.assertEqual(self.count_logins(), 0)
        self.admin("DROP TRIGGER pause ON brokerrouter_deployment.purpose_logins; DROP FUNCTION brokerrouter_deployment.pause();")
        self.passed("backend termination after CREATE ROLE rolls whole transaction back")

        self.apply()
        self.assertEqual(self.count_logins(), len(helper.GROUPS))
        self.apply()
        self.assertEqual(self.snapshot(directory), original)
        self.assertEqual(self.admin("SELECT rolname,encode(sha256(convert_to(rolpassword,'UTF8')),'hex') FROM pg_authid WHERE rolname IN('brokerrouter_migration','brokerrouter_runtime_login','brokerrouter_operator_login') ORDER BY rolname;").stdout, self.core_db)
        self.passed("fresh register and upgrade rerun retain every core credential")

        for mutation, repair, label in [
            ("ALTER ROLE brokerrouter_console_login SUPERUSER;", "ALTER ROLE brokerrouter_console_login NOSUPERUSER;", "privileged registered login"),
            ("GRANT brokerrouter_runtime TO brokerrouter_approvals_login;", "REVOKE brokerrouter_runtime FROM brokerrouter_approvals_login;", "mixed registered login membership"),
            ("GRANT SELECT ON endpoints TO brokerrouter_console_login;", "REVOKE SELECT ON endpoints FROM brokerrouter_console_login;", "direct registered login grant"),
        ]:
            self.admin(mutation)
            self.apply(expect=1)
            self.assertEqual(self.snapshot(directory), original)
            self.admin(repair)
            self.passed(label + " fails without auto-repair")

        manifest_file = directory / "manifest.json"
        content = manifest_file.read_text()
        changed = json.loads(content)
        changed["roles"][0]["group"] = "brokerrouter_operator"
        self.write(manifest_file, json.dumps(changed))
        self.apply(expect=1)
        self.create(expect=1)
        self.write(manifest_file, content)
        self.passed("bad manifest is neither adopted nor overwritten")

        password_file = directory / "console.password"
        backup = self.root / "password-backup"
        password_file.rename(backup)
        password_file.symlink_to(backup)
        self.apply(expect=1)
        self.create(expect=1)
        password_file.unlink()
        backup.rename(password_file)
        with (self.secret_dir / ".purpose-logins.lock").open("r+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.create(expect=1)
            self.apply(expect=1)
        self.passed("symlink and concurrent provisioning reject without mutation")

        interrupted = self.root / "interrupted"
        process = subprocess.Popen(["sh", str(HERE / "create-purpose-secrets.sh"), str(interrupted)], env=self.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        first = interrupted / ".purpose-logins.pending/control.password"
        try:
            deadline = time.monotonic() + 5
            while not first.exists() and time.monotonic() < deadline and process.poll() is None:
                time.sleep(0.0005)
            self.assertTrue(first.exists(), "interruption must happen after the first committed secret")
            saved = first.read_bytes()
            process.kill()
            stdout, stderr = process.communicate(timeout=5)
            self.outputs.extend([stdout, stderr])
            self.assertFalse((interrupted / "purpose-logins").exists())
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
        self.create(interrupted)
        self.assertEqual((interrupted / "purpose-logins/control.password").read_bytes(), saved)
        helper.validate(interrupted / "purpose-logins")
        self.passed("killed secret writer resumes without replacing committed passwords")

        service_lines = []
        passwords = []
        for entry in manifest["roles"]:
            purpose, login = entry["purpose"], entry["login"]
            password = (directory / (purpose + ".password")).read_text()
            passwords.append("postgres:5432:brokerrouter:" + login + ":" + password)
            service_lines.append("[" + purpose + "]\nhost=postgres\nport=5432\ndbname=brokerrouter\nuser=" + login + "\nsslmode=verify-full\nsslrootcert=/run/secrets/pg_ca\npassfile=" + str(self.root / "apps.pgpass") + "\n")
        self.write(self.root / "apps.pgpass", "\n".join(passwords) + "\n")
        apps = self.root / "apps.service"
        self.write(apps, "\n".join(service_lines))
        all_groups = sorted(set(helper.GROUPS.values()))
        for purpose, group in helper.GROUPS.items():
            allowed = [group] + (["brokerrouter_runtime"] if purpose == "reconciler" else [])
            matrix = """SELECT count(*) FROM _sqlx_migrations;
DO $matrix$ DECLARE target text; permitted boolean; BEGIN
 FOREACH target IN ARRAY ARRAY[%s] LOOP
  permitted:=target=ANY(ARRAY[%s]);
  BEGIN
   EXECUTE format('SET LOCAL ROLE %%I',target);
   IF NOT permitted THEN RAISE EXCEPTION 'cross-purpose SET ROLE unexpectedly succeeds'; END IF;
   EXECUTE 'SELECT count(*) FROM _sqlx_migrations';
   EXECUTE 'RESET ROLE';
  EXCEPTION WHEN insufficient_privilege THEN
   IF permitted THEN RAISE EXCEPTION 'own purpose SET ROLE rejected'; END IF;
  END;
 END LOOP;
END $matrix$;
""" % (",".join(helper.literal(g) for g in all_groups), ",".join(helper.literal(g) for g in allowed))
            self.psql(matrix, purpose, apps)
            forbidden = "UPDATE approvals SET state=state;" if purpose not in ["control", "approvals"] else "SELECT * FROM asset_chunks;"
            result = self.psql(forbidden, purpose, apps, expect=3)
            self.assertIn(b"permission denied", result.stderr)
            self.passed("TLS/SCRAM/positive and cross-role/negative permission matrix " + purpose)
        self.psql("SELECT 1;", "console", apps, expect=0)
        self.write(apps, apps.read_text().replace("sslmode=verify-full", "sslmode=disable"))
        self.psql("SELECT 1;", "console", apps, expect=2)
        self.write(apps, apps.read_text().replace("sslmode=disable", "sslmode=verify-full").replace("host=postgres", "host=127.0.0.1"))
        self.psql("SELECT 1;", "console", apps, expect=2)
        self.passed("network plaintext and wrong TLS hostname reject")

        self.apply()
        self.assertEqual(self.snapshot(directory), original)
        logs = subprocess.run(["docker", "logs", self.name], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        transcript = b"\n".join(self.outputs + [logs.stdout, logs.stderr])
        for entry in manifest["roles"]:
            self.assertNotIn((directory / (entry["purpose"] + ".password")).read_bytes(), transcript, "password appeared in captured logs")
            self.assertNotIn(entry["scram"].encode(), transcript, "SCRAM verifier appeared in captured logs")
        for password in self.passwords.values():
            # core_db was deliberately read into memory for unchanged assertions;
            # captured normal output never contains its plaintext.
            self.assertNotIn(password.encode(), transcript)
        self.passed("captured client/server logs contain no password or SCRAM verifier")


if __name__ == "__main__":
    unittest.main(verbosity=2)
