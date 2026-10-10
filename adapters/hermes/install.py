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
MODEL_TOOLS_SHA256 = "875512e422e3e0d1502f7f7e127d47288bf1f1b350b4442ad5fc210e526b3f67"
REGISTRY_SHA256 = "ca02644ea36dfa08ab7e12893d283a3b12549a5ffd33ea72c29c632a94e7b201"
PLATFORM_BASE_SHA256 = "14f2bcd915e97ecb2d14cae290e1dc07d36c3247ac9b7599f38c3175670ec52a"
RUN_VOICE_SHA256 = "236853144929c0165f766c9c44927c8acb0c2e575776b68b8a5f6dd0f9e8651f"
DELIVERY_LEDGER_SHA256 = "b2656cd6429fb848f8df405fd7bb5e50f2ea6cb0620141c59ccccc3f7c314ab7"

OLD = '''    if ref.name == "memory":
        agent._turns_since_memory = 0
    elif ref.name == "skill_manage":
        agent._iters_since_skill = 0

    _advance_start_order(lambda: _begin_tool_execution(agent, ref, display_index))
    return _run_with_activity_heartbeat(agent, ref.name, lambda: execute(ref.args))
'''

NEW = '''    dispatch_started = False
    frozen_arguments = None

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
        if frozen_arguments is None:
            execution_args = ref.args
        else:
            # Display/preflight callbacks and the executor cannot mutate each other's
            # arguments or the gate's checked context. Bytes fix the actual dispatch.
            ref.args = json.loads(frozen_arguments)
            state.args = ref.args
            execution_args = json.loads(frozen_arguments)
            from brokerrouter_hermes.native import quarantine_tool_output

            with quarantine_tool_output():
                return _run_with_activity_heartbeat(agent, ref.name, lambda: execute(execution_args))
        return _run_with_activity_heartbeat(agent, ref.name, lambda: execute(execution_args))

    # Strong Brokerrouter input boundary: pre-tool modifiers, scope checks and Hermes
    # guardrails have already fixed ref.args. Import failure is
    # fail-closed (it propagates); it never falls through to _dispatch_now.
    from brokerrouter_hermes import GovernanceHalt, gate_for_agent
    from brokerrouter_hermes.gate import _canonical, context_for_agent

    try:
        brokerrouter_gate = gate_for_agent(agent)
        if brokerrouter_gate is None:
            return _dispatch_now()
        frozen_arguments = _canonical(ref.args)
        ref.args = json.loads(frozen_arguments)
        state.args = ref.args
        checked_ref = _ToolCallRef(
            ref.name, json.loads(frozen_arguments), ref.task_id, ref.call_id, ref.trace,
        )
        return brokerrouter_gate.run_execution(
            context_for_agent(agent, checked_ref),
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

TOOL_EXECUTOR_REPLACEMENTS = (
    (OLD, NEW),
    (
        '''    trace: list

    def middleware_kwargs''',
        '''    trace: list
    brokerrouter_finalized: bool = False
    brokerrouter_recovered: bool = False

    def middleware_kwargs''',
    ),
    (
        '''    blocked: bool
    dispatched: bool


class _ToolTimeoutResult''',
        '''    blocked: bool
    dispatched: bool
    brokerrouter_finalized: bool = False
    brokerrouter_recovered: bool = False


class _ToolTimeoutResult''',
    ),
    (
        '''    from agent import relay_tools
    from hermes_cli.middleware import (
        apply_tool_request_middleware,
        run_tool_execution_middleware,
    )

    trace = middleware_trace if middleware_trace is not None else []
''',
        '''    from brokerrouter_hermes import GovernanceHalt
    from brokerrouter_hermes.native import NativeInvocation

    trace = middleware_trace if middleware_trace is not None else []
''',
    ),
    (
        '''    state.result, _relay_args = relay_tools.execute(
''',
        '''    origin_ref = _ToolCallRef(function_name, function_args, effective_task_id, tool_call_id, trace)
    try:
        native = NativeInvocation.for_call(agent, origin_ref)
        if native is not None:
            if scope_block is not None:
                # A frozen result does not override a current Hermes scope denial.
                # This existing branch only emits safe blocked metadata; no tool runs.
                state.result = _authorized_dispatch(function_args)
                return state
            # No optional Python Relay/request/execution continuations are trusted
            # in required mode: they can publish raw bytes themselves. Core Hermes
            # scope, pre-tool modifiers and input guardrails still run below.
            recovered = native.open()
            if recovered is not None:
                if begin_execution is not None:
                    begin_execution(None)
                state.result = recovered
                state.brokerrouter_finalized = state.brokerrouter_recovered = True
                return state
            state.result = _authorized_dispatch(function_args)
            if not state.blocked:
                state.result = native.finish(
                    _ToolCallRef(function_name, state.args, effective_task_id, tool_call_id, trace),
                    state.result,
                )
                state.brokerrouter_finalized = True
            return state
    except GovernanceHalt as governance_halt:
        if not state.dispatched and begin_execution is not None:
            begin_execution(None)
        from agent.tool_guardrails import ToolGuardrailDecision

        decision = ToolGuardrailDecision(
            action="halt", code=f"brokerrouter_{governance_halt.code}",
            message="Brokerrouter governance stopped this tool; review its operation state before retrying.",
            tool_name=function_name,
        )
        agent._tool_guardrail_halt_decision = decision
        state.blocked = True
        state.result = _blocked_tool_result(
            agent, origin_ref, block_message=None,
            block_error_type="brokerrouter_governance_halt", guardrail_decision=decision,
        )
        return state

    from agent import relay_tools
    from hermes_cli.middleware import (
        apply_tool_request_middleware,
        run_tool_execution_middleware,
    )
    state.result, _relay_args = relay_tools.execute(
''',
    ),
    (
        '''            result, ref.args, ref.trace = managed.result, managed.args, managed.middleware_trace
            blocked, dispatched = managed.blocked, managed.dispatched
''',
        '''            result, ref.args, ref.trace = managed.result, managed.args, managed.middleware_trace
            ref.brokerrouter_finalized = managed.brokerrouter_finalized
            ref.brokerrouter_recovered = managed.brokerrouter_recovered
            blocked, dispatched = managed.blocked, managed.dispatched
''',
    ),
    (
        '''        if not blocked and not dispatched:
            ref.emit_post(agent, result, duration_ms=int(duration * 1000))
''',
        '''        if not blocked and not ref.brokerrouter_recovered and (not dispatched or ref.brokerrouter_finalized):
            ref.emit_post(agent, result, duration_ms=int(duration * 1000))
''',
    ),
    (
        '''    ref.args, ref.trace, function_result = managed.args, managed.middleware_trace, managed.result
''',
        '''    ref.args, ref.trace, function_result = managed.args, managed.middleware_trace, managed.result
    ref.brokerrouter_finalized = managed.brokerrouter_finalized
    ref.brokerrouter_recovered = managed.brokerrouter_recovered
''',
    ),
    (
        '''    if not managed.blocked and not _execution_timed_out:
''',
        '''    if not managed.blocked and not _execution_timed_out and not managed.brokerrouter_recovered:
''',
    ),
    (
        '''        if not blocked:
            function_result = agent._append_guardrail_observation(
''',
        '''        if not blocked and not ref.brokerrouter_finalized:
            function_result = agent._append_guardrail_observation(
''',
    ),
    (
        '''        if not blocked:
            try:
                agent._record_file_mutation_result''',
        '''        if not blocked and not ref.brokerrouter_recovered:
            try:
                agent._record_file_mutation_result''',
    ),
    (
        '''    subdir_hints = agent._subdirectory_hints.check_tool_call(function_name, function_args)
''',
        '''    subdir_hints = (
        None if ref.brokerrouter_finalized else
        agent._subdirectory_hints.check_tool_call(function_name, function_args)
    )
''',
    ),
    (
        '''    if not blocked:
        # ``tool.completed`` projects AFTER''',
        '''    if not blocked and not ref.brokerrouter_recovered:
        # ``tool.completed`` projects AFTER''',
    ),
    (
        '''    if not blocked and agent.tool_complete_callback:
''',
        '''    if not blocked and not ref.brokerrouter_recovered and agent.tool_complete_callback:
''',
    ),
)

MODEL_TOOLS_REPLACEMENTS = (
    (
        '''    first string return wins. Gated on has_hook so the no-listener path is cheap.
    """
    try:
''',
        '''    first string return wins. Gated on has_hook so the no-listener path is cheap.
    """
    from brokerrouter_hermes.native import output_is_quarantined
    if output_is_quarantined():
        # Required mode has no trusted arbitrary result transformers. The native
        # guardrail observer and hints run before the dedicated final freeze.
        return result
    try:
''',
    ),
    (
        '''    except Exception as e:
        error_msg = f"Error executing {function_name}: {str(e)}"
''',
        '''    except Exception as e:
        from brokerrouter_hermes.native import output_is_quarantined
        if output_is_quarantined():
            from brokerrouter_hermes import GovernanceHalt
            raise GovernanceHalt("native_tool_execution_failed") from e
        error_msg = f"Error executing {function_name}: {str(e)}"
''',
    ),
)

REGISTRY_REPLACEMENTS = (
    (
        '''        except Exception as e:
            # exc_info already renders the exception, so keep the message copy bounded.
''',
        '''        except Exception as e:
            from brokerrouter_hermes.native import output_is_quarantined
            if output_is_quarantined():
                from brokerrouter_hermes import GovernanceHalt
                raise GovernanceHalt("native_tool_execution_failed") from e
            # exc_info already renders the exception, so keep the message copy bounded.
''',
    ),
)

PLATFORM_BASE_REPLACEMENTS = (
    (
        '''    def __init__(self, config: PlatformConfig, platform: Platform):
        self.config = config
''',
        '''    def __init__(self, config: PlatformConfig, platform: Platform):
        # Binary output remains unsupported until isolated byte references have
        # their own release grant. Close the actual APIs, including overrides.
        from brokerrouter_hermes.native import restrict_binary_capabilities
        restrict_binary_capabilities(self)
        self.config = config
''',
    ),
    (
        '''        delivery_adapter = self._final_delivery_adapter(event.source)
        logger.info("[%s] Sending response (%d chars) to %s", delivery_adapter.name,
''',
        '''        from brokerrouter_hermes.publication import send_final_once
        governed = await send_final_once(
            self, event, session_key, text_content, metadata,
            reply_to=reply_to, is_ephemeral_response=is_ephemeral_response,
        )
        if governed is not None:
            return governed
        delivery_adapter = self._final_delivery_adapter(event.source)
        logger.info("[%s] Sending response (%d chars) to %s", delivery_adapter.name,
''',
    ),
)


DELIVERY_LEDGER_REPLACEMENTS = (
    (
        '''    because the runtime sweep matches profiles exactly and could otherwise never claim it."""
    now, (pid, started) = now if now is not None else time.time(), _owner_stamp()
''',
        '''    because the runtime sweep matches profiles exactly and could otherwise never claim it."""
    from brokerrouter_hermes.publication import hold_required_recovery
    import gateway.delivery_ledger as _ledger
    if hold_required_recovery(_ledger):
        return []
    now, (pid, started) = now if now is not None else time.time(), _owner_stamp()
''',
    ),
    (
        '''    infer): the reconnect one, or the rate-limit one for a flood-refused row."""
    now, (pid, started) = now if now is not None else time.time(), _owner_stamp()
''',
        '''    infer): the reconnect one, or the rate-limit one for a flood-refused row."""
    from brokerrouter_hermes.publication import hold_required_recovery
    import gateway.delivery_ledger as _ledger
    if hold_required_recovery(_ledger):
        return []
    now, (pid, started) = now if now is not None else time.time(), _owner_stamp()
''',
    ),
    (
        '''    Rows past the attempts cap or stale cutoff are left for the sweeps to abandon."""
    now, (pid, started) = now if now is not None else time.time(), _owner_stamp()
''',
        '''    Rows past the attempts cap or stale cutoff are left for the sweeps to abandon."""
    from brokerrouter_hermes.publication import hold_required_recovery
    import gateway.delivery_ledger as _ledger
    if hold_required_recovery(_ledger):
        return []
    now, (pid, started) = now if now is not None else time.time(), _owner_stamp()
''',
    ),
    (
        '''            total = conn.execute("SELECT COUNT(*) FROM delivery_obligations").fetchone()[0]
''',
        '''            total = conn.execute("SELECT COUNT(*) FROM delivery_obligations WHERE obligation_id NOT LIKE 'br-final-v1:%'").fetchone()[0]
''',
    ),
    (
        '''                         SELECT obligation_id FROM delivery_obligations
                         ORDER BY CASE state
''',
        '''                         SELECT obligation_id FROM delivery_obligations
                         WHERE obligation_id NOT LIKE 'br-final-v1:%'
                         ORDER BY CASE state
''',
    ),
)

RUN_VOICE_REPLACEMENTS = (
    (
        '''        — UNLESS streaming consumed the response (already_sent): then the runner must do it."""
        if not response or response.startswith("Error:"):
''',
        '''        — UNLESS streaming consumed the response (already_sent): then the runner must do it."""
        from brokerrouter_hermes.native import binary_delivery_disabled
        if binary_delivery_disabled():
            return False
        if not response or response.startswith("Error:"):
''',
    ),
    (
        '''        over a platform limit); legacy single-file results keep working."""
        audio_path, actual_paths = None, []
''',
        '''        over a platform limit); legacy single-file results keep working."""
        from brokerrouter_hermes.native import binary_delivery_disabled
        if binary_delivery_disabled():
            logger.warning("brokerrouter_binary_artifact_release_unsupported")
            return
        audio_path, actual_paths = None, []
''',
    ),
    (
        '''        """Play the files in the connected voice channel, else send them as voice messages."""
        adapter = self._adapter_for_source(event.source)
''',
        '''        """Play the files in the connected voice channel, else send them as voice messages."""
        from brokerrouter_hermes.native import binary_delivery_disabled
        if binary_delivery_disabled():
            logger.warning("brokerrouter_binary_artifact_release_unsupported")
            return
        adapter = self._adapter_for_source(event.source)
''',
    ),
)

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
    return _replace_exact(source, TOOL_EXECUTOR_REPLACEMENTS, "agent/tool_executor.py")


def patched_run_turn_bytes(source: bytes) -> bytes:
    return _replace_exact(source, RUN_TURN_REPLACEMENTS, "gateway/run_turn.py")


def patched_run_turn_runner_bytes(source: bytes) -> bytes:
    return _replace_exact(
        source, RUN_TURN_RUNNER_REPLACEMENTS, "gateway/run_turn_runner.py"
    )


def patched_model_tools_bytes(source: bytes) -> bytes:
    return _replace_exact(source, MODEL_TOOLS_REPLACEMENTS, "model_tools.py")


def patched_registry_bytes(source: bytes) -> bytes:
    return _replace_exact(source, REGISTRY_REPLACEMENTS, "tools/registry.py")


def patched_platform_base_bytes(source: bytes) -> bytes:
    return _replace_exact(source, PLATFORM_BASE_REPLACEMENTS, "gateway/platforms/base.py")


def patched_delivery_ledger_bytes(source: bytes) -> bytes:
    return _replace_exact(source, DELIVERY_LEDGER_REPLACEMENTS, "gateway/delivery_ledger.py")


def patched_run_voice_bytes(source: bytes) -> bytes:
    return _replace_exact(source, RUN_VOICE_REPLACEMENTS, "gateway/run_voice.py")


PATCHES: tuple[tuple[str, str, Callable[[bytes], bytes]], ...] = (
    ("agent/tool_executor.py", TOOL_EXECUTOR_SHA256, patched_bytes),
    ("gateway/run_turn.py", RUN_TURN_SHA256, patched_run_turn_bytes),
    (
        "gateway/run_turn_runner.py",
        RUN_TURN_RUNNER_SHA256,
        patched_run_turn_runner_bytes,
    ),
    ("model_tools.py", MODEL_TOOLS_SHA256, patched_model_tools_bytes),
    ("tools/registry.py", REGISTRY_SHA256, patched_registry_bytes),
    ("gateway/platforms/base.py", PLATFORM_BASE_SHA256, patched_platform_base_bytes),
    ("gateway/run_voice.py", RUN_VOICE_SHA256, patched_run_voice_bytes),
    ("gateway/delivery_ledger.py", DELIVERY_LEDGER_SHA256, patched_delivery_ledger_bytes),
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
