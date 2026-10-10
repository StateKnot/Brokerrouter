#!/usr/bin/env python3
"""Offline secret creation and transactional, TLS-only purpose login registration."""
import argparse
import base64
import configparser
import contextlib
import fcntl
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import secrets
import stat
import subprocess
import sys
import tempfile

GROUPS = {
    "control": "governance_control", "console": "console", "catalog": "catalog",
    "settings": "settings", "approvals": "approvals", "media-review": "media_review",
    "bff": "bff", "assets": "assets", "output-worker": "media_outputs",
    "output-scan": "output_scan", "output-delivery": "output_delivery",
    "media-api": "runtime", "media-worker": "runtime", "mcp-judge": "runtime",
    "mcp-catalog": "mcp_catalog", "mcp-client": "mcp_client", "mcp-server": "mcp_server",
    "mcp-worker": "mcp_worker", "reconciler": "reconciler",
}
GROUPS = {purpose: "brokerrouter_" + group for purpose, group in GROUPS.items()}
LOGINS = {purpose: "brokerrouter_" + purpose.replace("-", "_") + "_login" for purpose in GROUPS}
REPOSITORY = Path(__file__).resolve().parents[2]
LIMIT = 1024 * 1024


def require(condition, message):
    if not condition:
        raise ValueError(message)


class DatabaseFailure(ValueError):
    def __init__(self, code):
        self.code = code


def private(path, directory=False):
    info = path.lstat()
    require(not stat.S_ISLNK(info.st_mode), "secret paths cannot be symlinks")
    require((stat.S_ISDIR if directory else stat.S_ISREG)(info.st_mode), "invalid secret file type")
    require(stat.S_IMODE(info.st_mode) == (0o700 if directory else 0o600), "secret directory/files require 0700/0600")
    require(info.st_uid == os.geteuid(), "secret state must belong to the provisioning identity")


def read(path):
    private(path)
    require(path.stat().st_size <= LIMIT, "secret state exceeds size limit")
    with path.open("rb") as handle:
        value = handle.read(LIMIT + 1)
    require(len(value) <= LIMIT, "secret state exceeds size limit")
    return value


def digest(value):
    return hashlib.sha256(value).hexdigest()


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def atomic_new(path, data):
    descriptor, temporary = tempfile.mkstemp(prefix=".writing-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.link(temporary, path)  # Never replaces an existing destination.
        sync_directory(path.parent)
    finally:
        os.unlink(temporary)


@contextlib.contextmanager
def locked_root(path):
    require(path.is_absolute() and path.resolve() == path, "secret directory must be absolute and canonical")
    require(path != REPOSITORY and REPOSITORY not in path.parents, "secret directory must be outside the repository")
    if not path.exists():
        path.mkdir(mode=0o700, parents=True)
    private(path, True)
    descriptor = os.open(path / ".purpose-logins.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+b") as lock:
        private(path / ".purpose-logins.lock")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield path


def scram(password):
    salt = secrets.token_bytes(16)
    salted = hashlib.pbkdf2_hmac("sha256", password, salt, 4096)
    client = hmac.new(salted, b"Client Key", "sha256").digest()
    server = hmac.new(salted, b"Server Key", "sha256").digest()
    encode = lambda value: base64.b64encode(value).decode("ascii")
    return "SCRAM-SHA-256$4096:" + encode(salt) + "$" + encode(hashlib.sha256(client).digest()) + ":" + encode(server)


def verify_scram(password, verifier):
    match = re.fullmatch(r"SCRAM-SHA-256\$4096:([A-Za-z0-9+/=]+)\$([A-Za-z0-9+/=]+):([A-Za-z0-9+/=]+)", verifier)
    require(match is not None, "invalid SCRAM state")
    salt, stored, server = [base64.b64decode(part, validate=True) for part in match.groups()]
    require(len(salt) == 16 and len(stored) == len(server) == 32, "invalid SCRAM state")
    salted = hashlib.pbkdf2_hmac("sha256", password, salt, 4096)
    require(hmac.compare_digest(stored, hashlib.sha256(hmac.new(salted, b"Client Key", "sha256").digest()).digest())
            and hmac.compare_digest(server, hmac.new(salted, b"Server Key", "sha256").digest()), "SCRAM state does not match password")


def env_content(purpose, password):
    login = LOGINS[purpose]
    return ("DATABASE_URL='postgres://" + login + ":" + password.decode("ascii")
            + "@postgres:5432/brokerrouter?sslmode=verify-full&sslrootcert=/run/secrets/pg_ca'\n").encode()


def validate(directory):
    private(directory, True)
    manifest_bytes = read(directory / "manifest.json")
    manifest = json.loads(manifest_bytes)
    require(set(manifest) == {"version", "roles"} and manifest["version"] == 1, "unsupported purpose manifest")
    require(isinstance(manifest["roles"], list) and len(manifest["roles"]) == len(GROUPS), "invalid purpose manifest roles")
    seen = set()
    for entry in manifest["roles"]:
        require(set(entry) == {"purpose", "login", "group", "password_sha256", "env_sha256", "scram"}, "invalid purpose entry")
        purpose = entry["purpose"]
        require(purpose in GROUPS and purpose not in seen and entry["login"] == LOGINS[purpose]
                and entry["group"] == GROUPS[purpose], "purpose identity differs from fixed role contract")
        seen.add(purpose)
        password = read(directory / (purpose + ".password"))
        require(re.fullmatch(b"[0-9a-f]{64}", password) is not None, "invalid generated password")
        require(digest(password) == entry["password_sha256"], "password state hash mismatch")
        expected = env_content(purpose, password)
        require(read(directory / (purpose + ".env")) == expected and digest(expected) == entry["env_sha256"], "environment state mismatch")
        verify_scram(password, entry["scram"])
    allowed = {"manifest.json"} | {purpose + extension for purpose in GROUPS for extension in [".password", ".env", ".scram"]}
    require({path.name for path in directory.iterdir()} == allowed, "unexpected or partial purpose state")
    for entry in manifest["roles"]:
        require(read(directory / (entry["purpose"] + ".scram")).decode("ascii") == entry["scram"], "SCRAM file differs from manifest")
    return manifest


def create(root):
    target = root / "purpose-logins"
    pending = root / ".purpose-logins.pending"
    if target.exists() or target.is_symlink():
        validate(target)
        require(not pending.exists(), "published and pending state coexist; inspect before recovery")
        print("Existing purpose secrets verified; no files replaced")
        return
    if not pending.exists():
        pending.mkdir(mode=0o700)
    private(pending, True)
    # An interrupted atomic write has not been linked into committed state.
    for temporary in pending.glob(".writing-*"):
        private(temporary)
        temporary.unlink()
    entries = []
    for purpose, group in GROUPS.items():
        password_path = pending / (purpose + ".password")
        if not password_path.exists() and not password_path.is_symlink():
            atomic_new(password_path, secrets.token_hex(32).encode("ascii"))
        password = read(password_path)
        require(re.fullmatch(b"[0-9a-f]{64}", password) is not None, "partial password state requires operator inspection")
        expected = env_content(purpose, password)
        environment = pending / (purpose + ".env")
        if not environment.exists() and not environment.is_symlink():
            atomic_new(environment, expected)
        require(read(environment) == expected, "partial environment state requires operator inspection")
        verifier_file = pending / (purpose + ".scram")
        if not verifier_file.exists() and not verifier_file.is_symlink():
            atomic_new(verifier_file, scram(password).encode("ascii"))
        verifier = read(verifier_file).decode("ascii")
        verify_scram(password, verifier)
        entries.append({"purpose": purpose, "login": LOGINS[purpose], "group": group,
                        "password_sha256": digest(password), "env_sha256": digest(expected), "scram": verifier})
    content = (json.dumps({"version": 1, "roles": entries}, sort_keys=True, indent=2) + "\n").encode()
    if not (pending / "manifest.json").exists():
        atomic_new(pending / "manifest.json", content)
    require(read(pending / "manifest.json") == content, "partial manifest state mismatch")
    validate(pending)
    os.rename(pending, target)
    sync_directory(root)
    print("Purpose secrets published atomically; component configuration is still required")


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def sql(manifest):
    rows = ",\n".join("(" + ",".join(literal(entry[key]) for key in ["purpose", "login", "group", "scram", "password_sha256"]) + ")" for entry in manifest["roles"])
    return """
SET log_statement = 'none';
SET log_min_duration_statement = -1;
SET log_min_error_statement = 'panic';
SET log_duration = off;
BEGIN;
SET LOCAL lock_timeout = '3s';
SET LOCAL statement_timeout = '30s';
SELECT pg_advisory_xact_lock(638037123);
DO $guard$ BEGIN
 IF NOT (SELECT rolsuper FROM pg_roles WHERE rolname=current_user)
    OR current_database()<>'brokerrouter'
    OR NOT (SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid())
    OR current_setting('password_encryption')<>'scram-sha-256' THEN
  RAISE EXCEPTION 'trusted TLS SCRAM deployment administrator required' USING ERRCODE='BR001';
 END IF;
 IF EXISTS(SELECT 1 FROM pg_hba_file_rules WHERE error IS NOT NULL)
    OR EXISTS(SELECT 1 FROM pg_hba_file_rules WHERE type IN('host','hostssl','hostnossl')
       AND auth_method<>'reject' AND NOT(type='hostssl' AND auth_method='scram-sha-256')) THEN
  RAISE EXCEPTION 'network HBA must require TLS and SCRAM' USING ERRCODE='BR002';
 END IF;
END $guard$;
CREATE SCHEMA IF NOT EXISTS brokerrouter_deployment;
REVOKE ALL ON SCHEMA brokerrouter_deployment FROM PUBLIC;
CREATE TABLE IF NOT EXISTS brokerrouter_deployment.purpose_logins(
 login text PRIMARY KEY, purpose text UNIQUE NOT NULL, purpose_group text NOT NULL,
 password_sha256 text NOT NULL, version integer NOT NULL CHECK(version=1));
REVOKE ALL ON brokerrouter_deployment.purpose_logins FROM PUBLIC;
DO $provision$ DECLARE r record; g oid; l oid; allowed text[]; reachable text[]; existing record;
BEGIN
 IF (SELECT nspowner<>current_user::regrole FROM pg_namespace WHERE nspname='brokerrouter_deployment')
    OR (SELECT relowner<>current_user::regrole FROM pg_class WHERE oid='brokerrouter_deployment.purpose_logins'::regclass) THEN
  RAISE EXCEPTION 'deployment registry owner mismatch' USING ERRCODE='BR003';
 END IF;
 FOR r IN SELECT * FROM (VALUES
""" + rows + """
 ) AS input(purpose,login,purpose_group,verifier,password_sha256) LOOP
  SELECT oid INTO g FROM pg_roles WHERE rolname=r.purpose_group AND NOT rolcanlogin AND NOT rolsuper
    AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolreplication AND NOT rolbypassrls;
  IF g IS NULL THEN RAISE EXCEPTION 'purpose group missing or privileged' USING ERRCODE='BR010'; END IF;
  allowed:=ARRAY[r.purpose_group];
  IF r.purpose='reconciler' THEN allowed:=allowed||ARRAY['brokerrouter_runtime']; END IF;
  WITH RECURSIVE ancestry(id) AS (
    SELECT g UNION SELECT m.roleid FROM pg_auth_members m JOIN ancestry a ON m.member=a.id
  ) SELECT array_agg(rolname::text ORDER BY rolname) INTO reachable FROM ancestry JOIN pg_roles p ON p.oid=ancestry.id;
  IF NOT(reachable @> allowed AND reachable <@ allowed) THEN
    RAISE EXCEPTION 'purpose group has unexpected recursive membership' USING ERRCODE='BR011';
  END IF;
  IF EXISTS(SELECT 1 FROM pg_roles WHERE rolname=ANY(reachable)
       AND(rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls))
     OR EXISTS(SELECT 1 FROM pg_class WHERE relowner=g)
     OR EXISTS(SELECT 1 FROM pg_namespace WHERE nspowner=g)
     OR EXISTS(SELECT 1 FROM pg_database WHERE datdba=g) THEN
    RAISE EXCEPTION 'purpose group has administrative ownership or flags' USING ERRCODE='BR012';
  END IF;
  IF has_column_privilege(g,'public.approvals','state','UPDATE')<>(r.purpose IN('control','approvals'))
     OR has_table_privilege(g,'public.asset_chunks','SELECT')<>(r.purpose IN('assets','output-worker','output-scan','output-delivery','media-review'))
     OR NOT has_table_privilege(g,'public._sqlx_migrations','SELECT') THEN
    RAISE EXCEPTION 'purpose group critical permission contract mismatch' USING ERRCODE='BR013';
  END IF;
  SELECT oid INTO l FROM pg_roles WHERE rolname=r.login;
  SELECT * INTO existing FROM brokerrouter_deployment.purpose_logins WHERE login=r.login;
  IF l IS NOT NULL AND NOT FOUND THEN RAISE EXCEPTION 'unregistered same-name login cannot be adopted' USING ERRCODE='BR020'; END IF;
  IF l IS NULL AND FOUND THEN RAISE EXCEPTION 'registered login is missing' USING ERRCODE='BR021'; END IF;
  IF l IS NULL THEN
    EXECUTE format('CREATE ROLE %I LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 8 PASSWORD %L',r.login,r.verifier);
    EXECUTE format('ALTER ROLE %I SET statement_timeout=''15s''',r.login);
    EXECUTE format('ALTER ROLE %I SET lock_timeout=''3s''',r.login);
    EXECUTE format('ALTER ROLE %I SET idle_in_transaction_session_timeout=''60s''',r.login);
    EXECUTE format('GRANT %I TO %I WITH ADMIN FALSE, INHERIT TRUE, SET TRUE',r.purpose_group,r.login);
    EXECUTE format('GRANT CONNECT ON DATABASE brokerrouter TO %I',r.login);
    INSERT INTO brokerrouter_deployment.purpose_logins VALUES(r.login,r.purpose,r.purpose_group,r.password_sha256,1);
    SELECT oid INTO l FROM pg_roles WHERE rolname=r.login;
  ELSE
    IF existing.purpose<>r.purpose OR existing.purpose_group<>r.purpose_group
       OR existing.password_sha256<>r.password_sha256 OR existing.version<>1 THEN
      RAISE EXCEPTION 'registered identity or secret conflict' USING ERRCODE='BR022';
    END IF;
  END IF;
  IF NOT EXISTS(SELECT 1 FROM pg_authid WHERE oid=l AND rolcanlogin AND rolinherit AND NOT rolsuper
       AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolreplication AND NOT rolbypassrls
       AND rolconnlimit=8 AND rolpassword=r.verifier)
     OR NOT EXISTS(SELECT 1 FROM pg_roles WHERE oid=l
       AND rolconfig @> ARRAY['statement_timeout=15s','lock_timeout=3s','idle_in_transaction_session_timeout=60s']
       AND cardinality(rolconfig)=3)
     OR (SELECT count(*) FROM pg_auth_members WHERE member=l)<>1
     OR NOT EXISTS(SELECT 1 FROM pg_auth_members WHERE member=l AND roleid=g AND NOT admin_option AND inherit_option AND set_option)
     OR NOT has_database_privilege(l,'brokerrouter','CONNECT')
     OR has_database_privilege(l,'brokerrouter','CREATE,TEMPORARY')
     OR has_schema_privilege(l,'public','CREATE')
     OR EXISTS(SELECT 1 FROM pg_class WHERE relowner=l)
     OR EXISTS(SELECT 1 FROM pg_namespace WHERE nspowner=l)
     OR EXISTS(SELECT 1 FROM pg_database WHERE datdba=l) THEN
    RAISE EXCEPTION 'registered login attributes or membership conflict' USING ERRCODE='BR023';
  END IF;
  IF EXISTS(SELECT 1 FROM pg_class c CROSS JOIN LATERAL aclexplode(c.relacl) a WHERE a.grantee=l)
     OR EXISTS(SELECT 1 FROM pg_attribute c CROSS JOIN LATERAL aclexplode(c.attacl) a WHERE a.grantee=l)
     OR EXISTS(SELECT 1 FROM pg_proc c CROSS JOIN LATERAL aclexplode(c.proacl) a WHERE a.grantee=l)
     OR EXISTS(SELECT 1 FROM pg_namespace c CROSS JOIN LATERAL aclexplode(c.nspacl) a WHERE a.grantee=l)
     OR EXISTS(SELECT 1 FROM pg_database c CROSS JOIN LATERAL aclexplode(c.datacl) a WHERE a.grantee=l AND NOT(c.datname='brokerrouter' AND a.privilege_type='CONNECT' AND NOT a.is_grantable)) THEN
    RAISE EXCEPTION 'registered login has direct privileges outside its purpose group' USING ERRCODE='BR024';
  END IF;
 END LOOP;
END $provision$;
COMMIT;
"""


def apply(root, service_file, service):
    manifest = validate(root / "purpose-logins")
    require(service_file.is_absolute() and service_file.resolve() == service_file, "admin service file must be absolute and canonical")
    config = configparser.ConfigParser(interpolation=None)
    config.read_string(read(service_file).decode("utf-8"))
    require(re.fullmatch(r"[A-Za-z][A-Za-z0-9_-]{0,63}", service) is not None and service in config, "invalid admin service")
    selected = config[service]
    require(not config.defaults() and set(selected) <= {"host", "hostaddr", "port", "dbname", "user", "sslmode", "sslrootcert", "passfile", "connect_timeout"}, "unsupported admin service settings")
    require(selected.get("sslmode") == "verify-full" and selected.get("dbname") == "brokerrouter"
            and selected.get("host") and selected.get("user"), "admin service requires explicit TLS verify-full target")
    for key in ["sslrootcert", "passfile"]:
        path = Path(selected.get(key, ""))
        require(path.is_absolute() and path.resolve() == path, "admin TLS/passfile paths must be canonical")
        if key == "passfile":
            read(path)
        else:
            require(path.is_file(), "admin CA file must exist")
    environment = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    environment["PGSERVICEFILE"] = str(service_file)
    environment["PGCONNECT_TIMEOUT"] = "5"
    result = subprocess.run(["psql", "service=" + service, "-X", "--quiet", "--no-password", "--set=ON_ERROR_STOP=1", "--set=VERBOSITY=sqlstate"],
                            input=sql(manifest).encode(), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            env=environment, timeout=60, check=False)
    if result.returncode != 0:
        codes = re.findall(rb"ERROR:\s+([A-Z0-9]{5})", result.stderr)
        raise DatabaseFailure(codes[-1].decode("ascii") if codes else "unavailable")
    print("Purpose logins verified and registered transactionally; existing core credentials unchanged")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create_parser = commands.add_parser("create")
    create_parser.add_argument("secret_directory", type=Path)
    apply_parser = commands.add_parser("apply")
    apply_parser.add_argument("secret_directory", type=Path)
    apply_parser.add_argument("admin_service_file", type=Path)
    apply_parser.add_argument("service")
    arguments = parser.parse_args()
    os.umask(0o077)
    try:
        with locked_root(arguments.secret_directory) as root:
            if arguments.command == "create":
                create(root)
            else:
                apply(root, arguments.admin_service_file, arguments.service)
    except DatabaseFailure as error:
        print("Purpose transaction failed closed; SQLSTATE=" + error.code, file=sys.stderr)
        return 1
    except (ValueError, TypeError, KeyError, OSError, UnicodeError, json.JSONDecodeError, configparser.Error, subprocess.SubprocessError):
        # Never expose subprocess output, paths, SQL/verifiers, DSNs or secret values.
        print("Purpose provisioning failed closed; inspect private state and documented recovery procedure", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
