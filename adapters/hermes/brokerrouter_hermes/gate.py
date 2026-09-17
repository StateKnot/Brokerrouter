from __future__ import annotations

from dataclasses import dataclass
from contextlib import nullcontext
import hashlib
import json
import secrets
import threading
import time
from typing import Any, Callable
from uuid import UUID

from .client import GatewayFailure, GovernanceClient, HttpTransport, Transport, TransportFailure
from .config import Config
from .integrity import verify_runtime_installation
from .store import Checkpoint, CheckpointConflict, CheckpointStore


SUPPORTED_HERMES_REVISION = "67764dc0863349a384c16425e73ee8571f3a94b7"


class GovernanceHalt(BaseException):
    """A fail-closed terminal state that ordinary tool exception handlers must not swallow."""

    def __init__(self, code: str, operation_id: str | None = None) -> None:
        super().__init__(code)
        self.code = code
        self.operation_id = operation_id


def _canonical(value: Any) -> bytes:
    try:
        return json.dumps(
            value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False
        ).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise GovernanceHalt("final_arguments_not_json") from exc


def _sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _uuid(value: Any, code: str) -> str:
    try:
        return str(UUID(str(value)))
    except (ValueError, TypeError, AttributeError) as exc:
        raise GovernanceHalt(code) from exc


def _valid_gateway_hash(value: Any) -> str | None:
    if isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value):
        return value
    return None


@dataclass(frozen=True, repr=False)
class ToolContext:
    turn_id: str
    session_id: str
    task_id: str
    call_id: str
    model_id: str | None
    tool_name: str
    arguments: dict[str, Any]

    def validate(self) -> ToolContext:
        _uuid(self.turn_id, "governance_turn_missing")
        for value, code, limit in (
            (self.session_id, "session_identity_missing", 500),
            (self.call_id, "tool_call_identity_missing", 500),
            (self.tool_name, "tool_name_invalid", 200),
        ):
            if not isinstance(value, str) or not value.strip() or len(value.encode("utf-8")) > limit:
                raise GovernanceHalt(code)
        if self.model_id is not None and (
            not isinstance(self.model_id, str) or not self.model_id or len(self.model_id.encode("utf-8")) > 200
        ):
            raise GovernanceHalt("model_identity_invalid")
        if not isinstance(self.arguments, dict):
            raise GovernanceHalt("final_arguments_not_object")
        return self

    @property
    def identity_hash(self) -> str:
        return _sha(
            "\0".join(
                [SUPPORTED_HERMES_REVISION, self.turn_id, self.session_id, self.task_id, self.call_id]
            ).encode("utf-8")
        )

    @property
    def operation_key(self) -> str:
        return f"hermes-v1-{self.identity_hash}"


@dataclass(frozen=True, repr=False)
class GatewayTurnBinding:
    turn_id: str
    session_id: str
    message_id: str
    run_id: str
    generation: int

    def validate(self) -> GatewayTurnBinding:
        _uuid(self.turn_id, "governance_turn_missing")
        for value, code in (
            (self.session_id, "session_identity_missing"),
            (self.message_id, "message_identity_missing"),
            (self.run_id, "run_identity_missing"),
        ):
            if not isinstance(value, str) or not value.strip() or len(value.encode("utf-8")) > 200:
                raise GovernanceHalt(code)
        if isinstance(self.generation, bool) or not isinstance(self.generation, int) or self.generation < 0:
            raise GovernanceHalt("generation_identity_invalid")
        return self


class GovernanceGate:
    def __init__(
        self,
        config: Config,
        *,
        transport: Transport | None = None,
        sleeper: Callable[[float], None] = time.sleep,
        monotonic: Callable[[], float] = time.monotonic,
        human_wait: Callable[[], Any] = nullcontext,
    ) -> None:
        self.config = config
        self.store = CheckpointStore(config.state_db, config.state_key)
        self.client = GovernanceClient(
            transport
            if transport is not None
            else HttpTransport(config.base_url, config.virtual_key, config.request_timeout_seconds)
        )
        self._sleep = sleeper
        self._monotonic = monotonic
        self._human_wait = human_wait

    def _status(self, callback: Callable[[str], None] | None, state: str) -> None:
        if callback is not None:
            try:
                callback(f"Brokerrouter governance: {state}")
            except Exception:
                pass

    def _check_body(self, context: ToolContext) -> dict[str, Any]:
        return {
            "turn_id": context.turn_id,
            "operation_key": context.operation_key,
            "stage": "tool_input",
            "model_id": context.model_id,
            "tool_name": context.tool_name,
            "payload": context.arguments,
        }

    def _record_check(self, context: ToolContext, response: dict[str, Any]) -> Checkpoint:
        operation_id = _uuid(response.get("id"), "gateway_operation_invalid")
        state = response.get("state")
        if state not in {"checking", "waiting_approval", "ready"}:
            raise GovernanceHalt("gateway_state_invalid", operation_id)
        gateway_hash = _valid_gateway_hash(response.get("payload_hash")) if state == "ready" else None
        if state == "ready" and gateway_hash is None:
            raise GovernanceHalt("gateway_payload_hash_invalid", operation_id)
        return self.store.checked(context.operation_key, operation_id, state, gateway_hash)

    def _approval_id(
        self, context: ToolContext, response: dict[str, Any], checkpoint: Checkpoint
    ) -> str | None:
        if checkpoint.state != "waiting_approval":
            return None
        approval = response.get("approval")
        if not isinstance(approval, dict):
            raise GovernanceHalt("gateway_approval_invalid", checkpoint.operation_id)
        approval_id = _uuid(approval.get("id"), "gateway_approval_invalid")
        if (
            _uuid(approval.get("operation_id"), "gateway_approval_invalid")
            != checkpoint.operation_id
            or _uuid(approval.get("turn_id"), "gateway_approval_invalid") != context.turn_id
            or approval.get("state") != "pending"
            or approval.get("scope") != "current_turn_only"
            or approval.get("resume_requires_recheck") is not True
            or approval.get("options") != ["continue_this_turn", "reject", "feedback"]
        ):
            raise GovernanceHalt("gateway_approval_invalid", checkpoint.operation_id)
        return approval_id

    def _notify_approval(
        self,
        context: ToolContext,
        approval_id: str,
        callback: Callable[[dict[str, str]], None] | None,
    ) -> None:
        if callback is None or not self.store.reserve_approval_notice(
            context.operation_key, approval_id
        ):
            return
        try:
            callback(
                {
                    "approval_id": approval_id,
                    "turn_id": context.turn_id,
                    "review_url": f"{self.config.approvals_url}/#approvals",
                }
            )
            self.store.complete_approval_notice(context.operation_key, approval_id)
        except BaseException:
            self.store.release_approval_notice(context.operation_key, approval_id)
            raise

    def _wait_until_ready(
        self,
        context: ToolContext,
        checkpoint: Checkpoint,
        status: Callable[[str], None] | None,
        approval: Callable[[dict[str, str]], None] | None,
    ) -> Checkpoint:
        deadline = self._monotonic() + self.config.approval_wait_seconds
        body = self._check_body(context)
        while checkpoint.state in {"staged", "checking", "waiting_approval", "ready"}:
            if checkpoint.state == "ready":
                return checkpoint
            if self._monotonic() >= deadline:
                raise GovernanceHalt("approval_wait_timeout", checkpoint.operation_id)
            try:
                response = self.client.check(body)
                checkpoint = self._record_check(context, response)
                self._status(status, checkpoint.state)
                approval_id = self._approval_id(context, response, checkpoint)
                if approval_id is not None:
                    self._notify_approval(context, approval_id, approval)
            except GatewayFailure as exc:
                if exc.code == "action_rejected":
                    if checkpoint.operation_id:
                        checkpoint = self.store.transition(
                            context.operation_key,
                            {"staged", "checking", "waiting_approval", "ready"},
                            "rejected",
                        )
                    raise GovernanceHalt("action_rejected", checkpoint.operation_id) from exc
                if exc.code in {"turn_closed", "invalid_virtual_key", "not_found", "policy_changed"}:
                    raise GovernanceHalt(exc.code, checkpoint.operation_id) from exc
                if 400 <= exc.status < 500:
                    raise GovernanceHalt(exc.code, checkpoint.operation_id) from exc
                self._status(status, "gateway_unavailable")
            except TransportFailure:
                self._status(status, "gateway_unavailable")
            if checkpoint.state != "ready":
                remaining = max(0.0, deadline - self._monotonic())
                if checkpoint.state == "waiting_approval":
                    with self._human_wait():
                        self._sleep(min(self.config.approval_poll_seconds, remaining))
                else:
                    self._sleep(min(self.config.approval_poll_seconds, remaining))
        raise GovernanceHalt(checkpoint.state, checkpoint.operation_id)

    def _receipt_best_effort(self, checkpoint: Checkpoint, outcome: str, evidence_ref: str) -> bool:
        if not checkpoint.operation_id or not checkpoint.claim_token:
            return False
        try:
            response = self.client.receipt(
                checkpoint.operation_id, checkpoint.claim_token, outcome, evidence_ref
            )
            return response.get("outcome") == outcome and response.get("evidence_ref") == evidence_ref
        except (GatewayFailure, TransportFailure):
            return False

    def run_tool(
        self,
        context: ToolContext,
        dispatch: Callable[[], Any],
        status: Callable[[str], None] | None = None,
        approval: Callable[[dict[str, str]], None] | None = None,
    ) -> Any:
        try:
            return self._run_tool(context, dispatch, status, approval)
        except GovernanceHalt:
            raise
        except Exception as exc:
            raise GovernanceHalt("adapter_internal_failure") from exc

    def _run_tool(
        self,
        context: ToolContext,
        dispatch: Callable[[], Any],
        status: Callable[[str], None] | None = None,
        approval: Callable[[dict[str, str]], None] | None = None,
    ) -> Any:
        context = context.validate()
        payload_hash = _sha(_canonical(context.arguments))
        checkpoint = self.store.stage(
            context.operation_key, context.identity_hash, context.tool_name, payload_hash
        )

        if checkpoint.state in {"dispatch_authorized", "dispatching"}:
            checkpoint = self.store.transition(
                context.operation_key,
                {"dispatch_authorized", "dispatching"},
                "execution_unknown",
                f"hermes://brokerrouter/{context.operation_key}/restart-ambiguity",
            )
            self._receipt_best_effort(checkpoint, "execution_unknown", checkpoint.evidence_ref or "")
        if checkpoint.state == "effect_recorded":
            if checkpoint.evidence_ref and self._receipt_best_effort(
                checkpoint, "completed", checkpoint.evidence_ref
            ):
                checkpoint = self.store.transition(
                    context.operation_key, {"effect_recorded"}, "completed"
                )
            raise GovernanceHalt("result_unavailable_after_restart", checkpoint.operation_id)
        if checkpoint.state in {
            "completed",
            "claim_unknown",
            "execution_unknown",
            "rejected",
        }:
            raise GovernanceHalt(checkpoint.state, checkpoint.operation_id)

        if checkpoint.state != "claim_prepared":
            checkpoint = self._wait_until_ready(context, checkpoint, status, approval)
            checkpoint = self.store.prepare_claim(context.operation_key, secrets.token_hex(32))

        if not checkpoint.operation_id or not checkpoint.gateway_payload_hash or not checkpoint.claim_token:
            raise GovernanceHalt("checkpoint_incomplete", checkpoint.operation_id)
        try:
            claim = self.client.claim(
                checkpoint.operation_id, checkpoint.claim_token, checkpoint.gateway_payload_hash
            )
        except GatewayFailure as exc:
            if exc.code in {
                "approval_required",
                "operation_in_progress",
                "policy_changed",
                "action_rejected",
                "turn_closed",
                "invalid_virtual_key",
                "not_found",
            }:
                raise GovernanceHalt(exc.code, checkpoint.operation_id) from exc
            checkpoint = self.store.transition(
                context.operation_key, {"claim_prepared"}, "claim_unknown"
            )
            raise GovernanceHalt("claim_response_unknown", checkpoint.operation_id) from exc
        except TransportFailure as exc:
            checkpoint = self.store.transition(
                context.operation_key, {"claim_prepared"}, "claim_unknown"
            )
            raise GovernanceHalt("claim_response_unknown", checkpoint.operation_id) from exc
        try:
            claim_id = _uuid(claim.get("id"), "claim_response_invalid")
        except GovernanceHalt as exc:
            checkpoint = self.store.transition(
                context.operation_key, {"claim_prepared"}, "claim_unknown"
            )
            raise GovernanceHalt("claim_response_unknown", checkpoint.operation_id) from exc
        valid_claim = (
            claim.get("dispatch_authorized") is True
            and claim.get("state") == "executing"
            and claim_id == checkpoint.operation_id
            and claim.get("payload_hash") == checkpoint.gateway_payload_hash
        )
        if not valid_claim:
            checkpoint = self.store.transition(
                context.operation_key, {"claim_prepared"}, "claim_unknown"
            )
            raise GovernanceHalt("claim_not_authorized", checkpoint.operation_id)

        checkpoint = self.store.transition(
            context.operation_key, {"claim_prepared"}, "dispatch_authorized"
        )
        checkpoint = self.store.transition(
            context.operation_key, {"dispatch_authorized"}, "dispatching"
        )
        self._status(status, "dispatch_authorized")
        try:
            result = dispatch()
        except BaseException as exc:
            evidence = f"hermes://brokerrouter/{context.operation_key}/execution-unknown/{type(exc).__name__}"
            checkpoint = self.store.transition(
                context.operation_key, {"dispatching"}, "execution_unknown", evidence
            )
            self._receipt_best_effort(checkpoint, "execution_unknown", evidence)
            raise GovernanceHalt("execution_unknown", checkpoint.operation_id) from exc

        evidence = f"hermes://brokerrouter/{context.operation_key}/tool-returned"
        checkpoint = self.store.transition(
            context.operation_key, {"dispatching"}, "effect_recorded", evidence
        )
        if not self._receipt_best_effort(checkpoint, "completed", evidence):
            raise GovernanceHalt("receipt_not_persisted", checkpoint.operation_id)
        self.store.transition(context.operation_key, {"effect_recorded"}, "completed")
        self._status(status, "completed")
        return result

    def bind_turn(self, binding: GatewayTurnBinding) -> str:
        binding = binding.validate()
        body = {
            "session_id": binding.session_id,
            "message_id": binding.message_id,
            "run_id": binding.run_id,
            "generation": binding.generation,
        }
        try:
            response = self.client.bind_turn(binding.turn_id, body)
        except GatewayFailure as exc:
            raise GovernanceHalt(exc.code, binding.turn_id) from exc
        except TransportFailure as exc:
            raise GovernanceHalt("turn_binding_unavailable", binding.turn_id) from exc
        response_id = _uuid(response.get("id"), "turn_binding_invalid")
        if (
            response_id != binding.turn_id
            or response.get("state") != "active"
            or response.get("session_id") != binding.session_id
            or response.get("message_id") != binding.message_id
            or response.get("run_id") != binding.run_id
            or response.get("generation") != binding.generation
        ):
            raise GovernanceHalt("turn_binding_invalid", binding.turn_id)
        return response_id


_gate_lock = threading.Lock()


def gate_for_agent(agent: Any) -> GovernanceGate | None:
    existing = getattr(agent, "_brokerrouter_governance_gate", None)
    if existing is not None:
        return existing
    try:
        config = Config.from_env()
    except Exception as exc:
        raise GovernanceHalt("adapter_configuration_invalid") from exc
    if config is None:
        return None
    try:
        verify_runtime_installation()
    except Exception as exc:
        raise GovernanceHalt("adapter_integrity_invalid") from exc
    with _gate_lock:
        existing = getattr(agent, "_brokerrouter_governance_gate", None)
        if existing is None:
            try:
                from tools.approval_context import _get_approval_timeout
                from tools.approval_human_wait import human_wait_window

                if config.approval_wait_seconds > float(_get_approval_timeout()):
                    raise ValueError(
                        "Brokerrouter approval wait exceeds Hermes approvals.timeout"
                    )
                existing = GovernanceGate(config, human_wait=human_wait_window)
            except Exception as exc:
                raise GovernanceHalt("adapter_configuration_invalid") from exc
            setattr(agent, "_brokerrouter_governance_gate", existing)
        return existing


def context_for_agent(agent: Any, ref: Any) -> ToolContext:
    turn_id = getattr(agent, "_brokerrouter_turn_id", None)
    return ToolContext(
        turn_id=str(turn_id or ""),
        session_id=str(getattr(agent, "session_id", "") or ""),
        task_id=str(getattr(ref, "task_id", "") or ""),
        call_id=str(getattr(ref, "call_id", "") or ""),
        model_id=str(getattr(agent, "model", "") or "") or None,
        tool_name=str(getattr(ref, "name", "") or ""),
        arguments=getattr(ref, "args", None),
    )


def reject_governed_proxy() -> None:
    try:
        config = Config.from_env()
    except Exception as exc:
        raise GovernanceHalt("adapter_configuration_invalid") from exc
    if config is not None:
        raise GovernanceHalt("governed_proxy_not_supported")


def bind_gateway_agent(agent: Any, context: Any, runner: Any) -> None:
    setattr(agent, "_brokerrouter_turn_id", None)
    setattr(agent, "_brokerrouter_approval_callback", None)
    gate = gate_for_agent(agent)
    if gate is None:
        return
    raw = getattr(context, "_brokerrouter_turn_binding", None)
    if not isinstance(raw, dict) or set(raw) != {"turn_id", "run_id"}:
        raise GovernanceHalt("governance_turn_binding_missing")
    binding = GatewayTurnBinding(
        turn_id=str(raw.get("turn_id") or ""),
        session_id=str(getattr(context, "session_id", "") or ""),
        message_id=str(getattr(context, "inbound_message_id", "") or ""),
        run_id=str(raw.get("run_id") or ""),
        generation=getattr(context, "run_generation", None),
    )
    turn_id = gate.bind_turn(binding)
    adapter = getattr(context, "_status_adapter", None)
    loop = getattr(context, "_voice_ack_loop", None)
    if adapter is None or loop is None:
        raise GovernanceHalt("approval_channel_unavailable", turn_id)

    def notify(details: dict[str, str]) -> None:
        import asyncio
        import concurrent.futures

        runner._close_native_stream_boundary("Brokerrouter approval")
        metadata = dict(getattr(context, "_status_thread_metadata", None) or {})
        metadata.update(
            {
                "is_approval_prompt": True,
                "brokerrouter_approval_id": details["approval_id"],
            }
        )
        message = (
            "Brokerrouter 需要本人审批才能继续当前工具。\n\n"
            f"审批 ID：`{details['approval_id']}`\n"
            f"请打开安全审阅页：{details['review_url']}\n\n"
            "审批仅对当前 turn 的指定护栏有效；在聊天中回复“同意”不会授权。"
        )
        future = asyncio.run_coroutine_threadsafe(
            adapter.send(getattr(context, "_status_chat_id", None), message, metadata=metadata),
            loop,
        )
        try:
            result = future.result(timeout=15)
        except concurrent.futures.TimeoutError:
            return
        if result is not None and getattr(result, "success", True) is not True:
            raise RuntimeError("Brokerrouter approval notice was refused")

    setattr(agent, "_brokerrouter_turn_id", turn_id)
    setattr(agent, "_brokerrouter_approval_callback", notify)
