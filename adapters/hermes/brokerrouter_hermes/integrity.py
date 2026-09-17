from __future__ import annotations

import hashlib
import hmac
import json
from pathlib import Path
import threading
from typing import Any

from .config import Config


REVISION = "67764dc0863349a384c16425e73ee8571f3a94b7"
HOST_FILES = {
    "agent/tool_executor.py": {
        "original_sha256": "1610a0893588cbc067aaf84b3d019a683cd83ba0fdb92d69baf4f7b9e6d121b9",
        "patched_sha256": "50614ef6af1fd2045e06eebfaba63d8488f9f9c2fe1b2d5493f7261fea3de492",
    },
    "gateway/run_turn.py": {
        "original_sha256": "dd203228bdfb7f69f1442adf4e24cc7e3196fb348b096ab6e7c2cb2bd2af5214",
        "patched_sha256": "071bb0fcd415528f7a21fdd7f3ffbc11b593e72c07e8606288e8f7cd38e7fa7f",
    },
    "gateway/run_turn_runner.py": {
        "original_sha256": "9bfa045616264e944b417ce3bfe0e302168c7b6d0218ceeb253b2dad2629e4ba",
        "patched_sha256": "1aac0ad4dcb229f4b0dc9827a4a9cf3647378340d6977f381ab91b80ea44efcf",
    },
}
MAX_MANIFEST_BYTES = 1024 * 1024
_verified_roots: set[Path] = set()
_lock = threading.Lock()


def _digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _same(left: Any, right: Any) -> bool:
    return isinstance(left, str) and isinstance(right, str) and hmac.compare_digest(left, right)


def verify_root(root: Path) -> None:
    root = root.resolve()
    manifest_path = root / "brokerrouter_hermes_install.json"
    if manifest_path.is_symlink() or not manifest_path.is_file():
        raise RuntimeError("Brokerrouter Hermes installation manifest is missing")
    raw = manifest_path.read_bytes()
    if not raw or len(raw) > MAX_MANIFEST_BYTES:
        raise RuntimeError("Brokerrouter Hermes installation manifest has an invalid size")
    try:
        manifest = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise RuntimeError("Brokerrouter Hermes installation manifest is invalid") from exc
    if (
        not isinstance(manifest, dict)
        or manifest.get("schema") != 2
        or manifest.get("hermes_revision") != REVISION
        or manifest.get("host_files") != HOST_FILES
    ):
        raise RuntimeError("Brokerrouter Hermes installation manifest is incompatible")

    for relative, expected in HOST_FILES.items():
        target = root / relative
        if target.is_symlink() or not target.is_file() or not _same(
            _digest(target), expected["patched_sha256"]
        ):
            raise RuntimeError(f"Brokerrouter Hermes host file failed verification: {relative}")

    package = root / "brokerrouter_hermes"
    actual_paths = sorted(path for path in package.rglob("*.py") if path.is_file())
    actual_names = {str(path.relative_to(root)) for path in actual_paths}
    expected_files = manifest.get("adapter_files")
    if not isinstance(expected_files, dict) or set(expected_files) != actual_names:
        raise RuntimeError("Brokerrouter Hermes adapter file set failed verification")
    for path in actual_paths:
        relative = str(path.relative_to(root))
        if path.is_symlink() or not _same(_digest(path), expected_files.get(relative)):
            raise RuntimeError(f"Brokerrouter Hermes adapter failed verification: {relative}")


def verify_runtime_installation() -> None:
    """Fail at Gateway import when governance is required and the image drifted."""
    if Config.from_env() is None:
        return
    root = Path(__file__).resolve().parents[1]
    with _lock:
        if root in _verified_roots:
            return
        verify_root(root)
        _verified_roots.add(root)
