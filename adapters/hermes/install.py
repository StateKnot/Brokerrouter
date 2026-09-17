#!/usr/bin/env python3
"""Strict, idempotent overlay installer for Hermes revision 67764dc.

The running container is never modified by this repository on its own. Operators build a
new image, run this installer against its source tree, run the contract suite, and only then
roll out that immutable image.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile


REVISION = "67764dc0863349a384c16425e73ee8571f3a94b7"
TOOL_EXECUTOR_SHA256 = "1610a0893588cbc067aaf84b3d019a683cd83ba0fdb92d69baf4f7b9e6d121b9"

OLD = '''    if ref.name == "memory":
        agent._turns_since_memory = 0
    elif ref.name == "skill_manage":
        agent._iters_since_skill = 0

    _advance_start_order(lambda: _begin_tool_execution(agent, ref, display_index))
    return _run_with_activity_heartbeat(agent, ref.name, lambda: execute(ref.args))
'''

NEW = '''    dispatch_started = False

    def _dispatch_now():
        nonlocal dispatch_started

        if ref.name == "memory":
            agent._turns_since_memory = 0
        elif ref.name == "skill_manage":
            agent._iters_since_skill = 0

        def _begin_governed_execution():
            nonlocal dispatch_started
            dispatch_started = True
            _begin_tool_execution(agent, ref, display_index)

        _advance_start_order(_begin_governed_execution)
        return _run_with_activity_heartbeat(agent, ref.name, lambda: execute(ref.args))

    # Strong Brokerrouter boundary: all Relay/execution middleware, pre-tool modifiers,
    # scope checks and Hermes guardrails have already fixed ref.args. Import failure is
    # fail-closed (it propagates); it never falls through to _dispatch_now.
    from brokerrouter_hermes import GovernanceHalt, gate_for_agent
    from brokerrouter_hermes.gate import context_for_agent

    try:
        brokerrouter_gate = gate_for_agent(agent)
        if brokerrouter_gate is None:
            return _dispatch_now()
        return brokerrouter_gate.run_tool(
            context_for_agent(agent, ref),
            _dispatch_now,
            status=lambda message: agent._emit_status(message),
        )
    except GovernanceHalt as governance_halt:
        if not dispatch_started:
            _advance_start_order()
        from agent.tool_guardrails import ToolGuardrailDecision

        decision = ToolGuardrailDecision(
            action="halt",
            code=f"brokerrouter_{governance_halt.code}",
            message="Brokerrouter governance stopped this tool; review its operation state before retrying.",
            tool_name=ref.name,
        )
        agent._tool_guardrail_halt_decision = decision
        state.blocked = True
        return _blocked_tool_result(
            agent,
            ref,
            block_message=None,
            block_error_type="brokerrouter_governance_halt",
            guardrail_decision=decision,
        )
'''


def digest(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def patched_bytes(source: bytes) -> bytes:
    try:
        text = source.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise RuntimeError("tool_executor.py is not UTF-8") from exc
    if text.count(OLD) != 1:
        raise RuntimeError("expected Hermes dispatch anchor exactly once")
    return text.replace(OLD, NEW).encode("utf-8")


def atomic_write(path: Path, value: bytes, mode: int) -> None:
    path.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb", closefd=True) as stream:
            stream.write(value)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def package_files() -> list[Path]:
    package = Path(__file__).resolve().parent / "brokerrouter_hermes"
    files = sorted(path for path in package.rglob("*.py") if path.is_file())
    if not files:
        raise RuntimeError("adapter package is missing")
    return files


def inspect(root: Path) -> tuple[str, str, bytes, bytes]:
    target = root / "agent" / "tool_executor.py"
    if root.is_symlink() or target.is_symlink() or not target.is_file():
        raise RuntimeError("Hermes root/tool_executor.py must be regular non-symlink paths")
    current = target.read_bytes()
    current_hash = digest(current)
    if current_hash == TOOL_EXECUTOR_SHA256:
        expected = patched_bytes(current)
        return "compatible_unpatched", current_hash, current, expected
    backup = root / f"agent/tool_executor.py.brokerrouter-{REVISION[:12]}.bak"
    if not backup.is_file() or digest(backup.read_bytes()) != TOOL_EXECUTOR_SHA256:
        raise RuntimeError("Hermes tool executor is neither the fixed baseline nor an audited installation")
    expected = patched_bytes(backup.read_bytes())
    if current_hash != digest(expected):
        raise RuntimeError("installed Hermes tool executor drifted from the audited patch")
    return "installed", current_hash, backup.read_bytes(), expected


def apply(root: Path) -> dict[str, object]:
    state, current_hash, original, expected = inspect(root)
    target = root / "agent" / "tool_executor.py"
    backup = root / f"agent/tool_executor.py.brokerrouter-{REVISION[:12]}.bak"
    if state == "compatible_unpatched":
        atomic_write(backup, original, 0o644)
        atomic_write(target, expected, 0o644)

    destination = root / "brokerrouter_hermes"
    for source in package_files():
        relative = source.relative_to(source.parents[1])
        atomic_write(root / relative, source.read_bytes(), 0o644)

    manifest = {
        "schema": 1,
        "hermes_revision": REVISION,
        "tool_executor_original_sha256": TOOL_EXECUTOR_SHA256,
        "tool_executor_patched_sha256": digest(expected),
        "adapter_files": {
            str(path.relative_to(path.parents[1])): digest(path.read_bytes()) for path in package_files()
        },
    }
    atomic_write(
        root / "brokerrouter_hermes_install.json",
        (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode("utf-8"),
        0o644,
    )
    final_state, final_hash, _, _ = inspect(root)
    if final_state != "installed" or final_hash != manifest["tool_executor_patched_sha256"]:
        raise RuntimeError("post-install verification failed")
    return manifest


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True, help="Hermes source root")
    parser.add_argument("--apply", action="store_true", help="apply overlay to an image build root")
    args = parser.parse_args()
    root = args.source.resolve()
    if args.apply:
        result: object = apply(root)
    else:
        state, current_hash, _, expected = inspect(root)
        result = {
            "state": state,
            "hermes_revision": REVISION,
            "current_sha256": current_hash,
            "patched_sha256": digest(expected),
        }
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"brokerrouter Hermes install refused: {exc}", file=sys.stderr)
        raise SystemExit(2)
