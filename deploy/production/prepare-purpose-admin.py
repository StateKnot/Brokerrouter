#!/usr/bin/env python3
"""Reference the existing migration identity in dedicated ops-only service/passfile Secrets."""
import importlib.util
import os
from pathlib import Path
import re
import stat
import sys

spec = importlib.util.spec_from_file_location("purpose", Path(__file__).with_name("purpose-logins.py"))
purpose = importlib.util.module_from_spec(spec)
spec.loader.exec_module(purpose)


def main():
    if len(sys.argv) != 2:
        raise ValueError("expected external Secret directory")
    root = Path(sys.argv[1])
    with purpose.locked_root(root):
        source = root / "postgres-migration-password"
        info = source.lstat()
        purpose.require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid()
                        and stat.S_IMODE(info.st_mode) in (0o400, 0o600) and info.st_size == 64, "invalid existing migration Secret")
        password = source.read_bytes()
        purpose.require(re.fullmatch(b"[0-9a-f]{64}", password) is not None, "unsupported existing migration Secret")
        files = {
            "purpose-admin.service": b"[purpose]\nhost=postgres\nport=5432\ndbname=brokerrouter\nuser=brokerrouter_migration\nsslmode=verify-full\nsslrootcert=/run/secrets/pg_ca\npassfile=/run/secrets/purpose_admin_passfile\nconnect_timeout=5\n",
            "purpose-admin.passfile": b"postgres:5432:brokerrouter:brokerrouter_migration:" + password + b"\n",
        }
        # Check all existing files before publishing any missing file.
        for name, content in files.items():
            path = root / name
            if path.exists() or path.is_symlink():
                purpose.require(purpose.read(path) == content, "ops administrator Secret conflict")
        for name, content in files.items():
            if not (root / name).exists():
                purpose.atomic_new(root / name, content)
        purpose.sync_directory(root)
    print("Ops-only administrator references verified; existing migration identity unchanged")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError):
        print("Ops administrator references rejected; inspect protected existing Secret state", file=sys.stderr)
        sys.exit(1)
