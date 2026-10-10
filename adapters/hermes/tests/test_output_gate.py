from __future__ import annotations

from contextlib import contextmanager
from dataclasses import replace
from datetime import datetime, timedelta, timezone
import hashlib
import os
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch
from uuid import uuid4

from brokerrouter_hermes.client import GatewayFailure, TransportFailure
from brokerrouter_hermes.config import Config
from brokerrouter_hermes.gate import GovernanceGate, GovernanceHalt, MAX_RESULT_WIRE_BYTES, ToolContext
from brokerrouter_hermes.store import CheckpointConflict


class Clock:
    def __init__(self):
        self.now = 0.0

    def sleep(self, seconds):
        self.now += seconds


class ResultServer:
    """Atomic record/receipt fixture; independently control the current release policy."""

    def __init__(self, turn_id):
        self.turn_id = turn_id
        self.input_id = str(uuid4())
        self.result_id = str(uuid4())
        self.output_id = str(uuid4())
        self.approval_id = str(uuid4())
        self.expires_at = (datetime.now(timezone.utc) + timedelta(minutes=30)).isoformat()
        self.requests = []
        self.states = ["ready"]
        self.text = None
        self.record_losses = 0
        self.receipt_losses = 0
        self.record_mutator = None
        self.release_mutator = None
        self.release_error = None
        self.before_release = None

    def metadata(self):
        clear = self.text.encode("utf-8")
        return {
            "input_operation_id": self.input_id,
            "turn_id": self.turn_id,
            "text_sha256": hashlib.sha256(clear).hexdigest(),
            "bytes": len(clear),
            "expires_at": self.expires_at,
        }

    def request(self, method, path, body=None):
        self.requests.append((method, path, body))
        if path == "/v1/governance/checks":
            assert body["turn_id"] == self.turn_id
            return {"id": self.input_id, "state": "ready", "payload_hash": "a" * 64}
        if path.endswith("/claim"):
            return {
                "id": self.input_id, "state": "executing", "dispatch_authorized": True,
                "payload_hash": body["payload_hash"],
            }
        if path.endswith("/receipt"):
            if self.receipt_losses:
                self.receipt_losses -= 1
                raise TransportFailure("execution receipt committed before acknowledgement loss")
            return {"outcome": body["outcome"], "evidence_ref": body["evidence_ref"]}
        if path == "/v1/governance/tool-results":
            assert body["input_operation_id"] == self.input_id
            assert set(body) == {"input_operation_id", "claim_token", "evidence_ref", "text"}
            if self.text is None:
                self.text = body["text"]
            assert self.text == body["text"]
            if self.record_losses:
                self.record_losses -= 1
                raise TransportFailure("record committed before acknowledgement loss")
            response = {"id": self.result_id, "state": "recorded", **self.metadata()}
            if self.record_mutator:
                self.record_mutator(response)
            return response
        if path.endswith("/release"):
            assert path == f"/v1/governance/tool-results/{self.result_id}/release"
            if self.before_release:
                self.before_release()
            if self.release_error:
                raise self.release_error
            state = self.states.pop(0) if len(self.states) > 1 else self.states[0]
            response = {
                "id": self.output_id, "output_operation_id": self.output_id,
                "result_id": self.result_id, "state": state,
                "payload_hash": "b" * 64, **self.metadata(),
            }
            if state == "ready":
                response["text"] = self.text
            elif state == "checking":
                response["evaluations"] = [str(uuid4())]
            elif state == "waiting_approval":
                response["approval"] = {
                    "id": self.approval_id, "operation_id": self.output_id,
                    "turn_id": self.turn_id, "state": "pending", "scope": "current_turn_only",
                    "resume_requires_recheck": True,
                    "options": ["continue_this_turn", "reject", "feedback"],
                }
            if self.release_mutator:
                self.release_mutator(response)
            return response
        raise AssertionError(path)


class OutputGateTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        os.chmod(self.temp.name, 0o700)
        self.config = Config(
            base_url="https://gateway.invalid", approvals_url="https://console.invalid",
            virtual_key="sk_br_" + "1" * 64, state_db=Path(self.temp.name) / "state.db",
            state_key=b"\x17" * 32, request_timeout_seconds=1,
            approval_poll_seconds=0.25, approval_wait_seconds=1,
        )
        self.context = ToolContext(
            turn_id=str(uuid4()), session_id="session", task_id="task", call_id="call",
            model_id="logical-model", tool_name="fixture", arguments={"private": "input-secret"},
        )
        self.server = ResultServer(self.context.turn_id)
        self.calls = []

    def tearDown(self):
        self.temp.cleanup()

    def gate(self, *, server=None, human_wait=None):
        clock = Clock()
        options = {"human_wait": human_wait} if human_wait else {}
        return GovernanceGate(
            self.config, transport=server or self.server, sleeper=clock.sleep,
            monotonic=lambda: clock.now, **options,
        )

    def dispatch(self, text="private-result"):
        self.calls.append(True)
        return text

    def test_freezes_and_completes_execution_before_first_release_of_exact_bytes(self):
        gate = self.gate()
        value = "原始\x00输出 e\u0301 \U0001f680\n"
        observed = []

        def before_release():
            checkpoint = gate.store.load(self.context.operation_key)
            frozen = gate.store.load_result(self.context.operation_key)
            observed.append((checkpoint.state, frozen.state, frozen.text, frozen.server_result_id))

        self.server.before_release = before_release
        self.assertEqual(gate.run_tool(self.context, lambda: self.dispatch(value)), value)
        self.assertEqual(observed, [("completed", "quarantined", value, self.server.result_id)])
        self.assertEqual(self.calls, [True])
        self.assertEqual([request[1].split("/")[-1] for request in self.server.requests],
                         ["checks", "claim", "tool-results", "release"])
        self.assertNotIn(value.encode(), self.config.state_db.read_bytes())
        self.assertNotIn(value, repr(gate.store.load_result(self.context.operation_key)))

    def test_semantic_then_human_gate_holds_body_and_notifies_once_without_secret(self):
        self.server.states = ["checking", "waiting_approval", "waiting_approval", "ready"]
        notices, windows, statuses = [], [], []

        @contextmanager
        def human_wait():
            windows.append("open")
            try:
                yield
            finally:
                windows.append("close")

        gate = self.gate(human_wait=human_wait)
        result = gate.run_tool(self.context, self.dispatch, approval=notices.append, status=statuses.append)
        self.assertEqual(result, "private-result")
        self.assertEqual(len(notices), 1)
        self.assertEqual(set(notices[0]), {"approval_id", "turn_id", "review_url"})
        self.assertEqual(notices[0]["approval_id"], self.server.approval_id)
        self.assertNotIn("private-result", repr(notices) + repr(statuses))
        self.assertEqual(windows, ["open", "close", "open", "close"])
        self.assertEqual(self.calls, [True])

    def test_completed_replay_checks_new_policy_and_recovers_after_approval(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.context, self.dispatch), "private-result")
        self.server.states = ["waiting_approval"]
        notices = []
        with self.assertRaises(GovernanceHalt) as held:
            self.gate().run_tool(self.context, self.dispatch, approval=notices.append)
        self.assertEqual(held.exception.code, "approval_wait_timeout")
        self.assertEqual(gate.store.load(self.context.operation_key).state, "completed")
        self.assertEqual(gate.store.load_result(self.context.operation_key).state, "waiting_approval")
        self.assertEqual(len(notices), 1)
        self.server.states = ["ready"]
        self.assertEqual(self.gate().run_tool(self.context, self.dispatch), "private-result")
        self.assertEqual(self.calls, [True])
        self.assertEqual(sum(path.endswith("/claim") for _, path, _ in self.server.requests), 1)

    def test_record_acknowledgement_loss_recovers_same_result_without_dispatch(self):
        self.server.record_losses = 1
        gate = self.gate()
        with self.assertRaises(GovernanceHalt) as lost:
            gate.run_tool(self.context, self.dispatch)
        self.assertEqual(lost.exception.code, "receipt_not_persisted")
        self.assertEqual(gate.store.load(self.context.operation_key).state, "effect_recorded")
        self.assertEqual(gate.store.load_result(self.context.operation_key).text, "private-result")
        self.assertEqual(self.gate().run_tool(self.context, self.dispatch), "private-result")
        self.assertEqual(self.calls, [True])
        records = [body for _, path, body in self.server.requests if path.endswith("/tool-results")]
        self.assertEqual(records[0], records[1])

    def test_release_transport_loss_retains_result_and_restart_releases_without_dispatch(self):
        self.server.release_error = TransportFailure("release acknowledgement lost")
        gate = self.gate()
        with self.assertRaises(GovernanceHalt) as lost:
            gate.run_tool(self.context, self.dispatch)
        self.assertEqual(lost.exception.code, "approval_wait_timeout")
        self.assertEqual(gate.store.load(self.context.operation_key).state, "completed")
        self.assertEqual(gate.store.load_result(self.context.operation_key).text, "private-result")
        self.server.release_error = None
        self.assertEqual(self.gate().run_tool(self.context, self.dispatch), "private-result")
        self.assertEqual(self.calls, [True])

    def test_record_identity_contract_rejects_each_drift_without_body_release(self):
        mutations = {
            "id": "not-a-uuid", "input_operation_id": str(uuid4()), "turn_id": str(uuid4()),
            "text_sha256": "0" * 64, "bytes": True, "state": "ready", "text": "premature",
            "expires_at": "2000-01-01T00:00:00Z",
        }
        for index, (name, value) in enumerate(mutations.items()):
            with self.subTest(name=name):
                context = replace(self.context, call_id=f"record-{index}")
                server = ResultServer(context.turn_id)
                server.record_mutator = lambda response, key=name, changed=value: response.update({key: changed})
                gate = self.gate(server=server)
                with self.assertRaises(GovernanceHalt):
                    gate.run_tool(context, self.dispatch)
                self.assertFalse(any(path.endswith("/release") for _, path, _ in server.requests))
                self.assertEqual(gate.store.load(context.operation_key).state, "effect_recorded")

    def test_release_identity_and_exact_body_contract_rejects_each_drift(self):
        mutations = {
            "result_id": str(uuid4()), "id": str(uuid4()), "output_operation_id": str(uuid4()),
            "input_operation_id": str(uuid4()), "turn_id": str(uuid4()),
            "text_sha256": "0" * 64, "bytes": True, "payload_hash": "invalid",
            "text": "changed-result", "state": "completed", "expires_at": "2030-01-01T00:00:00Z",
        }
        for index, (name, value) in enumerate(mutations.items()):
            with self.subTest(name=name):
                context = replace(self.context, call_id=f"release-{index}")
                server = ResultServer(context.turn_id)
                server.release_mutator = lambda response, key=name, changed=value: response.update({key: changed})
                gate = self.gate(server=server)
                with self.assertRaises(GovernanceHalt):
                    gate.run_tool(context, self.dispatch)
                self.assertEqual(gate.store.load(context.operation_key).state, "completed")
                self.assertNotEqual(gate.store.load_result(context.operation_key).state, "ready")

    def test_pending_response_with_body_is_never_accepted_or_notified(self):
        self.server.states = ["waiting_approval"]
        self.server.release_mutator = lambda response: response.update(text="private-result")
        notices = []
        with self.assertRaises(GovernanceHalt) as halted:
            self.gate().run_tool(self.context, self.dispatch, approval=notices.append)
        self.assertEqual(halted.exception.code, "gateway_result_body_before_release")
        self.assertEqual(notices, [])

    def test_semantic_response_requires_the_existing_evaluations_uuid_contract(self):
        for index, value in enumerate((None, [], ["not-a-uuid"], "wrong-type")):
            with self.subTest(index=index):
                context = replace(self.context, call_id=f"evaluation-{index}")
                server = ResultServer(context.turn_id)
                server.states = ["checking"]
                server.release_mutator = lambda response, changed=value: response.update(evaluations=changed)
                with self.assertRaises(GovernanceHalt) as halted:
                    self.gate(server=server).run_tool(context, self.dispatch)
                self.assertEqual(halted.exception.code, "gateway_evaluation_invalid")

    def test_output_approval_is_bound_to_output_operation_current_turn_and_scope(self):
        mutations = {
            "operation_id": self.server.input_id, "turn_id": str(uuid4()),
            "scope": "all_turns", "resume_requires_recheck": False, "state": "approved",
            "options": ["continue_this_turn"],
        }
        for index, (name, value) in enumerate(mutations.items()):
            with self.subTest(name=name):
                context = replace(self.context, call_id=f"approval-{index}")
                server = ResultServer(context.turn_id)
                server.states = ["waiting_approval"]
                server.release_mutator = lambda response, key=name, changed=value: response["approval"].update({key: changed})
                notices = []
                with self.assertRaises(GovernanceHalt) as halted:
                    self.gate(server=server).run_tool(context, self.dispatch, approval=notices.append)
                self.assertEqual(halted.exception.code, "gateway_approval_invalid")
                self.assertEqual(notices, [])

    def test_failed_output_notice_is_released_for_restart_and_does_not_redispatch(self):
        self.server.states = ["waiting_approval"]
        attempts = []

        def failed(details):
            attempts.append(details)
            raise RuntimeError("synthetic connector refusal")

        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch, approval=failed)
        notices = []
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch, approval=notices.append)
        self.assertEqual(attempts, notices)
        self.assertEqual(self.calls, [True])

    def test_record_identity_cannot_drift_after_completed_restart(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.context, self.dispatch), "private-result")
        self.server.result_id = str(uuid4())
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(self.calls, [True])
        self.assertEqual(gate.store.load_result(self.context.operation_key).state, "ready")

    def test_output_operation_and_payload_cannot_drift_after_restart(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.context, self.dispatch), "private-result")
        original_output_id = self.server.output_id
        self.server.output_id = str(uuid4())
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch)
        self.server.output_id = original_output_id
        self.server.release_mutator = lambda response: response.update(payload_hash="c" * 64)
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(self.calls, [True])

    def test_local_ready_committed_before_observation_loss_never_bypasses_new_policy(self):
        gate = self.gate()
        record = gate.store.output_checked

        def committed_then_lost(*args, **kwargs):
            record(*args, **kwargs)
            raise RuntimeError("ready observation lost after local commit")

        with patch.object(gate.store, "output_checked", side_effect=committed_then_lost):
            with self.assertRaises(GovernanceHalt):
                gate.run_tool(self.context, self.dispatch)
        self.assertEqual(gate.store.load_result(self.context.operation_key).state, "ready")
        self.server.states = ["waiting_approval"]
        with self.assertRaises(GovernanceHalt) as held:
            self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(held.exception.code, "approval_wait_timeout")
        self.assertEqual(self.calls, [True])

    def test_expired_frozen_body_is_unavailable_and_never_sent_or_redispatched(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.context, self.dispatch), "private-result")
        frozen = gate.store.load_result(self.context.operation_key)
        before = len(self.server.requests)
        with patch("brokerrouter_hermes.store.time.time_ns", return_value=frozen.expires_at_ns):
            with self.assertRaises(GovernanceHalt) as halted:
                self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(halted.exception.code, "result_unavailable_after_restart")
        self.assertEqual(gate.store.load_result(self.context.operation_key).state, "unavailable")
        self.assertEqual(gate.store.load(self.context.operation_key).state, "completed")
        self.assertEqual(len(self.server.requests), before)
        self.assertEqual(self.calls, [True])

    def test_rejection_destroys_frozen_body_and_replay_cannot_reopen(self):
        self.server.states = ["rejected"]
        gate = self.gate()
        with self.assertRaises(GovernanceHalt) as rejected:
            gate.run_tool(self.context, self.dispatch)
        self.assertEqual(rejected.exception.code, "action_rejected")
        self.assertEqual(gate.store.load_result(self.context.operation_key).state, "rejected")
        self.assertIsNone(gate.store.load_result(self.context.operation_key).text)
        self.server.states = ["ready"]
        before = len(self.server.requests)
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(len(self.server.requests), before)
        self.assertEqual(self.calls, [True])

    def test_gateway_rejection_error_and_turn_closed_fail_closed(self):
        for index, code in enumerate(("action_rejected", "turn_closed")):
            with self.subTest(code=code):
                context = replace(self.context, call_id=f"gateway-error-{index}")
                server = ResultServer(context.turn_id)
                server.release_error = GatewayFailure(409, code)
                gate = self.gate(server=server)
                with self.assertRaises(GovernanceHalt) as halted:
                    gate.run_tool(context, self.dispatch)
                self.assertEqual(halted.exception.code, code)
                self.assertEqual(gate.store.load(context.operation_key).state, "completed")

    def test_sealing_failure_never_records_or_repeats_the_tool(self):
        gate = self.gate()
        with patch.object(gate.store, "freeze_result", side_effect=CheckpointConflict("sealing failed")):
            with self.assertRaises(GovernanceHalt) as halted:
                gate.run_tool(self.context, self.dispatch)
        self.assertEqual(halted.exception.code, "result_not_persisted")
        self.assertEqual(gate.store.load(self.context.operation_key).state, "execution_unknown")
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(self.calls, [True])
        self.assertFalse(any(path.endswith("/tool-results") for _, path, _ in self.server.requests))

    def test_freeze_commit_observation_loss_uses_durable_bytes(self):
        gate = self.gate()
        freeze = gate.store.freeze_result

        def committed_then_lost(*args, **kwargs):
            freeze(*args, **kwargs)
            raise RuntimeError("observation lost after durable commit")

        with patch.object(gate.store, "freeze_result", side_effect=committed_then_lost):
            self.assertEqual(gate.run_tool(self.context, self.dispatch), "private-result")
        self.assertEqual(self.calls, [True])

    def test_wrong_key_and_corrupted_ciphertext_never_reexecute_or_release(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.context, self.dispatch), "private-result")
        wrong = replace(self.config, state_key=b"\x28" * 32)
        with self.assertRaises(GovernanceHalt):
            GovernanceGate(wrong, transport=self.server).run_tool(self.context, self.dispatch)
        with sqlite3.connect(self.config.state_db) as connection:
            connection.execute("DROP TRIGGER brokerrouter_tool_result_immutable")
            connection.execute("UPDATE brokerrouter_tool_results SET result_ciphertext=?", (b"\x01" * 50,))
        before = len(self.server.requests)
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(len(self.server.requests), before)
        self.assertEqual(self.calls, [True])

    def test_legacy_completed_without_frozen_bytes_is_unrecoverable_without_dispatch(self):
        gate = self.gate()
        gate.store.stage(self.context.operation_key, self.context.identity_hash,
                         self.context.tool_name, hashlib.sha256(b'{"private":"input-secret"}').hexdigest(),
                         model_binding_hash=self.context.model_binding_hash)
        gate.store.checked(self.context.operation_key, self.server.input_id, "ready", "a" * 64)
        gate.store.prepare_claim(self.context.operation_key, "c" * 64)
        for expected, state in (("claim_prepared", "dispatch_authorized"),
                                ("dispatch_authorized", "dispatching"),
                                ("dispatching", "effect_recorded"), ("effect_recorded", "completed")):
            gate.store.transition(self.context.operation_key, {expected}, state, "opaque-evidence")
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, self.dispatch)
        self.assertEqual(halted.exception.code, "result_unavailable_after_restart")
        self.assertEqual(self.calls, [])
        self.assertEqual(self.server.requests, [])

    def test_non_string_and_invalid_utf8_results_halt_and_cannot_repeat(self):
        for index, value in enumerate((None, {"result": "metadata"}, b"bytes", "\ud800")):
            with self.subTest(index=index):
                context = replace(self.context, call_id=f"unsupported-{index}")
                server = ResultServer(context.turn_id)
                gate = self.gate(server=server)
                with self.assertRaises(GovernanceHalt):
                    gate.run_tool(context, lambda result=value: self.dispatch(result))
                self.assertEqual(gate.store.load(context.operation_key).state, "completed")
                with self.assertRaises(GovernanceHalt):
                    gate.run_tool(context, self.dispatch)
                self.assertFalse(any(path.endswith("/tool-results") for _, path, _ in server.requests))
        self.assertEqual(len(self.calls), 4)

    def test_wire_limit_counts_json_escaping_and_never_truncates_or_reexecutes(self):
        for index, value in enumerate(("x" * MAX_RESULT_WIRE_BYTES,
                                       "\x00" * (MAX_RESULT_WIRE_BYTES // 6 + 1))):
            with self.subTest(index=index):
                context = replace(self.context, call_id=f"wire-{index}")
                server = ResultServer(context.turn_id)
                gate = self.gate(server=server)
                with self.assertRaises(GovernanceHalt) as halted:
                    gate.run_tool(context, lambda result=value: self.dispatch(result))
                self.assertEqual(halted.exception.code, "result_wire_unsupported")
                self.assertEqual(gate.store.load(context.operation_key).state, "completed")
                self.assertEqual(gate.store.load_result(context.operation_key).text, value)
                with self.assertRaises(GovernanceHalt):
                    gate.run_tool(context, self.dispatch)
                self.assertFalse(any(path.endswith("/tool-results") for _, path, _ in server.requests))
        self.assertEqual(len(self.calls), 2)

    def test_unsupported_wire_lost_receipt_retries_execution_fact_without_dispatch(self):
        self.server.receipt_losses = 1
        value = "\x00" * (MAX_RESULT_WIRE_BYTES // 6 + 1)
        gate = self.gate()
        with self.assertRaises(GovernanceHalt) as first:
            gate.run_tool(self.context, lambda: self.dispatch(value))
        self.assertEqual(first.exception.code, "result_wire_unsupported")
        self.assertEqual(gate.store.load(self.context.operation_key).state, "effect_recorded")
        self.assertEqual(gate.store.load_result(self.context.operation_key).text, value)
        with self.assertRaises(GovernanceHalt) as retried:
            self.gate().run_tool(self.context, self.dispatch)
        self.assertEqual(retried.exception.code, "result_wire_unsupported")
        self.assertEqual(gate.store.load(self.context.operation_key).state, "completed")
        receipts = [body for _, path, body in self.server.requests if path.endswith("/receipt")]
        self.assertEqual(len(receipts), 2)
        self.assertEqual(receipts[0], receipts[1])
        self.assertEqual(receipts[0]["outcome"], "completed")
        self.assertEqual(self.calls, [True])
        self.assertFalse(any(path.endswith("/tool-results") for _, path, _ in self.server.requests))

    def test_exact_wire_boundary_and_empty_output_are_released(self):
        for index, value in enumerate(("", "x" * (MAX_RESULT_WIRE_BYTES - 2))):
            context = replace(self.context, call_id=f"boundary-{index}")
            server = ResultServer(context.turn_id)
            self.assertEqual(self.gate(server=server).run_tool(context, lambda result=value: result), value)

    def test_two_runtime_instances_cannot_dispatch_same_checkpoint_twice(self):
        first, second = self.gate(), self.gate()
        executing, finish = threading.Event(), threading.Event()
        failures = []

        def blocked_dispatch():
            self.calls.append(True)
            executing.set()
            self.assertTrue(finish.wait(5))
            return "private-result"

        def first_run():
            try:
                first.run_tool(self.context, blocked_dispatch)
            except GovernanceHalt as halted:
                failures.append(halted.code)

        thread = threading.Thread(target=first_run)
        thread.start()
        self.assertTrue(executing.wait(5))
        try:
            with self.assertRaises(GovernanceHalt):
                second.run_tool(self.context, self.dispatch)
        finally:
            finish.set()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(self.calls, [True])
        self.assertEqual(first.store.load(self.context.operation_key).state, "execution_unknown")
        self.assertTrue(failures)


if __name__ == "__main__":
    unittest.main()
