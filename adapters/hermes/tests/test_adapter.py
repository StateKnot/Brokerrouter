from __future__ import annotations

import json
import os
from contextlib import contextmanager
import asyncio
import hashlib
from pathlib import Path
import sqlite3
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from uuid import NAMESPACE_URL, uuid4, uuid5

from brokerrouter_hermes.client import GatewayFailure, TransportFailure
from brokerrouter_hermes.config import Config, ConfigurationError
from brokerrouter_hermes.gate import (
    GatewayTurnBinding,
    GovernanceGate,
    GovernanceHalt,
    ToolContext,
    bind_gateway_agent,
)
from brokerrouter_hermes.integrity import verify_root
import brokerrouter_hermes.integrity as integrity
from brokerrouter_hermes.store import CheckpointStore


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
            self.turn_id = body["turn_id"]
            if self.check_error is not None:
                raise self.check_error
            if len(self.checks) > 1:
                return self.checks.pop(0)
            return self.checks[0]
        if path.endswith("/bind"):
            return {"id": path.split("/")[-2], "state": "active", **body}
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
        if path == "/v1/governance/tool-results":
            if self.receipt_error:
                raise TransportFailure("lost result receipt")
            self.result_id = str(uuid5(NAMESPACE_URL, body["input_operation_id"]))
            self.result_text = body["text"]
            self.result_metadata = {
                "input_operation_id": body["input_operation_id"],
                "turn_id": self.turn_id,
                "text_sha256": hashlib.sha256(body["text"].encode("utf-8")).hexdigest(),
                "bytes": len(body["text"].encode("utf-8")),
                "expires_at": "2030-01-01T00:00:00Z",
            }
            return {"id": self.result_id, "state": "recorded", **self.result_metadata}
        if path.endswith("/release"):
            output_id = str(uuid5(NAMESPACE_URL, self.result_id))
            return {
                "id": output_id, "output_operation_id": output_id,
                "result_id": self.result_id, "state": "ready",
                "payload_hash": "a" * 64, "text": self.result_text, **self.result_metadata,
            }
        raise AssertionError(path)


class AdapterTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        os.chmod(self.temp.name, 0o700)
        self.operation = str(uuid4())
        self.config = Config(
            base_url="https://gateway.invalid",
            approvals_url="https://console.invalid",
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
        if state == "waiting_approval":
            value["approval"] = {
                "id": str(uuid4()),
                "operation_id": self.operation,
                "turn_id": self.context.turn_id,
                "state": "pending",
                "scope": "current_turn_only",
                "resume_requires_recheck": True,
                "options": ["continue_this_turn", "reject", "feedback"],
            }
        if payload_hash is not None:
            value["payload_hash"] = payload_hash
        return value

    def gate(self, transport):
        transport.turn_id = self.context.turn_id
        return GovernanceGate(self.config, transport=transport, sleeper=lambda _: None)

    def test_waits_for_same_operation_then_dispatches_once(self):
        waiting = self.response("waiting_approval")
        transport = FakeTransport(
            [
                waiting,
                waiting,
                self.response("ready", payload_hash="a" * 64),
            ]
        )
        gate = self.gate(transport)
        calls = []
        notices = []
        result = gate.run_tool(
            self.context,
            lambda: calls.append("called") or "result",
            approval=notices.append,
        )
        self.assertEqual(result, "result")
        self.assertEqual(calls, ["called"])
        checkpoint = gate.store.load(self.context.operation_key)
        self.assertEqual(checkpoint.state, "completed")
        checks = [request for request in transport.requests if request[1] == "/v1/governance/checks"]
        self.assertEqual(len(checks), 3)
        self.assertEqual(checks[0][2], checks[1][2])
        self.assertEqual(len(notices), 1)
        self.assertEqual(notices[0]["approval_id"], waiting["approval"]["id"])
        self.assertEqual(notices[0]["review_url"], "https://console.invalid/#approvals")
        self.assertNotIn("synthetic", self.config.state_db.read_bytes().decode("latin1"))

    def test_malformed_approval_contract_halts_before_dispatch(self):
        waiting = self.response("waiting_approval")
        waiting["approval"]["resume_requires_recheck"] = False
        gate = self.gate(FakeTransport([waiting]))
        called = []
        with self.assertRaises(GovernanceHalt) as halted:
            gate.run_tool(self.context, lambda: called.append(True))
        self.assertEqual(halted.exception.code, "gateway_approval_invalid")
        self.assertEqual(called, [])

    def test_failed_notice_is_released_and_retried_without_dispatch(self):
        waiting = self.response("waiting_approval")
        gate = self.gate(FakeTransport([waiting]))
        attempts = []

        def fail_notice(details):
            attempts.append(details["approval_id"])
            raise RuntimeError("synthetic connector refusal")

        with self.assertRaises(GovernanceHalt) as first:
            gate.run_tool(self.context, lambda: self.fail("must not dispatch"), approval=fail_notice)
        self.assertEqual(first.exception.code, "adapter_internal_failure")
        with self.assertRaises(GovernanceHalt):
            gate.run_tool(self.context, lambda: self.fail("must not dispatch"), approval=fail_notice)
        self.assertEqual(attempts, [waiting["approval"]["id"], waiting["approval"]["id"]])

    def test_runtime_turn_binding_uses_every_host_identity(self):
        transport = FakeTransport([self.response("checking")])
        gate = self.gate(transport)
        binding = GatewayTurnBinding(
            turn_id=self.context.turn_id,
            session_id=self.context.session_id,
            message_id="message-1",
            run_id="run-1",
            generation=7,
        )
        self.assertEqual(gate.bind_turn(binding), self.context.turn_id)
        request = transport.requests[-1]
        self.assertEqual(
            request,
            (
                "POST",
                f"/v1/governance/turns/{self.context.turn_id}/bind",
                {
                    "session_id": "session-1",
                    "message_id": "message-1",
                    "run_id": "run-1",
                    "generation": 7,
                },
            ),
        )

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

    def test_receipt_loss_holds_result_and_restart_repairs_and_rechecks_output(self):
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
        self.assertEqual(
            recovered.run_tool(self.context, lambda: called.append(True)), "secret-result"
        )
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
            model_binding_hash=self.context.model_binding_hash,
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

    def test_concurrent_approval_notice_reservation_has_one_sender(self):
        gate = self.gate(FakeTransport([self.response("checking")]))
        gate.store.stage(
            self.context.operation_key,
            self.context.identity_hash,
            self.context.tool_name,
            "3" * 64,
        )
        approval_id = str(uuid4())
        barrier = threading.Barrier(8)
        results = []
        failures = []

        def reserve():
            try:
                barrier.wait(timeout=5)
                results.append(
                    gate.store.reserve_approval_notice(
                        self.context.operation_key, approval_id
                    )
                )
            except BaseException as exc:
                failures.append(exc)

        threads = [threading.Thread(target=reserve) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=10)
        self.assertEqual(failures, [])
        self.assertEqual(len(results), 8)
        self.assertEqual(sum(results), 1)


class ConfigTest(unittest.TestCase):
    def test_disabled_needs_no_secrets(self):
        self.assertIsNone(Config.from_env({}))

    def test_required_rejects_remote_plain_http_and_bad_key(self):
        base = {
            "BROKERROUTER_HERMES_GOVERNANCE": "required",
            "BROKERROUTER_BASE_URL": "http://gateway.example",
            "BROKERROUTER_HERMES_APPROVALS_URL": "https://console.example",
            "BROKERROUTER_VIRTUAL_KEY": "wrong",
            "BROKERROUTER_HERMES_STATE_DB": "/tmp/state.db",
            "BROKERROUTER_HERMES_STATE_KEY": "11" * 32,
        }
        with self.assertRaises(ConfigurationError):
            Config.from_env(base)

    def test_required_rejects_approval_url_with_path(self):
        values = {
            "BROKERROUTER_HERMES_GOVERNANCE": "required",
            "BROKERROUTER_BASE_URL": "https://gateway.example",
            "BROKERROUTER_HERMES_APPROVALS_URL": "https://console.example/tenant",
            "BROKERROUTER_VIRTUAL_KEY": "sk_br_" + "1" * 64,
            "BROKERROUTER_HERMES_STATE_DB": "/tmp/state.db",
            "BROKERROUTER_HERMES_STATE_KEY": "11" * 32,
        }
        with self.assertRaises(ConfigurationError):
            Config.from_env(values)


class StoreMigrationTest(unittest.TestCase):
    def test_version_one_database_is_upgraded_without_losing_existing_tables(self):
        with tempfile.TemporaryDirectory() as directory:
            os.chmod(directory, 0o700)
            path = Path(directory) / "state.db"
            connection = sqlite3.connect(path)
            connection.execute("CREATE TABLE legacy_marker(value TEXT NOT NULL)")
            connection.execute("INSERT INTO legacy_marker VALUES('kept')")
            connection.execute("PRAGMA user_version=1")
            connection.commit()
            connection.close()

            CheckpointStore(path, b"\x18" * 32)
            connection = sqlite3.connect(path)
            version = connection.execute("PRAGMA user_version").fetchone()[0]
            marker = connection.execute("SELECT value FROM legacy_marker").fetchone()[0]
            table = connection.execute(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='brokerrouter_approval_notices'"
            ).fetchone()
            connection.close()
            self.assertEqual(version, 3)
            self.assertEqual(marker, "kept")
            self.assertEqual(table[0], "brokerrouter_approval_notices")


class HostBindingTest(unittest.TestCase):
    def test_host_identities_are_bound_and_native_notice_contains_no_grant_path(self):
        turn_id = str(uuid4())
        approval_id = str(uuid4())
        observed = []
        closed = []

        class Gate:
            def bind_turn(self, binding):
                observed.append(binding)
                return binding.turn_id

        class Adapter:
            async def send(self, chat_id, message, metadata=None):
                observed.append((chat_id, message, metadata))
                return SimpleNamespace(success=True)

        loop = asyncio.new_event_loop()
        thread = threading.Thread(target=loop.run_forever, daemon=True)
        thread.start()
        try:
            context = SimpleNamespace(
                _brokerrouter_turn_binding={"turn_id": turn_id, "run_id": "run-1"},
                session_id="session-1",
                inbound_message_id="message-1",
                run_generation=3,
                _status_adapter=Adapter(),
                _voice_ack_loop=loop,
                _status_thread_metadata={"thread_id": "thread-1"},
                _status_chat_id="chat-1",
            )
            runner = SimpleNamespace(
                _close_native_stream_boundary=lambda reason: closed.append(reason)
            )
            agent = SimpleNamespace()
            with patch("brokerrouter_hermes.gate.gate_for_agent", return_value=Gate()):
                bind_gateway_agent(agent, context, runner)
            self.assertEqual(agent._brokerrouter_turn_id, turn_id)
            binding = observed[0]
            self.assertEqual(
                (binding.session_id, binding.message_id, binding.run_id, binding.generation),
                ("session-1", "message-1", "run-1", 3),
            )

            agent._brokerrouter_approval_callback(
                {
                    "approval_id": approval_id,
                    "turn_id": turn_id,
                    "review_url": "https://console.example/#approvals",
                }
            )
            self.assertEqual(closed, ["Brokerrouter approval"])
            chat_id, message, metadata = observed[1]
            self.assertEqual(chat_id, "chat-1")
            self.assertIn(approval_id, message)
            self.assertIn("https://console.example/#approvals", message)
            self.assertIn("\u540c\u610f\u201d\u4e0d\u4f1a\u6388\u6743", message)
            self.assertEqual(metadata["brokerrouter_approval_id"], approval_id)
            self.assertNotIn("token", json.dumps(metadata))
        finally:
            loop.call_soon_threadsafe(loop.stop)
            thread.join(timeout=5)
            loop.close()

    def test_missing_trusted_metadata_fails_before_binding(self):
        agent = SimpleNamespace(_brokerrouter_turn_id="stale")
        context = SimpleNamespace(_brokerrouter_turn_binding=None)
        with patch("brokerrouter_hermes.gate.gate_for_agent", return_value=SimpleNamespace()):
            with self.assertRaises(GovernanceHalt) as halted:
                bind_gateway_agent(agent, context, SimpleNamespace())
        self.assertEqual(halted.exception.code, "governance_turn_binding_missing")
        self.assertIsNone(agent._brokerrouter_turn_id)


class InstallationIntegrityTest(unittest.TestCase):
    def test_exact_manifest_and_files_are_required(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            host = root / "gateway" / "host.py"
            adapter = root / "brokerrouter_hermes" / "fixture.py"
            host.parent.mkdir(parents=True)
            adapter.parent.mkdir(parents=True)
            host.write_bytes(b"fixed-host")
            adapter.write_bytes(b"fixed-adapter")
            host_hash = __import__("hashlib").sha256(host.read_bytes()).hexdigest()
            adapter_hash = __import__("hashlib").sha256(adapter.read_bytes()).hexdigest()
            expected_host = {
                "gateway/host.py": {
                    "original_sha256": "0" * 64,
                    "patched_sha256": host_hash,
                }
            }
            manifest = {
                "schema": 2,
                "hermes_revision": integrity.REVISION,
                "host_files": expected_host,
                "adapter_files": {"brokerrouter_hermes/fixture.py": adapter_hash},
            }
            (root / "brokerrouter_hermes_install.json").write_text(
                json.dumps(manifest), encoding="utf-8"
            )
            with patch.object(integrity, "HOST_FILES", expected_host):
                verify_root(root)
                adapter.write_bytes(b"drift")
                with self.assertRaises(RuntimeError):
                    verify_root(root)


if __name__ == "__main__":
    unittest.main()
