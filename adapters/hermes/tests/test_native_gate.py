from __future__ import annotations

from dataclasses import asdict, replace
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from uuid import uuid4

from brokerrouter_hermes.config import Config
from brokerrouter_hermes.gate import GovernanceGate, GovernanceHalt, ToolContext
from brokerrouter_hermes.store import CheckpointConflict, CheckpointStore
from test_output_gate import Clock, ResultServer


class NativeGateTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        os.chmod(self.temp.name, 0o700)
        self.config = Config(
            base_url="https://gateway.invalid", approvals_url="https://console.invalid",
            virtual_key="sk_br_" + "1" * 64, state_db=Path(self.temp.name) / "state.db",
            state_key=b"\x17" * 32, request_timeout_seconds=1,
            approval_poll_seconds=0.25, approval_wait_seconds=1,
        )
        self.origin = ToolContext(str(uuid4()), "session", "task", "call", "model",
                                  "fixture", {"original": "private-argument"})
        self.final = replace(self.origin, arguments={"modified": "actual-final-argument"})
        self.server = ResultServer(self.origin.turn_id)
        self.calls = []

    def tearDown(self):
        self.temp.cleanup()

    def gate(self):
        clock = Clock()
        return GovernanceGate(self.config, transport=self.server,
                              sleeper=clock.sleep, monotonic=lambda: clock.now)

    def execute(self):
        self.calls.append(True)
        return "raw executor result"

    def prepared(self, gate=None):
        gate = gate or self.gate()
        self.assertIsNone(gate.native_open(self.origin))
        self.assertEqual(gate.run_execution(self.final, self.execute), "raw executor result")
        return gate

    def test_raw_execution_is_not_frozen_and_only_final_middleware_text_is_governed(self):
        gate = self.prepared()
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "completed")
        self.assertIsNone(gate.store.load_result(self.origin.operation_key))
        self.assertFalse(any(path.endswith("/tool-results") for _, path, _ in self.server.requests))
        self.assertEqual(gate.finish_native(self.final, "final transformed e\u0301 \U0001f680"),
                         "final transformed e\u0301 \U0001f680")
        self.assertEqual(self.server.text, "final transformed e\u0301 \U0001f680")
        self.assertEqual(self.calls, [True])
        self.assertNotIn(b"actual-final-argument", self.config.state_db.read_bytes())
        self.assertNotIn(b"private-argument", self.config.state_db.read_bytes())

    def test_restore_empty_final_result_skips_execution_and_accepts_empty_string(self):
        gate = self.prepared()
        self.assertEqual(gate.finish_native(self.final, ""), "")
        self.assertEqual(self.gate().native_open(self.origin), "")
        self.assertEqual(self.calls, [True])
        self.assertEqual(sum(path.endswith("/claim") for _, path, _ in self.server.requests), 1)

    def test_restore_rechecks_new_output_policy_with_native_approval_callback(self):
        gate = self.prepared()
        self.assertEqual(gate.finish_native(self.final, "final-result"), "final-result")
        self.server.states = ["waiting_approval"]
        notices = []
        with self.assertRaises(GovernanceHalt) as held:
            self.gate().native_open(self.origin, approval=notices.append)
        self.assertEqual(held.exception.code, "approval_wait_timeout")
        self.assertEqual(len(notices), 1)
        self.server.states = ["ready"]
        self.assertEqual(self.gate().native_open(self.origin), "final-result")
        self.assertEqual(self.calls, [True])

    def test_known_effect_without_final_text_is_unrecoverable_and_cannot_be_backfilled(self):
        gate = self.prepared()
        recovered = self.gate()
        with self.assertRaises(GovernanceHalt) as lost:
            recovered.native_open(self.origin)
        self.assertEqual(lost.exception.code, "result_unavailable_after_restart")
        with self.assertRaises(GovernanceHalt):
            recovered.finish_native(self.final, "invented after restart")
        with self.assertRaises(GovernanceHalt):
            recovered.run_execution(self.final, self.execute)
        self.assertEqual(self.calls, [True])
        self.assertIsNone(gate.store.load_result(self.origin.operation_key))

    def test_final_argument_mutation_during_claim_never_reaches_dispatch(self):
        gate = self.gate()
        gate.native_open(self.origin)
        request = self.server.request

        def changed_claim(method, path, body=None):
            response = request(method, path, body)
            if path.endswith("/claim"):
                self.final.arguments["modified"] = "changed after authorization"
            return response

        self.server.request = changed_claim
        with self.assertRaises(GovernanceHalt) as changed:
            gate.run_execution(self.final, self.execute)
        self.assertEqual(changed.exception.code, "final_arguments_changed")
        self.assertEqual(self.calls, [])
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "execution_unknown")
        self.assertEqual([body["outcome"] for _, path, body in self.server.requests
                          if path.endswith("/receipt")], ["execution_unknown"])
        with self.assertRaises(GovernanceHalt):
            self.gate().native_open(self.origin)
        self.assertEqual(self.calls, [])

    def test_status_callback_cannot_modify_arguments_after_the_dispatch_claim(self):
        gate = self.gate()
        gate.native_open(self.origin)

        def status(value):
            if value == "Brokerrouter governance: dispatch_authorized":
                self.final.arguments["invalid"] = float("nan")

        with self.assertRaises(GovernanceHalt) as changed:
            gate.run_execution(self.final, self.execute, status=status)
        self.assertEqual(changed.exception.code, "final_arguments_changed")
        self.assertEqual(self.calls, [])
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "execution_unknown")

    def test_standalone_dispatch_rechecks_arguments_changed_during_claim(self):
        gate = self.gate()
        request = self.server.request

        def changed_claim(method, path, body=None):
            response = request(method, path, body)
            if path.endswith("/claim"):
                self.origin.arguments["original"] = "changed after authorization"
            return response

        self.server.request = changed_claim
        with self.assertRaises(GovernanceHalt) as changed:
            gate.run_tool(self.origin, self.execute)
        self.assertEqual(changed.exception.code, "final_arguments_changed")
        self.assertEqual(self.calls, [])
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "execution_unknown")

    def test_receipt_loss_does_not_lose_effect_and_final_record_repairs_original_receipt(self):
        self.server.receipt_losses = 1
        gate = self.prepared()
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "effect_recorded")
        self.assertEqual(gate.finish_native(self.final, "final-result"), "final-result")
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "completed")
        receipt = [body for _, path, body in self.server.requests if path.endswith("/receipt")][0]
        record = [body for _, path, body in self.server.requests if path.endswith("/tool-results")][0]
        self.assertEqual(receipt["token"], record["claim_token"])
        self.assertEqual(receipt["evidence_ref"], record["evidence_ref"])
        self.assertEqual(self.calls, [True])

    def test_record_response_loss_after_final_freeze_restores_without_relay_or_dispatch(self):
        gate = self.prepared()
        self.server.record_losses = 1
        with self.assertRaises(GovernanceHalt) as lost:
            gate.finish_native(self.final, "final-result")
        self.assertEqual(lost.exception.code, "receipt_not_persisted")
        self.assertEqual(self.gate().native_open(self.origin), "final-result")
        self.assertEqual(self.calls, [True])

    def test_final_freeze_commit_observation_loss_recovers_exact_frozen_bytes(self):
        gate = self.prepared()
        freeze = gate.store.freeze_native_result

        def committed_then_lost(*args, **kwargs):
            freeze(*args, **kwargs)
            raise RuntimeError("commit observation lost")

        with patch.object(gate.store, "freeze_native_result", side_effect=committed_then_lost):
            self.assertEqual(gate.finish_native(self.final, "final-result"), "final-result")
        with self.assertRaises(GovernanceHalt):
            gate.finish_native(self.final, "different final-result")
        self.assertEqual(self.calls, [True])

    def test_final_freeze_failure_keeps_known_effect_and_restart_cannot_repeat(self):
        gate = self.prepared()
        with patch.object(gate.store, "freeze_native_result", side_effect=CheckpointConflict("sealing failed")):
            with self.assertRaises(GovernanceHalt):
                gate.finish_native(self.final, "final-result")
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "completed")
        with self.assertRaises(GovernanceHalt):
            self.gate().native_open(self.origin)
        self.assertEqual(self.calls, [True])

    def test_native_effect_transaction_failure_rolls_back_marker_and_restart_is_unknown(self):
        gate = self.gate()
        self.assertIsNone(gate.native_open(self.origin))
        with sqlite3.connect(self.config.state_db) as connection:
            connection.execute("CREATE TRIGGER abort_native_effect BEFORE UPDATE OF state ON brokerrouter_checkpoints WHEN NEW.state='effect_recorded' BEGIN SELECT RAISE(ABORT,'injected write failure'); END")
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_execution(self.final, self.execute)
        self.assertEqual(halted.exception.code, "native_effect_not_persisted")
        with sqlite3.connect(self.config.state_db) as connection:
            marker = connection.execute("SELECT effect_recorded_at_ns FROM brokerrouter_native_origins").fetchone()[0]
        self.assertIsNone(marker)
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "dispatching")
        with self.assertRaises(GovernanceHalt) as restarted:
            self.gate().native_open(self.origin)
        self.assertEqual(restarted.exception.code, "execution_unknown")
        self.assertEqual(self.calls, [True])

    def test_native_effect_commit_observation_loss_preserves_current_execution_capability(self):
        gate = self.gate()
        self.assertIsNone(gate.native_open(self.origin))
        record = gate.store.record_native_effect

        def committed_then_lost(*args, **kwargs):
            record(*args, **kwargs)
            raise RuntimeError("effect observation lost after commit")

        with patch.object(gate.store, "record_native_effect", side_effect=committed_then_lost):
            self.assertEqual(gate.run_execution(self.final, self.execute), "raw executor result")
        self.assertEqual(gate.finish_native(self.final, "final-result"), "final-result")
        self.assertEqual(self.calls, [True])

    def test_native_unsupported_wire_retains_final_bytes_and_completed_execution(self):
        gate = self.prepared()
        final_text = "\x00" * 90_000
        with self.assertRaises(GovernanceHalt) as halted:
            gate.finish_native(self.final, final_text)
        self.assertEqual(halted.exception.code, "result_wire_unsupported")
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "completed")
        self.assertEqual(gate.store.load_result(self.origin.operation_key).text, final_text)
        with self.assertRaises(GovernanceHalt):
            self.gate().native_open(self.origin)
        self.assertEqual(self.calls, [True])

    def test_v2_preexecution_migration_retains_identity_and_rechecks_model(self):
        gate = self.gate()
        payload = hashlib.sha256(json.dumps(self.origin.arguments, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        gate.store.stage(self.origin.operation_key, self.origin.identity_hash, self.origin.tool_name, payload)
        gate.store.checked(self.origin.operation_key, self.server.input_id, "ready", "a" * 64)
        with sqlite3.connect(self.config.state_db) as connection:
            connection.execute("DROP TRIGGER brokerrouter_checkpoint_model_immutable")
            connection.execute("ALTER TABLE brokerrouter_checkpoints DROP COLUMN model_binding_hash")
            connection.execute("PRAGMA user_version=2")
        upgraded = self.gate()
        self.assertEqual(upgraded.run_tool(self.origin, self.execute), "raw executor result")
        self.assertEqual(upgraded.store.load(self.origin.operation_key).operation_id, self.server.input_id)
        self.assertEqual(upgraded.store.load(self.origin.operation_key).model_binding_hash, self.origin.model_binding_hash)
        self.assertEqual(sum(path.endswith("/checks") for _, path, _ in self.server.requests), 1)

    def test_result_encryption_authenticates_model_binding(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.origin, self.execute), "raw executor result")
        with sqlite3.connect(self.config.state_db) as connection:
            connection.execute("DROP TRIGGER brokerrouter_checkpoint_model_immutable")
            connection.execute("UPDATE brokerrouter_checkpoints SET model_binding_hash=?", ("0" * 64,))
        with self.assertRaises(CheckpointConflict):
            gate.store.load_result(self.origin.operation_key)

    def test_original_model_tool_and_final_argument_drift_cannot_create_a_new_dispatch(self):
        gate = self.prepared()
        self.assertEqual(gate.finish_native(self.final, "final-result"), "final-result")
        for context in (replace(self.origin, arguments={"changed": True}),
                        replace(self.origin, model_id="different-model"),
                        replace(self.origin, tool_name="different-tool")):
            with self.subTest(context=context):
                self.assertEqual(context.operation_key, self.origin.operation_key)
                with self.assertRaises(GovernanceHalt):
                    self.gate().native_open(context)
        with self.assertRaises(GovernanceHalt):
            self.gate().run_execution(replace(self.final, arguments={"changed": True}), self.execute)
        with self.assertRaises(GovernanceHalt):
            gate.finish_native(replace(self.final, arguments={"changed": True}), "final-result")
        self.assertEqual(self.calls, [True])

    def test_final_arguments_are_immutable_before_claim_and_can_differ_from_original(self):
        gate = self.gate()
        self.assertIsNone(gate.native_open(self.origin))
        original = self.server.request

        def input_pending(method, path, body=None):
            if path.endswith("/checks"):
                return {"id": self.server.input_id, "state": "checking"}
            return original(method, path, body)

        with patch.object(self.server, "request", side_effect=input_pending):
            with self.assertRaises(GovernanceHalt):
                gate.run_execution(self.final, self.execute)
        with self.assertRaises(GovernanceHalt):
            gate.run_execution(replace(self.final, arguments={"different": True}), self.execute)
        self.assertEqual(self.calls, [])
        self.assertEqual(gate.run_execution(self.final, self.execute), "raw executor result")
        checks = [body for _, path, body in self.server.requests if path.endswith("/checks")]
        self.assertEqual(checks[-1]["payload"], self.final.arguments)

    def test_execution_and_finish_require_origin_and_native_execution_facts(self):
        gate = self.gate()
        with self.assertRaises(GovernanceHalt):
            gate.run_execution(self.final, self.execute)
        self.assertIsNone(gate.native_open(self.origin))
        with self.assertRaises(GovernanceHalt):
            gate.finish_native(self.final, "unexecuted")
        with self.assertRaises(GovernanceHalt):
            gate.run_tool(self.final, self.execute)
        self.assertEqual(self.calls, [])

    def test_standalone_model_drift_uses_same_key_and_fails_closed(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.origin, self.execute), "raw executor result")
        changed = replace(self.origin, model_id="different-model")
        self.assertEqual(changed.operation_key, self.origin.operation_key)
        with self.assertRaises(GovernanceHalt):
            self.gate().run_tool(changed, self.execute)
        self.assertEqual(self.calls, [True])

    def test_native_origin_cannot_be_attached_to_legacy_completed_execution(self):
        gate = self.gate()
        self.assertEqual(gate.run_tool(self.origin, self.execute), "raw executor result")
        with self.assertRaises(GovernanceHalt):
            self.gate().native_open(self.origin)
        with self.assertRaises(CheckpointConflict):
            gate.store.freeze_native_result(self.origin.operation_key, "forged", "evidence")
        self.assertEqual(self.calls, [True])

    def test_legacy_preexecution_model_binding_forces_full_gateway_recheck(self):
        gate = self.gate()
        payload = hashlib.sha256(json.dumps(self.origin.arguments, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        gate.store.stage(self.origin.operation_key, self.origin.identity_hash, self.origin.tool_name, payload)
        gate.store.checked(self.origin.operation_key, self.server.input_id, "ready", "a" * 64)
        self.assertEqual(gate.run_tool(self.origin, self.execute), "raw executor result")
        checks = [body for _, path, body in self.server.requests if path.endswith("/checks")]
        self.assertEqual(len(checks), 1)
        self.assertEqual(checks[0]["model_id"], self.origin.model_id)
        self.assertEqual(gate.store.load(self.origin.operation_key).model_binding_hash, self.origin.model_binding_hash)

    def test_legacy_claim_without_model_binding_fails_closed_without_claim_or_dispatch(self):
        gate = self.gate()
        payload = hashlib.sha256(json.dumps(self.origin.arguments, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        gate.store.stage(self.origin.operation_key, self.origin.identity_hash, self.origin.tool_name, payload)
        gate.store.checked(self.origin.operation_key, self.server.input_id, "ready", "a" * 64)
        gate.store.prepare_claim(self.origin.operation_key, "c" * 64)
        with self.assertRaises(GovernanceHalt):
            gate.run_tool(self.origin, self.execute)
        self.assertEqual(self.calls, [])
        self.assertEqual(self.server.requests, [])

    def test_native_facts_are_immutable_and_ordinary_freeze_cannot_bypass_final_boundary(self):
        gate = self.prepared()
        with self.assertRaises(CheckpointConflict):
            gate.store.freeze_result(self.origin.operation_key, "raw", "evidence")
        with sqlite3.connect(self.config.state_db) as connection:
            for statement in (
                "UPDATE brokerrouter_native_origins SET original_payload_hash='" + "0" * 64 + "'",
                "UPDATE brokerrouter_native_origins SET final_payload_hash=NULL",
                "UPDATE brokerrouter_native_origins SET effect_recorded_at_ns=NULL",
                "DELETE FROM brokerrouter_native_origins",
                "UPDATE brokerrouter_checkpoints SET model_binding_hash=NULL",
            ):
                with self.assertRaises(sqlite3.IntegrityError):
                    connection.execute(statement)

    def test_sigkill_at_known_effect_before_final_freeze_never_executes_again(self):
        self.kill_child("effect")
        gate = self.gate()
        with self.assertRaises(GovernanceHalt) as halted:
            gate.native_open(self.origin)
        self.assertEqual(halted.exception.code, "result_unavailable_after_restart")
        with self.assertRaises(GovernanceHalt):
            gate.run_execution(self.final, self.execute)
        self.assertEqual(self.calls, [])
        self.assertEqual(gate.store.load(self.origin.operation_key).state, "completed")
        self.assertIsNone(gate.store.load_result(self.origin.operation_key))

    def test_sigkill_after_final_freeze_commit_restores_exact_final_text(self):
        self.kill_child("final")
        self.assertEqual(self.gate().native_open(self.origin), "child-final-exact")
        self.assertEqual(self.calls, [])
        self.assertEqual(sum(path.endswith("/claim") for _, path, _ in self.server.requests), 0)

    def kill_child(self, point):
        script = r'''
import json, os, signal, sys
from dataclasses import replace
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from test_output_gate import Clock, ResultServer
from brokerrouter_hermes.config import Config
from brokerrouter_hermes.gate import GovernanceGate, ToolContext
origin = ToolContext(**json.loads(sys.argv[3]))
final = replace(origin, arguments={"modified":"actual-final-argument"})
server = ResultServer(origin.turn_id)
server.input_id = sys.argv[4]
clock = Clock()
gate = GovernanceGate(Config(base_url="https://gateway.invalid", approvals_url="https://console.invalid",
    virtual_key="sk_br_"+"1"*64, state_db=Path(sys.argv[2]), state_key=b"\x17"*32,
    request_timeout_seconds=1, approval_poll_seconds=.25, approval_wait_seconds=1),
    transport=server, sleeper=clock.sleep, monotonic=lambda:clock.now)
assert gate.native_open(origin) is None
assert gate.run_execution(final, lambda:"child-raw") == "child-raw"
if sys.argv[5] == "effect":
    os.kill(os.getpid(), signal.SIGKILL)
freeze = gate.store.freeze_native_result
def committed_then_killed(*args, **kwargs):
    freeze(*args, **kwargs)
    os.kill(os.getpid(), signal.SIGKILL)
gate.store.freeze_native_result = committed_then_killed
gate.finish_native(final, "child-final-exact")
'''
        result = subprocess.run(
            [sys.executable, "-c", script, str(Path(__file__).parent), str(self.config.state_db),
             json.dumps(asdict(self.origin)), self.server.input_id, point],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10,
        )
        self.assertEqual(result.returncode, -signal.SIGKILL, result.stderr.decode())


if __name__ == "__main__":
    unittest.main()
