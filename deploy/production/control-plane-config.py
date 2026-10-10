#!/usr/bin/env python3
"""Encode external, purpose-scoped UTF-8 configuration without shell evaluation."""
import argparse
import base64
import fcntl
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
from urllib.parse import urlsplit

LIMIT = 1024 * 1024
LINE_LIMIT = 131072
PREFIX = "BROKERROUTER_"
RESULT = {PREFIX + name for name in ("ACTIVE_RESULT_KEY", "RESULT_KEYS")}
S3 = RESULT | {PREFIX + "ASSET_S3_" + name for name in (
    "ENDPOINT", "BUCKET", "REGION", "ACCESS_KEY_ID", "SECRET_ACCESS_KEY")}
PROVIDERS = {PREFIX + "PROVIDER_PRIMARY_KEY", PREFIX + "PROVIDER_SECONDARY_KEY"}
REQUIRED = {
    "control": RESULT | {PREFIX + "HUMAN_" + name for name in ("ISSUER", "AUDIENCE", "PUBLIC_KEYS")},
    "assets": S3, "output-scan": S3, "output-worker": S3,
    "output-delivery": S3 | {PREFIX + "HUMAN_" + name for name in ("ISSUER", "AUDIENCE", "PUBLIC_KEYS")},
    "media-api": RESULT, "media-worker": RESULT, "mcp-judge": RESULT,
    "bff": {PREFIX + name for name in (
        "BFF_ORIGIN", "OIDC_ISSUER", "OIDC_ALLOWED_ORIGINS", "OIDC_CLIENT_ID", "OIDC_CLIENT_SECRET",
        "BFF_ACTIVE_KEY", "BFF_KEYS", "BFF_MEDIA_PREVIEW_ORIGIN")},
}
for purpose in ("console", "catalog", "settings", "approvals", "media-review"):
    verifier = purpose.replace("-", "_").upper()
    REQUIRED[purpose] = {PREFIX + verifier + "_" + name for name in ("ISSUER", "AUDIENCE", "PUBLIC_KEYS")}
    if purpose == "approvals":
        REQUIRED[purpose] |= RESULT
    if purpose == "media-review":
        REQUIRED[purpose] |= S3
    signer = "BFF" if purpose == "console" else "BFF_" + verifier
    REQUIRED["bff"] |= {PREFIX + "BFF_" + verifier + "_ORIGIN", PREFIX + verifier + "_AUDIENCE",
                        PREFIX + signer + "_SIGNING_KID", PREFIX + signer + "_SIGNING_PRIVATE_KEY"}
BASE_PURPOSES = frozenset(REQUIRED)
RUNTIME_REQUIRED = {"runtime-assets": S3}
OPTIONAL = {purpose: set() for purpose in REQUIRED | RUNTIME_REQUIRED}
OPTIONAL["catalog"] = {PREFIX + "CATALOG_CONNECTIONS"}
for purpose in ("assets", "output-scan", "output-worker", "output-delivery", "media-review", "runtime-assets"):
    OPTIONAL[purpose] = {PREFIX + "ASSET_S3_SESSION_TOKEN", PREFIX + "ASSET_CONCURRENCY"}
OPTIONAL["runtime-assets"] |= {PREFIX + "RUNTIME_ARTIFACT_TTL_SECONDS"}
for purpose in ("media-api", "media-worker", "mcp-judge"):
    OPTIONAL[purpose] = PROVIDERS


def require(condition, message):
    if not condition:
        raise ValueError(message)


def private(path, directory=False):
    info = path.lstat()
    require((stat.S_ISDIR if directory else stat.S_ISREG)(info.st_mode), "configuration paths must be real files/directories")
    require(info.st_uid == os.geteuid(), "configuration owner mismatch")
    require(stat.S_IMODE(info.st_mode) == (0o700 if directory else 0o600), "configuration requires directory 0700/file 0600")


def text(value):
    require(isinstance(value, str) and value and "\x00" not in value, "configuration must be nonempty UTF-8 text without NUL")
    encoded = value.encode("utf-8")
    require(len(encoded) <= 96 * 1024, "configuration value exceeds limit")
    return encoded


def encode(purpose, config):
    required = (REQUIRED | RUNTIME_REQUIRED).get(purpose)
    require(required is not None and isinstance(config, dict), "unknown purpose or invalid configuration object")
    require(required <= config.keys() and config.keys() <= required | OPTIONAL[purpose], "missing or unknown purpose configuration name")
    for name, value in config.items():
        text(value)
        if name.endswith("_ORIGIN") or name.endswith("_ISSUER") or name.endswith("_S3_ENDPOINT"):
            parsed = urlsplit(value)
            require(parsed.scheme == "https" and parsed.hostname and not parsed.username and not parsed.password
                    and not parsed.query and not parsed.fragment, "configuration endpoint requires credential-free HTTPS")
            if not name.endswith("_ISSUER"):
                require(parsed.path in ("", "/"), "configuration endpoint must be an origin")
    encoded = b"".join(name.encode("ascii") + b"=" + base64.b64encode(text(config[name])) + b"\n" for name in sorted(config))
    require(len(encoded) <= LIMIT and all(len(line) <= LINE_LIMIT for line in encoded.splitlines()), "configuration file exceeds limit")
    return encoded


def new_file(path, data):
    descriptor, temporary = tempfile.mkstemp(prefix=".component-writing-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.link(temporary, path, follow_symlinks=False)
    finally:
        os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("configuration_json", type=Path)
    parser.add_argument("external_secret_dir", type=Path)
    args = parser.parse_args()
    source, root = args.configuration_json, args.external_secret_dir
    require(source.is_absolute() and source.resolve() == source and root.is_absolute() and root.resolve() == root, "paths must be absolute and canonical")
    private(source)
    private(root, True)
    require(not root.is_relative_to(Path(__file__).resolve().parents[2]), "configuration Secrets must be outside the repository")
    require(source.stat().st_size <= LIMIT, "configuration JSON exceeds limit")
    def object_pairs(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "duplicate configuration JSON name")
            result[key] = value
        return result
    configuration = json.loads(source.read_text(encoding="utf-8"), object_pairs_hook=object_pairs)
    require(isinstance(configuration, dict) and frozenset(configuration) in (BASE_PURPOSES, frozenset(RUNTIME_REQUIRED)), "all existing control-plane purposes must be explicitly configured")
    files = {purpose + ".env": encode(purpose, config) for purpose, config in configuration.items()}
    lock_path = root / ".component-config.lock"
    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "rb") as lock:
        private(lock_path)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        target, pending = root / "component-config", root / ".component-config.pending"
        if target.exists():
            private(target, True)
            require({entry.name for entry in target.iterdir()} == files.keys(), "component configuration set mismatch")
            for name, data in files.items():
                private(target / name)
                require((target / name).read_bytes() == data, "component configuration conflict; publish a separate release directory")
        else:
            if not pending.exists():
                pending.mkdir(mode=0o700)
            private(pending, True)
            for entry in pending.iterdir():
                private(entry)
                if entry.name.startswith(".component-writing-"):
                    entry.unlink()
                else:
                    require(entry.name in files and entry.read_bytes() == files[entry.name], "partial component configuration conflict")
            for name, data in files.items():
                if not (pending / name).exists():
                    new_file(pending / name, data)
            directory = os.open(pending, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
            os.rename(pending, target)
            directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
    print("Purpose-scoped component configuration verified; values were not printed")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, UnicodeError) as error:
        # Do not print decoder/JSON exceptions: their text can contain source values.
        print("Component configuration rejected; check schema, protected paths and release consistency", file=sys.stderr)
        sys.exit(1)
