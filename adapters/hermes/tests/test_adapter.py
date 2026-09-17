from __future__ import annotations

import json
import os
from contextlib import contextmanager
from pathlib import Path
import sqlite3
import tempfile
import unittest
from uuid import uuid4

from brokerrouter_hermes.client import GatewayFailure, TransportFailure
from brokerrouter_hermes.config import Config, ConfigurationError
from brokerrouter_hermes.gate import GovernanceGate, GovernanceHalt, ToolContext


class FakeTransport:
    def __init__(
        self,
        checks,
        *,
        claim=True,
        claim_error=False,
        receipt_error=False,
        check_error=None,
    ):
        self.checks = list(checks)
        self.claim_result = claim
        self.claim_error = claim_error
        self.receipt_error = receipt_error
        self.check_error = check_error
        self.requests = []

    def request(self, method, path, body=None):
        self.requests.append((method, path, body))
        if path == "/v1/governance/checks":
            if self.check_error is not None:
                raise self.check_error
            if len(self.checks) > 1:
                return self.checks.pop(0)
            return self.checks[0]
        if path.endswith("/claim"):
            if self.claim_error:
                raise TransportFailure("lost response")
            return {
                "id": path.split("/")[-2],
                "state": "executing",
                "payload_hash": body["payload_hash"],
                "dispatch_authorized": self.claim_result,
            }
        if path.endswith("/receipt"):
            if self.receipt_error:
                raise TransportFailure("lost receipt")
            return {"outcome": body["outcome"], "evidence_ref": body["evidence_ref"]}
        raise AssertionError(path)


class AdapterTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        os.chmod(self.temp.name, 0o700)
        self.operation = str(uuid4())
        self.config = Config(
            base_url="https://gateway.invalid",
            virtual_key="sk_br_" + "1" * 64,
            state_db=Path(self.temp.name) / "governance.db",
            state_key=b"\x17" * 32,
            request_timeout_seconds=1,
            approval_poll_seconds=0.25,
            approval_wait_seconds=30,
        )
        self.context = ToolContext(
            turn_id=str(uuid4()),
            session_id="session-1",
            task_id="task-1",
            call_id="call-1",
            model_id="logical-model",
            tool_name="send_message",
            arguments={"recipient": "fixture", "text": "synthetic"},
        )

    def tearDown(self):
        self.temp.cleanup()

    def response(self, state, *, payload_hash=None):
        value = {"id": self.operation, "state": state}
        if payload_hash is not None:
            value["payload_hash"] = payload_hash
        return value

    def gate(self, transport):
        return GovernanceGate(self.config, transport=transport, sleeper=lambda _: None)

    def test_waits_for_same_operation_then_dispatches_once(self):
        transport = FakeTransport(
            [
                self.response("waiting_approval"),
                self.response("ready", payload_hash="a" * 64),
            ]
        )
        gate = self.gate(transport)
        calls = []
        result = gate.run_tool(self.context, lambda: calls.append("called") or "result")
        self.assertEqual(result, "result")
        self.assertEqual(calls, ["called"])
        checkpoint = gate.store.load(self.context.operation_key)
        self.assertEqual(checkpoint.state, "completed")
        checks = [request for request in transport.requests if request[1] == "/v1/governance/checks"]
        self.assertEqual(len(checks), 2)
        self.assertEqual(checks[0][2], checks[1][2])
        self.assertNotIn("synthetic", self.config.state_db.read_bytes().decode("latin1"))

    def test_only_human_approval_sleep_extends_hermes_wait_window(self):
        transport = FakeTransport(
            [
                self.response("checking"),
                self.response("waiting_approval"),
                self.response("ready", payload_hash="9" * 64),
            ]
        )
        windows = []

        @contextmanager
        def human_wait():
            windows.append("open")
            try:
                yield
            finally:
                windows.append("close")

        gate = GovernanceGate(
            self.config,
            transport=transport,
            sleeper=lambda _: None,
            human_wait=human_wait,
        )
        self.assertEqual(gate.run_tool(self.context, lambda: "ok"), "ok")
        self.assertEqual(windows, ["open", "close"])

    def test_unknown_client_contract_error_halts_without_retry_or_dispatch(self):
        transport = FakeTransport(
            [self.response("checking")],
            check_error=GatewayFailure(422, "unsupported_contract"),
        )
        gate = self.gate(transport)
        called = []
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, lambda: called.append(True))
        self.assertEqual(halted.exception.code, "unsupported_contract")
        self.assertEqual(called, [])
        self.assertEqual(len(transport.requests), 1)

    def test_claim_response_loss_never_dispatches_or_reclaims(self):
        transport = FakeTransport(
            [self.response("ready", payload_hash="b" * 64)], claim_error=True
        )
        gate = self.gate(transport)
        called = []
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, lambda: called.append(True))
        self.assertEqual(halted.exception.code, "claim_response_unknown")
        self.assertEqual(called, [])
        self.assertEqual(gate.store.load(self.context.operation_key).state, "claim_unknown")
        with self.assertRaises(GovernanceHalt) as replayed:
            gate.run_tool(self.context, lambda: called.append(True))
        self.assertEqual(replayed.exception.code, "claim_unknown")
        self.assertEqual(called, [])

    def test_tool_exception_is_receipted_unknown_and_cannot_repeat(self):
        transport = FakeTransport([self.response("ready", payload_hash="c" * 64)])
        gate = self.gate(transport)
        called = []

        def uncertain():
            called.append(True)
            raise RuntimeError("synthetic failure after possible effect")

        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, uncertain)
        self.assertEqual(halted.exception.code, "execution_unknown")
        self.assertEqual(called, [True])
        self.assertEqual(gate.store.load(self.context.operation_key).state, "execution_unknown")
        receipt = [request for request in transport.requests if request[1].endswith("/receipt")][-1]
        self.assertEqual(receipt[2]["outcome"], "execution_unknown")
        self.assertNotIn("synthetic failure", json.dumps(receipt[2]))
        with self.assertRaises(GovernanceHalt):
            gate.run_tool(self.context, uncertain)
        self.assertEqual(called, [True])

    def test_receipt_loss_holds_result_and_restart_only_repairs_receipt(self):
        transport = FakeTransport(
            [self.response("ready", payload_hash="d" * 64)], receipt_error=True
        )
        gate = self.gate(transport)
        called = []
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, lambda: called.append(True) or "secret-result")
        self.assertEqual(halted.exception.code, "receipt_not_persisted")
        self.assertEqual(called, [True])
        self.assertEqual(gate.store.load(self.context.operation_key).state, "effect_recorded")

        repaired = FakeTransport([self.response("ready", payload_hash="d" * 64)])
        recovered = self.gate(repaired)
        with self.assertRaises(GovernanceHalt) as restarted:
            recovered.run_tool(self.context, lambda: called.append(True))
        self.assertEqual(restarted.exception.code, "result_unavailable_after_restart")
        self.assertEqual(called, [True])
        self.assertEqual(recovered.store.load(self.context.operation_key).state, "completed")

    def test_restart_after_authorization_becomes_unknown_without_dispatch(self):
        transport = FakeTransport([self.response("ready", payload_hash="e" * 64)])
        gate = self.gate(transport)
        checkpoint = gate.store.stage(
            self.context.operation_key,
            self.context.identity_hash,
            self.context.tool_name,
            __import__("hashlib").sha256(
                json.dumps(
                    self.context.arguments,
                    ensure_ascii=False,
                    sort_keys=True,
                    separators=(",", ":"),
                ).encode()
            ).hexdigest(),
        )
        gate.store.checked(
            checkpoint.operation_key, self.operation, "ready", "e" * 64
        )
        gate.store.prepare_claim(checkpoint.operation_key, "f" * 64)
        gate.store.transition(checkpoint.operation_key, {"claim_prepared"}, "dispatch_authorized")
        called = []
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, lambda: called.append(True))
        self.assertEqual(halted.exception.code, "execution_unknown")
        self.assertEqual(called, [])
        self.assertEqual(gate.store.load(self.context.operation_key).state, "execution_unknown")

    def test_final_argument_or_identity_change_conflicts(self):
        gate = self.gate(FakeTransport([self.response("waiting_approval")]))
        gate.store.stage(
            self.context.operation_key,
            self.context.identity_hash,
            self.context.tool_name,
            "0" * 64,
        )
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, lambda: None)
        self.assertEqual(halted.exception.code, "adapter_internal_failure")

    def test_claim_token_is_encrypted_at_rest(self):
        gate = self.gate(FakeTransport([self.response("ready", payload_hash="1" * 64)]))
        gate.store.stage(
            self.context.operation_key,
            self.context.identity_hash,
            self.context.tool_name,
            "2" * 64,
        )
        gate.store.checked(self.context.operation_key, self.operation, "ready", "1" * 64)
        token = "f" * 64
        checkpoint = gate.store.prepare_claim(self.context.operation_key, token)
        self.assertEqual(checkpoint.claim_token, token)
        connection = sqlite3.connect(self.config.state_db)
        raw = connection.execute(
            "SELECT claim_ciphertext FROM brokerrouter_checkpoints WHERE operation_key=?",
            (self.context.operation_key,),
        ).fetchone()[0]
        connection.close()
        self.assertNotIn(token.encode(), raw)
        self.assertEqual(self.config.state_db.stat().st_mode & 0o777, 0o600)
        self.assertNotIn(token, repr(checkpoint))
        self.assertNotIn(self.config.virtual_key, repr(self.config))
        self.assertNotIn(self.config.state_key.hex(), repr(self.config))


class ConfigTest(unittest.TestCase):
    def test_disabled_needs_no_secrets(self):
        self.assertIsNone(Config.from_env({}))

    def test_required_rejects_remote_plain_http_and_bad_key(self):
        base = {
            "BROKERROUTER_HERMES_GOVERNANCE": "required",
            "BROKERROUTER_BASE_URL": "http://gateway.example",
            "BROKERROUTER_VIRTUAL_KEY": "wrong",
            "BROKERROUTER_HERMES_STATE_DB": "/tmp/state.db",
            "BROKERROUTER_HERMES_STATE_KEY": "11" * 32,
        }
        with self.assertRaises(ConfigurationError):
            Config.from_env(base)


if __name__ == "__main__":
    unittest.main()
