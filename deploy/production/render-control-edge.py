#!/usr/bin/env python3
"""Publish an immutable edge configuration pair for independently issued HTTPS origins."""
import argparse
import json
import os
from pathlib import Path
import re
import sys
from urllib.parse import urlsplit

import importlib.util
spec = importlib.util.spec_from_file_location("control_config", Path(__file__).with_name("control-plane-config.py"))
configuration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(configuration)


def origin(value):
    parsed = urlsplit(value)
    configuration.require(parsed.scheme == "https" and parsed.netloc == parsed.hostname
                          and parsed.path == "" and not parsed.query and not parsed.fragment
                          and re.fullmatch(r"[a-z0-9]+(?:[.-][a-z0-9]+)*\.[a-z]{2,63}", parsed.hostname or ""),
                          "edge requires an exact lower-case HTTPS DNS origin without a port")
    return parsed.hostname


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("console_origin")
    parser.add_argument("preview_origin")
    parser.add_argument("external_config_directory", type=Path)
    args = parser.parse_args()
    main_host, preview_host = origin(args.console_origin), origin(args.preview_origin)
    # A deployment operator must additionally confirm the current Public Suffix List.
    # This conservative structural check rejects same-apex and parent/child hosts.
    configuration.require(main_host.split(".")[-2:] != preview_host.split(".")[-2:]
                          and not main_host.endswith("." + preview_host)
                          and not preview_host.endswith("." + main_host), "preview requires a separate site")
    root = args.external_config_directory
    configuration.require(root.is_absolute() and root.resolve() == root, "edge directory must be absolute and canonical")
    configuration.private(root, True)
    repository = Path(__file__).resolve().parents[2]
    configuration.require(not root.is_relative_to(repository), "render edge configuration outside the repository")
    files = {}
    for component in ("console", "preview"):
        source = (repository / "deploy" / component / "nginx.conf").read_text()
        source = source.replace("console.example.invalid", main_host).replace("review.example-media.invalid", preview_host)
        source = source.replace("worker_processes auto;", "user nginx;\nworker_processes 1;")
        source = source.replace("default_type application/octet-stream;", "default_type application/octet-stream;\n    client_body_temp_path /tmp/client-body;\n    proxy_temp_path /tmp/proxy;\n    fastcgi_temp_path /tmp/fastcgi;\n    uwsgi_temp_path /tmp/uwsgi;\n    scgi_temp_path /tmp/scgi;")
        source = source.replace("/etc/brokerrouter/tls/fullchain.pem", "/run/secrets/tls_fullchain")
        source = source.replace("/etc/brokerrouter/tls/privkey.pem", "/run/secrets/tls_private_key")
        source = source.replace("proxy_pass http://127.0.0.1:8087;", "proxy_pass http://bff:8087;")
        files[component + "-nginx.conf"] = source.encode()
    files["origins.json"] = (json.dumps({"console_origin": args.console_origin, "preview_origin": args.preview_origin}, sort_keys=True) + "\n").encode()
    target = root / "control-edge"
    pending = root / ".control-edge.pending"
    lock_path = root / ".control-edge.lock"
    import fcntl
    descriptor = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "rb") as lock:
        configuration.private(lock_path)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if target.exists():
            configuration.private(target, True)
            configuration.require({entry.name for entry in target.iterdir()} == files.keys(), "edge release set mismatch")
            for name, content in files.items():
                configuration.private(target / name)
                configuration.require((target / name).read_bytes() == content, "edge release conflict")
        else:
            if not pending.exists():
                pending.mkdir(mode=0o700)
            configuration.private(pending, True)
            for entry in pending.iterdir():
                configuration.private(entry)
                if entry.name.startswith(".component-writing-"):
                    entry.unlink()
                else:
                    configuration.require(entry.name in files and entry.read_bytes() == files[entry.name], "partial edge release conflict")
            for name, content in files.items():
                if not (pending / name).exists():
                    configuration.new_file(pending / name, content)
            for directory in (pending, root):
                descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(descriptor)
                finally:
                    os.close(descriptor)
            os.rename(pending, target)
            descriptor = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
    print("Console/preview edge configuration verified; TLS material remains external")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, UnicodeError):
        print("Edge configuration rejected; check origins, protected paths and release consistency", file=sys.stderr)
        sys.exit(1)
