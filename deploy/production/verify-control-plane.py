#!/usr/bin/env python3
"""Read-only preflight for the fixed control-plane deployment release."""
import argparse
import base64
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


cfg = module("cfg", "control-plane-config.py")
purpose = module("purpose", "purpose-logins.py")
edge = module("edge", "render-control-edge.py")


def run(arguments, data=None):
    result = subprocess.run(arguments, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    cfg.require(result.returncode == 0, "preflight command failed")
    return result.stdout


def read(path):
    cfg.private(path)
    cfg.require(path.stat().st_size <= cfg.LIMIT, "configuration exceeds bound")
    return path.read_bytes()


def public_key(private):
    return run(["openssl", "pkey", "-pubout"], private.encode())


def public_pem(pem):
    cfg.require(isinstance(pem, str) and pem and len(pem) <= cfg.LINE_LIMIT, "invalid public PEM")
    return run(["openssl", "pkey", "-pubin", "-pubout"], pem.encode())


def keyring(config, prefix):
    active = config["BROKERROUTER_ACTIVE_RESULT_KEY"] if prefix == "RESULT" else config["BROKERROUTER_BFF_ACTIVE_KEY"]
    keys = json.loads(config["BROKERROUTER_" + prefix + "_KEYS"])
    cfg.require(isinstance(keys, dict) and 1 <= len(keys) <= 16 and active in keys
                and all(re.fullmatch(r"[A-Za-z0-9_-]{1,32}", name) and isinstance(value, str)
                        and re.fullmatch(r"[0-9a-fA-F]{64}", value) for name, value in keys.items()), "invalid keyring")
    return {name: value.lower() for name, value in keys.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("external_secret_directory", type=Path)
    parser.add_argument("external_edge_directory", type=Path)
    parser.add_argument("console_image")
    parser.add_argument("preview_image")
    parser.add_argument("provisioner_image")
    args = parser.parse_args()
    root, edge_root = args.external_secret_directory, args.external_edge_directory
    for path in (root, edge_root):
        cfg.require(path.is_absolute() and path.resolve() == path, "paths must be canonical")
        cfg.private(path, True)
    cfg.require(not root.is_relative_to(Path(__file__).resolve().parents[2])
                and not edge_root.is_relative_to(Path(__file__).resolve().parents[2]), "release state must be external")
    purpose.validate(root / "purpose-logins")
    for name in ("pg-ca.crt", "bff-ca.crt", "asset-s3-ca.crt"):
        ca = root / name
        info = ca.lstat()
        cfg.require(stat.S_ISREG(info.st_mode) and stat.S_IMODE(info.st_mode) in (0o444, 0o644)
                    and info.st_size <= cfg.LIMIT, "CA bundle must be bounded, regular and readable after UID drop")
        run(["openssl", "x509", "-noout"], ca.read_bytes())
    component = root / "component-config"
    cfg.private(component, True)
    cfg.require({file.name for file in component.iterdir()} == {name + ".env" for name in cfg.REQUIRED}, "invalid component release set")
    configurations = {}
    for name in cfg.REQUIRED:
        content = read(component / (name + ".env"))
        values = {}
        for line in content.splitlines():
            key, encoded = line.split(b"=", 1)
            key = key.decode("ascii")
            cfg.require(key not in values, "duplicate configuration name")
            decoded = base64.b64decode(encoded, validate=True)
            cfg.require(base64.b64encode(decoded) == encoded, "noncanonical configuration encoding")
            values[key] = decoded.decode("utf-8")
        cfg.require(cfg.encode(name, values) == content, "noncanonical configuration file")
        configurations[name] = values
    origins = json.loads(read(edge_root / "origins.json"))
    cfg.require(set(origins) == {"console_origin", "preview_origin"}, "invalid edge manifest")
    main_host, preview_host = edge.origin(origins["console_origin"]), edge.origin(origins["preview_origin"])
    bff = configurations["bff"]
    cfg.require(bff["BROKERROUTER_BFF_ORIGIN"] == origins["console_origin"]
                and bff["BROKERROUTER_BFF_MEDIA_PREVIEW_ORIGIN"] == origins["preview_origin"], "edge/BFF origin mismatch")
    audiences, signing_keys = set(), set()
    for name in ("console", "catalog", "settings", "approvals", "media-review"):
        prefix = name.replace("-", "_").upper()
        signer = "BFF" if name == "console" else "BFF_" + prefix
        kid = bff["BROKERROUTER_" + signer + "_SIGNING_KID"]
        expected = public_key(bff["BROKERROUTER_" + signer + "_SIGNING_PRIVATE_KEY"])
        verifier = configurations[name]
        public = json.loads(verifier["BROKERROUTER_" + prefix + "_PUBLIC_KEYS"])
        cfg.require(isinstance(public, dict) and kid in public and public_pem(public[kid]) == expected, "purpose signer/verifier mismatch")
        audience = verifier["BROKERROUTER_" + prefix + "_AUDIENCE"]
        cfg.require(verifier["BROKERROUTER_" + prefix + "_ISSUER"] == origins["console_origin"]
                    and bff["BROKERROUTER_" + prefix + "_AUDIENCE"] == audience
                    and audience not in audiences and expected not in signing_keys
                    and bff["BROKERROUTER_BFF_" + prefix + "_ORIGIN"] == "https://" + name + ".brokerrouter.internal:8443", "purpose trust scope mismatch")
        audiences.add(audience)
        signing_keys.add(expected)
    cipher = keyring(bff, "BFF")
    results = [keyring(values, "RESULT") for values in configurations.values() if "BROKERROUTER_RESULT_KEYS" in values]
    cfg.require(all(not set(cipher.values()) & set(ring.values()) for ring in results), "BFF cookie keys cannot decrypt governed results")
    cfg.require(all(ring == results[0] for ring in results)
                and len({values["BROKERROUTER_ACTIVE_RESULT_KEY"] for values in configurations.values() if "BROKERROUTER_RESULT_KEYS" in values}) == 1,
                "governed result keyrings must preserve shared historical decryption")
    storage = [values for values in configurations.values() if "BROKERROUTER_ASSET_S3_ENDPOINT" in values]
    cfg.require(all(all(values["BROKERROUTER_ASSET_S3_" + key] == storage[0]["BROKERROUTER_ASSET_S3_" + key]
                        for key in ("ENDPOINT", "BUCKET", "REGION")) for values in storage), "private asset namespace mismatch")
    # HUMAN ingress/delivery remains a separate signer. The preflight never creates that private key.
    for name in ("control", "output-delivery"):
        human = json.loads(configurations[name]["BROKERROUTER_HUMAN_PUBLIC_KEYS"])
        cfg.require(isinstance(human, dict) and human and all(public_pem(value) not in signing_keys for value in human.values()), "human ingress trust cannot reuse BFF purpose keys")
    for name in ("purpose-admin.service", "purpose-admin.passfile"):
        read(root / name)
    for certificate, hostname in (("control-plane", "console.brokerrouter.internal"), ("console", main_host), ("preview", preview_host)):
        pem = read(root / (certificate + "-fullchain.pem"))
        key = read(root / (certificate + "-privkey.pem"))
        certificate_key = run(["openssl", "x509", "-pubkey", "-noout"], pem)
        cfg.require(certificate_key == public_key(key.decode()), "TLS certificate/private key mismatch")
        run(["openssl", "x509", "-noout", "-checkend", "604800"], pem)
        # verify_hostname tests trust and SAN together; the browser checks public roots separately.
        if certificate == "control-plane":
            ca = root / "bff-ca.crt"
            info = ca.lstat()
            cfg.require(stat.S_ISREG(info.st_mode) and info.st_mode & 0o022 == 0 and info.st_size <= cfg.LIMIT, "invalid public BFF CA")
            for name in ("control", "assets", "media-api", "output-scan", "output-delivery", "console", "catalog", "settings", "approvals", "media-review"):
                run(["openssl", "verify", "-CAfile", str(ca), "-verify_hostname", name + ".brokerrouter.internal", str(root / "control-plane-fullchain.pem")])
        else:
            # x509's checkhost output is deterministic but its exit code alone is not portable.
            output = run(["openssl", "x509", "-noout", "-checkhost", hostname], pem)
            cfg.require(b"does match certificate" in output, "public edge TLS hostname mismatch")
    for name in ("console-nginx.conf", "preview-nginx.conf"):
        read(edge_root / name)
    for image in (args.console_image, args.preview_image, args.provisioner_image):
        cfg.require(re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", image), "release images must use immutable digests")
        run(["docker", "image", "inspect", image])
    label = run(["docker", "image", "inspect", args.preview_image, "--format", '{{index .Config.Labels "io.brokerrouter.preview.parent-origin"}}']).decode().strip()
    cfg.require(label == origins["console_origin"], "immutable preview parent origin differs from edge/BFF")
    print("Control-plane release preflight passed; real OIDC membership, S3/scanner and capacity acceptance remain required")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, UnicodeError, KeyError, subprocess.TimeoutExpired):
        print("Control-plane preflight rejected; inspect protected release configuration and immutable artifacts", file=sys.stderr)
        sys.exit(1)
