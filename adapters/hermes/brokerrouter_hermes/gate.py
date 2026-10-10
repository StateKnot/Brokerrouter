from __future__ import annotations

from dataclasses import dataclass
from contextlib import nullcontext
from datetime import datetime, timezone
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
from .store import Checkpoint, CheckpointConflict, CheckpointStore, FrozenResult, MAX_RESULT_BYTES


SUPPORTED_HERMES_REVISION = "67764dc0863349a384c16425e73ee8571f3a94b7"
MAX_RESULT_WIRE_BYTES = 512 * 1024


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

    @property
    def model_binding_hash(self) -> str:
        return _sha(_canonical({"model_id": self.model_id}))


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
            else HttpTransport(config.base_url, config.virtual_key, config.request_timeout_seconds,
                               config.gateway_ca_file)
        )
        self._sleep = sleeper
        self._monotonic = monotonic
        self._human_wait = human_wait
        self._native_returns: set[str] = set()
        self._native_lock = threading.Lock()

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
        self, context: ToolContext, response: dict[str, Any], checkpoint: Checkpoint | FrozenResult
    ) -> str | None:
        if checkpoint.state != "waiting_approval":
            return None
        operation_id = (
            checkpoint.output_operation_id if isinstance(checkpoint, FrozenResult)
            else checkpoint.operation_id
        )
        approval = response.get("approval")
        if not isinstance(approval, dict):
            raise GovernanceHalt("gateway_approval_invalid", operation_id)
        approval_id = _uuid(approval.get("id"), "gateway_approval_invalid")
        if (
            _uuid(approval.get("operation_id"), "gateway_approval_invalid")
            != operation_id
            or _uuid(approval.get("turn_id"), "gateway_approval_invalid") != context.turn_id
            or approval.get("state") != "pending"
            or approval.get("scope") != "current_turn_only"
            or approval.get("resume_requires_recheck") is not True
            or approval.get("options") != ["continue_this_turn", "reject", "feedback"]
        ):
            raise GovernanceHalt("gateway_approval_invalid", operation_id)
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

    def _result_metadata(
        self,
        context: ToolContext,
        checkpoint: Checkpoint,
        frozen: FrozenResult,
        response: dict[str, Any],
    ) -> datetime:
        try:
            value = response.get("expires_at")
            if type(value) is not str or len(value) > 64:
                raise ValueError("invalid expiration")
            expires_at = datetime.fromisoformat(value.replace("Z", "+00:00"))
            if expires_at.tzinfo is None or expires_at <= datetime.now(timezone.utc):
                raise ValueError("expired result")
        except (ValueError, TypeError, OverflowError) as exc:
            raise GovernanceHalt("gateway_result_expiry_invalid", checkpoint.operation_id) from exc
        if (
            _uuid(response.get("input_operation_id"), "gateway_result_identity_invalid")
            != checkpoint.operation_id
            or _uuid(response.get("turn_id"), "gateway_result_identity_invalid") != context.turn_id
            or response.get("text_sha256") != frozen.sha256
            or type(response.get("bytes")) is not int
            or response["bytes"] != frozen.byte_length
        ):
            raise GovernanceHalt("gateway_result_identity_invalid", checkpoint.operation_id)
        return expires_at

    def _complete_execution_receipt(self, checkpoint: Checkpoint) -> Checkpoint:
        if checkpoint.state == "effect_recorded" and checkpoint.evidence_ref and self._receipt_best_effort(
            checkpoint, "completed", checkpoint.evidence_ref
        ):
            checkpoint = self.store.transition(checkpoint.operation_key, {"effect_recorded"}, "completed")
        return checkpoint

    def _release_result(
        self,
        context: ToolContext,
        checkpoint: Checkpoint,
        status: Callable[[str], None] | None,
        approval: Callable[[dict[str, str]], None] | None,
    ) -> str:
        frozen = self.store.load_result(context.operation_key)
        if frozen is None or frozen.text is None:
            self._complete_execution_receipt(checkpoint)
            raise GovernanceHalt("result_unavailable_after_restart", checkpoint.operation_id)
        if not checkpoint.operation_id or not checkpoint.claim_token or not checkpoint.evidence_ref:
            raise GovernanceHalt("checkpoint_incomplete", checkpoint.operation_id)
        if (
            frozen.byte_length > MAX_RESULT_WIRE_BYTES
            or len(json.dumps(frozen.text, ensure_ascii=False).encode("utf-8")) > MAX_RESULT_WIRE_BYTES
        ):
            self._complete_execution_receipt(checkpoint)
            raise GovernanceHalt("result_wire_unsupported", checkpoint.operation_id)
        try:
            # This endpoint commits the immutable result and completed execution receipt
            # atomically. Lost acknowledgements are recovered with the same claim and bytes.
            response = self.client.record_tool_result(
                checkpoint.operation_id, checkpoint.claim_token, checkpoint.evidence_ref, frozen.text
            )
        except (GatewayFailure, TransportFailure) as exc:
            raise GovernanceHalt("receipt_not_persisted", checkpoint.operation_id) from exc
        result_id = _uuid(response.get("id"), "gateway_result_identity_invalid")
        expires_at = self._result_metadata(context, checkpoint, frozen, response)
        if response.get("state") != "recorded" or "text" in response:
            raise GovernanceHalt("gateway_result_record_invalid", checkpoint.operation_id)
        frozen = self.store.bind_remote_result(
            context.operation_key, result_id, checkpoint.operation_id,
            response["text_sha256"], response["bytes"],
        )
        checkpoint = self.store.load(context.operation_key)
        if checkpoint is None:
            raise GovernanceHalt("checkpoint_incomplete")
        if checkpoint.state == "effect_recorded":
            checkpoint = self.store.transition(context.operation_key, {"effect_recorded"}, "completed")
        if checkpoint.state != "completed":
            raise GovernanceHalt("execution_fact_invalid", checkpoint.operation_id)

        deadline = self._monotonic() + self.config.approval_wait_seconds
        while self._monotonic() < deadline:
            waiting = False
            try:
                # Even a local ready observation is rechecked against current policy.
                response = self.client.release_tool_result(result_id)
                if (
                    _uuid(response.get("result_id"), "gateway_result_identity_invalid") != result_id
                    or self._result_metadata(context, checkpoint, frozen, response) != expires_at
                ):
                    raise GovernanceHalt("gateway_result_identity_invalid", checkpoint.operation_id)
                output_id = _uuid(response.get("id"), "gateway_output_operation_invalid")
                if _uuid(response.get("output_operation_id"), "gateway_output_operation_invalid") != output_id:
                    raise GovernanceHalt("gateway_output_operation_invalid", output_id)
                payload_hash = _valid_gateway_hash(response.get("payload_hash"))
                if payload_hash is None:
                    raise GovernanceHalt("gateway_payload_hash_invalid", output_id)
                state = response.get("state")
                if state not in {"checking", "waiting_approval", "ready", "rejected"}:
                    raise GovernanceHalt("gateway_state_invalid", output_id)
                if state != "ready" and "text" in response:
                    raise GovernanceHalt("gateway_result_body_before_release", output_id)
                if state == "ready":
                    text = response.get("text")
                    if type(text) is not str or text != frozen.text:
                        raise GovernanceHalt("gateway_result_bytes_invalid", output_id)
                    try:
                        clear = text.encode("utf-8")
                    except UnicodeEncodeError as exc:
                        raise GovernanceHalt("gateway_result_bytes_invalid", output_id) from exc
                    if len(clear) != frozen.byte_length or _sha(clear) != frozen.sha256:
                        raise GovernanceHalt("gateway_result_bytes_invalid", output_id)
                if state == "checking":
                    evaluations = response.get("evaluations")
                    if type(evaluations) is not list or not 1 <= len(evaluations) <= 128:
                        raise GovernanceHalt("gateway_evaluation_invalid", output_id)
                    for evaluation in evaluations:
                        _uuid(evaluation, "gateway_evaluation_invalid")
                frozen = self.store.output_checked(
                    context.operation_key, output_id, state, payload_hash
                )
                self._status(status, f"output_{state}")
                approval_id = self._approval_id(context, response, frozen)
                if approval_id is not None:
                    self._notify_approval(context, approval_id, approval)
                    waiting = True
                if state == "ready":
                    return text
                if state == "rejected":
                    raise GovernanceHalt("action_rejected", output_id)
            except GatewayFailure as exc:
                if exc.code == "action_rejected":
                    if frozen.output_operation_id:
                        self.store.output_checked(
                            context.operation_key, frozen.output_operation_id,
                            "rejected", frozen.output_payload_hash,
                        )
                    else:
                        self.store.mark_result_unavailable(context.operation_key)
                if 400 <= exc.status < 500:
                    raise GovernanceHalt(exc.code, frozen.output_operation_id) from exc
                self._status(status, "gateway_unavailable")
            except TransportFailure:
                self._status(status, "gateway_unavailable")
            remaining = max(0.0, deadline - self._monotonic())
            if waiting:
                with self._human_wait():
                    self._sleep(min(self.config.approval_poll_seconds, remaining))
            else:
                self._sleep(min(self.config.approval_poll_seconds, remaining))
        raise GovernanceHalt("approval_wait_timeout", frozen.output_operation_id)

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

    def native_open(
        self, context: ToolContext, status: Callable[[str], None] | None = None,
        approval: Callable[[dict[str, str]], None] | None = None,
    ) -> str | None:
        """Bind original identity before Relay; recovery never reenters middleware."""
        try:
            context = context.validate()
            origin = self.store.register_native_origin(
                context.operation_key, context.identity_hash, context.tool_name,
                context.model_binding_hash, _sha(_canonical(context.arguments)),
            )
            checkpoint = self.store.load(context.operation_key)
            if checkpoint is None:
                return None
            if checkpoint.state in {"dispatch_authorized", "dispatching"}:
                checkpoint = self.store.transition(
                    context.operation_key, {"dispatch_authorized", "dispatching"}, "execution_unknown",
                    f"hermes://brokerrouter/{context.operation_key}/restart-ambiguity",
                )
                self._receipt_best_effort(checkpoint, "execution_unknown", checkpoint.evidence_ref or "")
            if checkpoint.state in {"effect_recorded", "completed"}:
                if (
                    origin.effect_recorded_at_ns is None
                    or origin.final_payload_hash != checkpoint.local_payload_hash
                    or origin.model_binding_hash != checkpoint.model_binding_hash
                ):
                    raise GovernanceHalt("native_execution_fact_invalid", checkpoint.operation_id)
                return self._release_result(context, checkpoint, status, approval)
            if checkpoint.state in {"claim_unknown", "execution_unknown", "rejected"}:
                raise GovernanceHalt(checkpoint.state, checkpoint.operation_id)
            return None
        except GovernanceHalt:
            raise
        except Exception as exc:
            raise GovernanceHalt("native_origin_conflict") from exc

    def run_execution(
        self, context: ToolContext, dispatch: Callable[[], Any],
        status: Callable[[str], None] | None = None,
        approval: Callable[[dict[str, str]], None] | None = None,
    ) -> Any:
        """Claim final arguments and record the known effect before returning to Relay."""
        try:
            context = context.validate()
            self.store.bind_native_final(
                context.operation_key, context.identity_hash, context.tool_name,
                context.model_binding_hash, _sha(_canonical(context.arguments)),
            )
            return self._run_tool(context, dispatch, status, approval, native_execution=True)
        except GovernanceHalt:
            raise
        except Exception as exc:
            raise GovernanceHalt("native_execution_conflict") from exc

    def finish_native(
        self, context: ToolContext, final_text: str,
        status: Callable[[str], None] | None = None,
        approval: Callable[[dict[str, str]], None] | None = None,
    ) -> str:
        """Freeze once after all host transformations, before any content projection."""
        try:
            context = context.validate()
            self.store.bind_native_final(
                context.operation_key, context.identity_hash, context.tool_name,
                context.model_binding_hash, _sha(_canonical(context.arguments)),
            )
            checkpoint = self.store.stage(
                context.operation_key, context.identity_hash, context.tool_name,
                _sha(_canonical(context.arguments)), model_binding_hash=context.model_binding_hash,
                native=True,
            )
            frozen = self.store.load_result(context.operation_key)
            with self._native_lock:
                returned = context.operation_key in self._native_returns
            if frozen is None and not returned:
                raise GovernanceHalt("native_final_result_unavailable", checkpoint.operation_id)
            if checkpoint.state not in {"effect_recorded", "completed"} or not checkpoint.evidence_ref:
                raise GovernanceHalt("native_execution_fact_invalid", checkpoint.operation_id)
            try:
                self.store.freeze_native_result(context.operation_key, final_text, checkpoint.evidence_ref)
            except Exception as exc:
                frozen = self.store.load_result(context.operation_key)
                if frozen is None or type(final_text) is not str or frozen.text != final_text:
                    raise GovernanceHalt("native_final_result_not_persisted", checkpoint.operation_id) from exc
            with self._native_lock:
                self._native_returns.discard(context.operation_key)
            return self._release_result(context, checkpoint, status, approval)
        except GovernanceHalt:
            raise
        except Exception as exc:
            raise GovernanceHalt("native_final_result_invalid") from exc

    def _run_tool(
        self,
        context: ToolContext,
        dispatch: Callable[[], Any],
        status: Callable[[str], None] | None = None,
        approval: Callable[[dict[str, str]], None] | None = None,
        *, native_execution: bool = False,
    ) -> Any:
        context = context.validate()
        payload_hash = _sha(_canonical(context.arguments))
        checkpoint = self.store.stage(
            context.operation_key, context.identity_hash, context.tool_name, payload_hash,
            model_binding_hash=context.model_binding_hash, native=native_execution,
        )

        if checkpoint.state in {"dispatch_authorized", "dispatching"}:
            checkpoint = self.store.transition(
                context.operation_key,
                {"dispatch_authorized", "dispatching"},
                "execution_unknown",
                f"hermes://brokerrouter/{context.operation_key}/restart-ambiguity",
            )
            self._receipt_best_effort(checkpoint, "execution_unknown", checkpoint.evidence_ref or "")
        if checkpoint.state in {"effect_recorded", "completed"}:
            if native_execution:
                raise GovernanceHalt("native_final_result_unavailable", checkpoint.operation_id)
            return self._release_result(context, checkpoint, status, approval)
        if checkpoint.state in {
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
            unchanged = _sha(_canonical(context.arguments)) == payload_hash
        except (Exception, GovernanceHalt):
            unchanged = False
        if not unchanged:
            # A claim authorizes precisely the checked arguments. Mutable host
            # references and callbacks must not change them during approval/HTTP
            # waits. The claim remains consumed; no tool has run or may be retried.
            evidence = f"hermes://brokerrouter/{context.operation_key}/final-arguments-changed-before-dispatch"
            checkpoint = self.store.transition(
                context.operation_key, {"dispatching"}, "execution_unknown", evidence
            )
            self._receipt_best_effort(checkpoint, "execution_unknown", evidence)
            raise GovernanceHalt("final_arguments_changed", checkpoint.operation_id)
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
        if native_execution:
            try:
                checkpoint = self.store.record_native_effect(context.operation_key, evidence)
            except Exception as exc:
                checkpoint = self.store.load(context.operation_key)
                if checkpoint is None or checkpoint.state not in {"effect_recorded", "completed"}:
                    raise GovernanceHalt("native_effect_not_persisted") from exc
            with self._native_lock:
                self._native_returns.add(context.operation_key)
            self._complete_execution_receipt(checkpoint)
            return result
        unsupported = None
        if type(result) is not str:
            unsupported = "tool_result_not_string"
        elif len(result) > MAX_RESULT_BYTES:
            unsupported = "result_storage_unsupported"
        else:
            try:
                if len(result.encode("utf-8")) > MAX_RESULT_BYTES:
                    unsupported = "result_storage_unsupported"
            except UnicodeEncodeError:
                unsupported = "tool_result_not_utf8"
        if unsupported:
            checkpoint = self.store.transition(
                context.operation_key, {"dispatching"}, "effect_recorded", evidence
            )
            self._complete_execution_receipt(checkpoint)
            raise GovernanceHalt(unsupported, checkpoint.operation_id)
        try:
            self.store.freeze_result(context.operation_key, result, evidence)
        except (Exception, GovernanceHalt) as exc:
            # A freeze can commit before its observation fails. Authentic persisted bytes
            # are recoverable; absent or corrupt bytes never justify another dispatch.
            try:
                frozen = self.store.load_result(context.operation_key)
            except Exception:
                frozen = None
            if frozen is None or frozen.text is None:
                current = self.store.load(context.operation_key)
                if current is not None and current.state == "dispatching":
                    current = self.store.transition(
                        context.operation_key, {"dispatching"}, "execution_unknown", evidence
                    )
                    self._receipt_best_effort(current, "execution_unknown", evidence)
                if isinstance(exc, GovernanceHalt):
                    raise
                raise GovernanceHalt("result_not_persisted", checkpoint.operation_id) from exc
        checkpoint = self.store.load(context.operation_key)
        if checkpoint is None:
            raise GovernanceHalt("checkpoint_incomplete")
        return self._release_result(context, checkpoint, status, approval)

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
