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
        "patched_sha256": "4037d2bacf6d2abb2344e8090cb2257e35a0d0816b7da14a8a020d885730a9e8"
    },
    "gateway/run_turn.py": {
        "original_sha256": "dd203228bdfb7f69f1442adf4e24cc7e3196fb348b096ab6e7c2cb2bd2af5214",
        "patched_sha256": "a518cb574737ae85bc58f3b52de9b8a69d4d949a02031772a3e73ec8b71167c3"
    },
    "gateway/run_turn_runner.py": {
        "original_sha256": "9bfa045616264e944b417ce3bfe0e302168c7b6d0218ceeb253b2dad2629e4ba",
        "patched_sha256": "b961942e135109c29c969fcdad7ea3df1ff21ddf92a78f1c767aa893905151df"
    },
    "model_tools.py": {
        "original_sha256": "875512e422e3e0d1502f7f7e127d47288bf1f1b350b4442ad5fc210e526b3f67",
        "patched_sha256": "c9c19a50760e1a1ec7e5884397900ea1a8bfbaf2e5c90bb839f222029276ad06"
    },
    "tools/registry.py": {
        "original_sha256": "ca02644ea36dfa08ab7e12893d283a3b12549a5ffd33ea72c29c632a94e7b201",
        "patched_sha256": "38c5da3fc20d0ac8c66133216d73a0ae22b46791852c8c4f95482bc0400e5bac"
    },
    "gateway/platforms/base.py": {
        "original_sha256": "14f2bcd915e97ecb2d14cae290e1dc07d36c3247ac9b7599f38c3175670ec52a",
        "patched_sha256": "8a1c532d3cd7f8be5c26bd29ccf4c62276baa9d805362057459bdc6f0e8fa674"
    },
    "gateway/run_voice.py": {
        "original_sha256": "236853144929c0165f766c9c44927c8acb0c2e575776b68b8a5f6dd0f9e8651f",
        "patched_sha256": "401f2a830b4fc00a43e84c29ff0f629475d13573ee51c197d2e4936047f96163"
    },
    "gateway/delivery_ledger.py": {
        "original_sha256": "b2656cd6429fb848f8df405fd7bb5e50f2ea6cb0620141c59ccccc3f7c314ab7",
        "patched_sha256": "d5bafbd4fc6e1598f31d5b4e3e93c27c22e0f9d48c19446fa941e4abb463df63"
    },
    "agent/agent_runtime_helpers.py": {
        "original_sha256": "328f3f8ee5e6956ae530007ec0c22457e3bb681b6125c918cd43ea7124b1767e",
        "patched_sha256": "18d611d086aac9991c787d7fd751dbeccd814c4bff8f9d7e7c83d8728481c485"
    },
    "agent/turn_api_call.py": {
        "original_sha256": "5dead01b498131df2ee987e431ded73ca21313ef1a52633e9aa802efc2aa00b5",
        "patched_sha256": "61993076054bf2b1a6dac58f51c3d02c655d4bf389da9d8c69b01708fe436ca0"
    },
    "agent/turn_final_response.py": {
        "original_sha256": "ff414924aa9ffc30fb64b91a602f4621307f22a9f6e5aa3f88e916eabbadbce1",
        "patched_sha256": "69b424ab6c0f7d57e09d31c78f5c2d42cf6fdb4353b88c526157b0fbb4f33c13"
    },
    "agent/turn_finalizer.py": {
        "original_sha256": "4b79fa4eeb325095b4664df6085df54385d0f38cb6e610c6145c91680f46badd",
        "patched_sha256": "a303c891581daaeb7a596a7d18c154ea9c2e47b4408d68008a698b32588a3645"
    },
    "agent/session_persistence.py": {
        "original_sha256": "83ee5c9f48a7790deb3f771e45990180e1fd714c7b1fabc24303944f17773a1b",
        "patched_sha256": "212c8d3606eb445c5f9a4b77a20e01b41c41947bee76e676eb4e9b5b3b599694"
    },
    "agent/stream_delivery.py": {
        "original_sha256": "38d3fda9b9ab09048c14154588e30fa695db89db20efb06fdeb6ffdbf00721c3",
        "patched_sha256": "e842c348c356fbf340f4304e82a1f1c647b570c53d1d15a492a50f457373de54"
    },
    "gateway/session_lifecycle.py": {
        "original_sha256": "fc8a17d5dd5cbc40fd51abf2b4c75b6c5dd5c1bf1cecbd5f7f4deea8e1f58071",
        "patched_sha256": "cc2932d5fbd8c6792bc6c02577c1d6156919d0118d90232e9af4a9ab7f821e25"
    },
    "agent/auxiliary_client.py": {
        "original_sha256": "ca46e494e1e518ca793c65c3b4707ae2f3f38c27ee142b2076ff050978d1065b",
        "patched_sha256": "75bbcfd5f53d475f61d0bff1a47f078cb7f9f0f34a915d315f335b812c9d9e75"
    },
    "agent/title_generator.py": {
        "original_sha256": "1eb801e5f020bb9f32373ed52afb68ab64e384618649260fdf82b12370c13d4b",
        "patched_sha256": "faf086e96d272cb14cd5afaf0b29f07ea4cea7649fb1d1b966adeb35851e2624"
    },
    "agent/chat_completion_helpers.py": {
        "original_sha256": "2e5308b231ac50cb52b90439f17d7a5e05618d46dd23cffcf45cafb5d77e3073",
        "patched_sha256": "3011f24301096af685c540b749f5e9a1f6624e8915ae14ead494f10f024b900a"
    },
    "gateway/run_inbound.py": {
        "original_sha256": "d20049510d55674bb60d20cb48f09b71f829880850467b18a9c55a8188e2f9f5",
        "patched_sha256": "42ebe9fba46e68b096154750ae81a1cbabc0eaf72a852081027ab62f91852f82"
    }
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
