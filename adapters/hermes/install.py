#!/usr/bin/env python3
"""Strict, resumable overlay installer for one audited Hermes revision.

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
from typing import Callable


REVISION = "67764dc0863349a384c16425e73ee8571f3a94b7"
TOOL_EXECUTOR_SHA256 = "1610a0893588cbc067aaf84b3d019a683cd83ba0fdb92d69baf4f7b9e6d121b9"
RUN_TURN_SHA256 = "dd203228bdfb7f69f1442adf4e24cc7e3196fb348b096ab6e7c2cb2bd2af5214"
RUN_TURN_RUNNER_SHA256 = "9bfa045616264e944b417ce3bfe0e302168c7b6d0218ceeb253b2dad2629e4ba"

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
            approval=getattr(agent, "_brokerrouter_approval_callback", None),
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

RUN_TURN_REPLACEMENTS = (
    (
        '''            event._heartbeat_execution_started = True
            agent_result = await self._run_agent(
''',
        '''            event._heartbeat_execution_started = True
            _event_metadata = getattr(event, "metadata", None)
            _brokerrouter_turn_binding = (
                _event_metadata.get("brokerrouter_turn")
                if isinstance(_event_metadata, dict) else None
            )
            agent_result = await self._run_agent(
''',
    ),
    (
        '''                persist_user_display_metadata={"gateway_input_owner": prepared.persistence_owner},
                message_type=event.message_type,
''',
        '''                persist_user_display_metadata={"gateway_input_owner": prepared.persistence_owner},
                message_type=event.message_type,
                brokerrouter_turn_binding=_brokerrouter_turn_binding,
''',
    ),
    (
        '''        # The one-slot progress/holder containers shared with the callbacks are TurnContext defaults.
        turn_ctx = TurnContext(
''',
        '''        # Governance metadata is not a TurnContext schema field. Remove it before the fixed
        # dataclass constructor and attach it as host-only state for the audited runner hook.
        brokerrouter_turn_binding = turn_params.pop("brokerrouter_turn_binding", None)
        # The one-slot progress/holder containers shared with the callbacks are TurnContext defaults.
        turn_ctx = TurnContext(
''',
    ),
    (
        '''            **{name: getattr(disp, name) for name in self._DISPLAY_TO_TURN_CTX}, **turn_params,
        )
        turn_runner = TurnRunner(self, turn_ctx)
''',
        '''            **{name: getattr(disp, name) for name in self._DISPLAY_TO_TURN_CTX}, **turn_params,
        )
        turn_ctx._brokerrouter_turn_binding = brokerrouter_turn_binding
        turn_runner = TurnRunner(self, turn_ctx)
''',
    ),
    (
        '''        persist_user_display_kind: Optional[str] = None, message_type: Optional[str] = None,
        persist_user_display_metadata: Optional[dict] = None,
''',
        '''        persist_user_display_kind: Optional[str] = None, message_type: Optional[str] = None,
        persist_user_display_metadata: Optional[dict] = None,
        brokerrouter_turn_binding: Optional[dict] = None,
''',
    ),
    (
        '''        if self._get_proxy_url():
            return await self._run_agent_via_proxy(
''',
        '''        if self._get_proxy_url():
            # The remote proxy process cannot prove this local Hermes turn identity. A governed
            # deployment must use the in-process runner until the proxy has its own attestation.
            from brokerrouter_hermes import reject_governed_proxy
            reject_governed_proxy()
            return await self._run_agent_via_proxy(
''',
    ),
    (
        '''            persist_user_display_kind=persist_user_display_kind,
            persist_user_display_metadata=persist_user_display_metadata,
        )
''',
        '''            persist_user_display_kind=persist_user_display_kind,
            persist_user_display_metadata=persist_user_display_metadata,
            brokerrouter_turn_binding=brokerrouter_turn_binding,
        )
''',
    ),
)

RUN_TURN_RUNNER_REPLACEMENTS = (
    (
        '''# Log-record parity with the origin module.
logger = logging.getLogger("gateway.run")
''',
        '''# Log-record parity with the origin module.
logger = logging.getLogger("gateway.run")

# A governed image must fail during Gateway import, before accepting a turn, if the
# fixed host overlay or adapter package differs from the build-time manifest.
from brokerrouter_hermes import verify_runtime_installation
verify_runtime_installation()
''',
    ),
    (
        '''        ctx.tools_holder[0] = getattr(agent, "tools", None)  # transcript logging
''',
        '''        ctx.tools_holder[0] = getattr(agent, "tools", None)  # transcript logging

        # Bind the BFF-created turn before history loading or the first model/tool invocation.
        # GovernanceHalt derives from BaseException so ordinary agent handlers cannot swallow it;
        # at this outer boundary convert it to the host's sanitized turn failure path.
        from brokerrouter_hermes import GovernanceHalt, bind_gateway_agent
        try:
            bind_gateway_agent(agent, ctx, self)
        except GovernanceHalt as governance_halt:
            raise RuntimeError(
                f"Brokerrouter governance stopped turn: {governance_halt.code}"
            ) from governance_halt
''',
    ),
)


def digest(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _replace_exact(source: bytes, replacements: tuple[tuple[str, str], ...], name: str) -> bytes:
    try:
        text = source.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise RuntimeError(f"{name} is not UTF-8") from exc
    for old, new in replacements:
        if text.count(old) != 1:
            raise RuntimeError(f"expected {name} patch anchor exactly once")
        text = text.replace(old, new)
    return text.encode("utf-8")


def patched_bytes(source: bytes) -> bytes:
    """Backwards-compatible name for the tool executor patch contract."""
    return _replace_exact(source, ((OLD, NEW),), "agent/tool_executor.py")


def patched_run_turn_bytes(source: bytes) -> bytes:
    return _replace_exact(source, RUN_TURN_REPLACEMENTS, "gateway/run_turn.py")


def patched_run_turn_runner_bytes(source: bytes) -> bytes:
    return _replace_exact(
        source, RUN_TURN_RUNNER_REPLACEMENTS, "gateway/run_turn_runner.py"
    )


PATCHES: tuple[tuple[str, str, Callable[[bytes], bytes]], ...] = (
    ("agent/tool_executor.py", TOOL_EXECUTOR_SHA256, patched_bytes),
    ("gateway/run_turn.py", RUN_TURN_SHA256, patched_run_turn_bytes),
    (
        "gateway/run_turn_runner.py",
        RUN_TURN_RUNNER_SHA256,
        patched_run_turn_runner_bytes,
    ),
)


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


def _backup(root: Path, relative: str) -> Path:
    return root / f"{relative}.brokerrouter-{REVISION[:12]}.bak"


def _inspect_file(
    root: Path, relative: str, original_hash: str, patcher: Callable[[bytes], bytes]
) -> dict[str, object]:
    target = root / relative
    if root.is_symlink() or target.is_symlink() or not target.is_file():
        raise RuntimeError(f"Hermes root/{relative} must be regular non-symlink paths")
    current = target.read_bytes()
    current_hash = digest(current)
    backup = _backup(root, relative)
    if current_hash == original_hash:
        original = current
        state = "compatible_unpatched"
    else:
        if backup.is_symlink() or not backup.is_file():
            raise RuntimeError(f"Hermes {relative} is neither the fixed baseline nor an audited installation")
        original = backup.read_bytes()
        if digest(original) != original_hash:
            raise RuntimeError(f"Hermes {relative} backup does not match the fixed baseline")
        state = "installed"
    expected = patcher(original)
    if state == "installed" and current_hash != digest(expected):
        raise RuntimeError(f"installed Hermes {relative} drifted from the audited patch")
    return {
        "relative": relative,
        "state": state,
        "current_sha256": current_hash,
        "original_sha256": original_hash,
        "patched_sha256": digest(expected),
        "original": original,
        "expected": expected,
    }


def inspect(root: Path) -> tuple[str, list[dict[str, object]]]:
    files = [_inspect_file(root, *spec) for spec in PATCHES]
    states = {item["state"] for item in files}
    if states == {"installed"}:
        state = "installed"
    elif states == {"compatible_unpatched"}:
        state = "compatible_unpatched"
    else:
        state = "partially_installed"
    return state, files


def apply(root: Path) -> dict[str, object]:
    _, files = inspect(root)
    for item in files:
        relative = str(item["relative"])
        if item["state"] == "compatible_unpatched":
            atomic_write(_backup(root, relative), item["original"], 0o644)
            atomic_write(root / relative, item["expected"], 0o644)

    for source in package_files():
        relative = source.relative_to(source.parents[1])
        atomic_write(root / relative, source.read_bytes(), 0o644)

    final_state, final_files = inspect(root)
    if final_state != "installed":
        raise RuntimeError("post-install host verification failed")
    manifest = {
        "schema": 2,
        "hermes_revision": REVISION,
        "host_files": {
            str(item["relative"]): {
                "original_sha256": item["original_sha256"],
                "patched_sha256": item["patched_sha256"],
            }
            for item in final_files
        },
        "adapter_files": {
            str(path.relative_to(path.parents[1])): digest(path.read_bytes())
            for path in package_files()
        },
    }
    atomic_write(
        root / "brokerrouter_hermes_install.json",
        (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode("utf-8"),
        0o644,
    )
    return manifest


def _public_file(item: dict[str, object]) -> dict[str, object]:
    return {key: value for key, value in item.items() if key not in {"original", "expected"}}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True, help="Hermes source root")
    parser.add_argument("--apply", action="store_true", help="apply overlay to an image build root")
    args = parser.parse_args()
    root = args.source.resolve()
    if args.apply:
        result: object = apply(root)
    else:
        state, files = inspect(root)
        result = {
            "state": state,
            "hermes_revision": REVISION,
            "host_files": [_public_file(item) for item in files],
        }
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"brokerrouter Hermes install refused: {exc}", file=sys.stderr)
        raise SystemExit(2)
