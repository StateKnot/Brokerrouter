"""Actual fixed native final-send/restart paths with a zero-fee transport.

This verifies durable once/unknown facts, not final assistant output approval or
an external platform's idempotency/reconciliation certification.
"""
from __future__ import annotations
import asyncio
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from uuid import uuid4


@unittest.skipUnless(os.environ.get("BROKERROUTER_HERMES_NATIVE_CONTRACT") == "1", "requires complete fixed Hermes final delivery host")
class NativePublicationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from gateway.platforms.base import BasePlatformAdapter, Platform, PlatformConfig, SendResult
        from gateway.run_startup import GatewayStartupMixin
        from gateway import delivery_ledger
        from brokerrouter_hermes import publication
        cls.Base, cls.Platform, cls.PlatformConfig, cls.SendResult = BasePlatformAdapter, Platform, PlatformConfig, SendResult
        cls.Startup, cls.ledger, cls.publication = GatewayStartupMixin, delivery_ledger, publication

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        self.env = patch.dict(os.environ, {
            "HERMES_HOME": str(self.home), "BROKERROUTER_HERMES_GOVERNANCE": "required",
            "BROKERROUTER_BASE_URL": "https://fixture.invalid",
            "BROKERROUTER_HERMES_APPROVALS_URL": "https://fixture.invalid",
            "BROKERROUTER_VIRTUAL_KEY": "sk_br_" + "1" * 64,
            "BROKERROUTER_HERMES_STATE_DB": str(self.home / "gate.db"),
            "BROKERROUTER_HERMES_STATE_KEY": "17" * 32,
        })
        self.env.start()
        self.enabled = patch.object(self.ledger, "ledger_enabled", return_value=True)
        self.enabled.start()
        self.sends = []
        self.sender = None
        owner = self

        class LocalTransport(self.Base):
            name = "native-publication-zero-fee"

            async def connect(self): return True
            async def disconnect(self): return None
            async def get_chat_info(self, _chat): return {}
            async def send(self, chat_id, content, **kwargs):
                owner.sends.append((chat_id, content, kwargs))
                if owner.sender is not None:
                    return await owner.sender(chat_id, content, kwargs)
                return owner.SendResult(success=True, message_id="local-ack", continuation_message_ids=("local-first",))

        self.Adapter = LocalTransport
        self.adapter = self.Adapter(self.PlatformConfig(), self.Platform.TELEGRAM)
        self.event = SimpleNamespace(text="fixture inbound", message_id="fixed-inbound", ledger_message_id=None,
            metadata={"brokerrouter_turn": {"turn_id": str(uuid4()), "run_id": "fixture-run"}},
            source=SimpleNamespace(platform=self.Platform.TELEGRAM, chat_id="fixed-chat", thread_id=None))
        self.body = "PRIVATE_RELEASED_TEXT_FIXTURE"

    def tearDown(self):
        self.enabled.stop()
        self.env.stop()
        self.temp.cleanup()

    async def final(self, text=None, metadata=None, reply_to=None):
        # This suite tests the existing ledger's once/unknown public seam.
        # Dedicated current final authority is tested through the real Base
        # sender in test_final_publication; no local ACK is a release grant.
        result, _adapter = await self.publication.send_final_once(
            self.adapter,
            self.event, "fixed-session", self.body if text is None else text,
            {} if metadata is None else metadata, reply_to=reply_to, is_ephemeral_response=False,
        )
        return result

    def rows(self):
        with closing(self.ledger._connect()) as conn:
            return conn.execute("SELECT obligation_id,state,content,last_error,attempts FROM delivery_obligations ORDER BY obligation_id").fetchall()

    def recovery(self):
        class Startup(self.Startup): pass
        startup = Startup()
        startup.adapters = {self.Platform.TELEGRAM: self.adapter}
        startup._authorization_adapter = lambda *_args: self.adapter
        self.cleared = []
        async def clear(rows): self.cleared.extend(rows)
        startup._clear_resume_pending_for_claimed_obligations = clear
        return asyncio.run(startup._redeliver_pending_obligations())

    def test_real_final_replay_returns_durable_ack_without_sending_again(self):
        first = asyncio.run(self.final())
        second = asyncio.run(self.final())
        self.assertTrue(first.success and second.success)
        self.assertEqual(len(self.sends), 1)
        self.assertEqual(second.message_id, "local-ack")
        self.assertEqual(second.continuation_message_ids, ("local-first",))
        self.assertTrue(second.raw_response["brokerrouter_already_delivered"])
        row = self.rows()[0]
        self.assertEqual(row[1], "br_delivered")
        self.assertEqual(row[4], 1)
        self.assertNotIn(self.body, row[2])
        self.assertNotIn(os.environ["BROKERROUTER_VIRTUAL_KEY"], row[0])

    def test_native_steer_slash_payload_is_a_model_turn_not_a_control_reply(self):
        from gateway.run_inbound import GatewayInboundMixin
        self.event.text = "/steer /hello"
        self.event.get_command_args = lambda: "/hello"
        handled, response = GatewayInboundMixin._hm_send_payload_as_turn(self.event, "usage")
        self.assertFalse(handled)
        self.assertIsNone(response)
        self.assertEqual(self.event.text, "/hello")
        self.assertTrue(asyncio.run(self.final()).success)
        self.assertTrue(asyncio.run(self.final()).success)
        self.assertEqual(len(self.sends), 1)
        self.assertEqual(self.rows()[0][1], "br_delivered")

    def test_same_identity_body_route_metadata_and_reply_drift_cannot_create_new_send(self):
        self.assertTrue(asyncio.run(self.final()).success)
        self.assertFalse(asyncio.run(self.final(text="different bytes")).success)
        self.assertFalse(asyncio.run(self.final(metadata={"thread_id": "other"})).success)
        self.assertFalse(asyncio.run(self.final(reply_to="other-anchor")).success)
        self.event.source.chat_id = "other-chat"
        self.assertFalse(asyncio.run(self.final()).success)
        self.assertEqual(len(self.sends), 1)
        self.assertEqual(len(self.rows()), 1)

    def test_missing_turn_identity_and_oversized_text_are_explicitly_unsupported(self):
        self.event.metadata = {}
        result = asyncio.run(self.final())
        self.assertFalse(result.success)
        self.assertEqual(result.error, "brokerrouter_delivery_fact_unavailable")
        self.assertEqual(self.sends, [])
        self.event.metadata = {"brokerrouter_turn": {"turn_id": str(uuid4()), "run_id": "fixture-run"}}
        self.assertFalse(asyncio.run(self.final(text="x" * (512 * 1024))).success)
        self.assertEqual(self.sends, [])

    def test_concurrent_native_final_callers_claim_one_send(self):
        async def run():
            opened, finish = asyncio.Event(), asyncio.Event()
            async def sender(*_args):
                opened.set()
                await finish.wait()
                return self.SendResult(success=True, message_id="one")
            self.sender = sender
            first = asyncio.create_task(self.final())
            await opened.wait()
            second = await self.final()
            self.assertFalse(second.success)
            finish.set()
            self.assertTrue((await first).success)
        asyncio.run(run())
        self.assertEqual(len(self.sends), 1)
        self.assertEqual(self.rows()[0][1], "br_delivered")

    def test_declined_retryable_and_partial_sends_are_unknown_without_inline_retry(self):
        for result in (
            self.SendResult(success=False, error="send_path_degraded", retryable=True),
            self.SendResult(success=False, error="flood_control:5", retry_after=5),
            self.SendResult(success=True, raw_response={"partial_overflow": True}),
        ):
            with self.subTest(result_type=result.error):
                self.event.message_id = str(uuid4())
                async def sender(*_args): return result
                self.sender = sender
                before = len(self.sends)
                self.assertFalse(asyncio.run(self.final()).success)
                self.assertFalse(asyncio.run(self.final()).success)
                self.assertEqual(len(self.sends), before + 1)
        self.assertTrue(all(row[1] == "br_unknown" for row in self.rows()))
        self.assertEqual(self.ledger.sweep_failed_for_runtime("telegram"), [])
        self.assertEqual(self.ledger.pending_flood_retries(), [])

    def test_sender_exception_contains_no_raw_body_and_cannot_be_retried(self):
        async def sender(*_args): raise RuntimeError(self.body)
        self.sender = sender
        first = asyncio.run(self.final())
        second = asyncio.run(self.final())
        self.assertFalse(first.success or second.success)
        self.assertNotIn(self.body, first.error)
        self.assertEqual(len(self.sends), 1)
        self.assertEqual(self.rows()[0][1], "br_unknown")

    def test_cancelled_sender_leaves_unknown_and_no_repeat(self):
        async def run():
            opened = asyncio.Event()
            async def sender(*_args):
                opened.set()
                await asyncio.Event().wait()
            self.sender = sender
            task = asyncio.create_task(self.final())
            await opened.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError): await task
            self.assertFalse((await self.final()).success)
        asyncio.run(run())
        self.assertEqual(len(self.sends), 1)
        self.assertEqual(self.rows()[0][1], "br_unknown")

    def test_durable_record_failure_prevents_the_actual_platform_send(self):
        with patch.object(self.publication, "_claim", side_effect=sqlite3.OperationalError("fixture-disk-failure")):
            result = asyncio.run(self.final())
        self.assertFalse(result.success)
        self.assertEqual(self.sends, [])

    def test_ack_commit_loss_does_not_invent_success_or_send_again(self):
        with patch.object(self.publication, "_finish", side_effect=sqlite3.OperationalError("fixture-ack-loss")):
            first = asyncio.run(self.final())
        second = asyncio.run(self.final())
        self.assertFalse(first.success or second.success)
        self.assertEqual(len(self.sends), 1)
        self.assertEqual(self.rows()[0][1], "br_attempting")

    def test_sql_identity_ack_replace_delete_and_completed_reset_are_rejected(self):
        self.assertTrue(asyncio.run(self.final()).success)
        oid = self.rows()[0][0]
        for sql in (
            "UPDATE delivery_obligations SET content='changed' WHERE obligation_id=?",
            "UPDATE delivery_obligations SET state='br_attempting' WHERE obligation_id=?",
            "UPDATE delivery_obligations SET owner_pid=0 WHERE obligation_id=?",
            "UPDATE delivery_obligations SET last_error='changed' WHERE obligation_id=?",
            "DELETE FROM delivery_obligations WHERE obligation_id=?",
        ):
            with self.subTest(sql=sql), self.assertRaises(sqlite3.IntegrityError), self.ledger._transaction() as conn:
                conn.execute(sql, (oid,))
        with self.assertRaises(sqlite3.IntegrityError):
            self.ledger.record_obligation(obligation_id=oid, session_key="fixed-session", platform="telegram",
                chat_id="fixed-chat", thread_id=None, content=self.body)
        self.assertEqual(self.rows()[0][1], "br_delivered")
        self.assertTrue(asyncio.run(self.final()).success)
        self.assertEqual(len(self.sends), 1)

    def test_pruning_cannot_erase_a_replayable_delivery_fact(self):
        self.assertTrue(asyncio.run(self.final()).success)
        with patch.object(self.ledger, "_MAX_ROWS", 0): self.ledger._prune(now=10**15)
        self.assertEqual(len(self.rows()), 1)
        self.assertTrue(asyncio.run(self.final()).success)
        self.assertEqual(len(self.sends), 1)

    def test_actual_restart_holds_legacy_pending_and_unknown_without_clearing_newer_resume(self):
        for state in ("pending", "attempting", "failed"):
            oid = "legacy-" + state
            self.ledger.record_obligation(obligation_id=oid, session_key="fixed-session", platform="telegram",
                chat_id="fixed-chat", thread_id=None, content="PRIVATE_LEGACY_OUTPUT_FIXTURE")
            with self.ledger._transaction() as conn:
                conn.execute("UPDATE delivery_obligations SET state=?,owner_pid=0 WHERE obligation_id=?", (state, oid))
        self.assertEqual(self.recovery(), 0)
        self.assertEqual(self.sends, [])
        self.assertEqual(self.cleared, [])
        self.assertTrue(all(row[1] == "br_legacy_held" for row in self.rows()))

    def test_dead_process_after_actual_send_before_ack_is_unknown_and_never_blindly_replayed(self):
        event = json.dumps({"turn_id": self.event.metadata["brokerrouter_turn"]["turn_id"], "run_id": "fixture-run"})
        child = '''
import asyncio,json,os
from pathlib import Path
from types import SimpleNamespace
from gateway.platforms.base import BasePlatformAdapter,Platform,PlatformConfig,SendResult
class Transport(BasePlatformAdapter):
 name="zero-fee-crash-contract"
 async def connect(self): return True
 async def disconnect(self): pass
 async def get_chat_info(self,_chat): return {}
 async def send(self,chat_id,content,**kwargs):
  Path(os.environ["HERMES_HOME"],"external-send-fact").write_text("accepted-once")
  os._exit(66)
event=SimpleNamespace(text="fixture inbound",message_id="fixed-inbound",ledger_message_id=None,
 metadata={"brokerrouter_turn":json.loads(os.environ["NATIVE_FIXTURE_BINDING"])},
 source=SimpleNamespace(platform=Platform.TELEGRAM,chat_id="fixed-chat",thread_id=None))
adapter=Transport(PlatformConfig(),Platform.TELEGRAM)
from brokerrouter_hermes.publication import send_final_once
asyncio.run(send_final_once(adapter,event,"fixed-session","PRIVATE_RELEASED_TEXT_FIXTURE",{},reply_to=None,is_ephemeral_response=False))
'''
        process = subprocess.run([sys.executable, "-B", "-c", child], env=dict(os.environ, NATIVE_FIXTURE_BINDING=event),
                                 capture_output=True, text=True, timeout=20)
        self.assertEqual(process.returncode, 66, process.stderr)
        self.assertEqual((self.home / "external-send-fact").read_text(), "accepted-once")
        self.assertEqual(self.rows()[0][1], "br_attempting")
        self.assertEqual(self.recovery(), 0)
        self.assertEqual(self.rows()[0][1], "br_unknown")
        self.assertFalse(asyncio.run(self.final()).success)
        self.assertEqual(self.sends, [])
        self.assertEqual(self.cleared, [])

    def test_recovery_does_not_poison_a_live_sender(self):
        async def run():
            opened, finish = asyncio.Event(), asyncio.Event()
            async def sender(*_args):
                opened.set();await finish.wait()
                return self.SendResult(success=True, message_id="live")
            self.sender = sender
            task = asyncio.create_task(self.final())
            await opened.wait()
            self.assertEqual(await asyncio.to_thread(self.ledger.sweep_recoverable), [])
            finish.set()
            self.assertTrue((await task).success)
        asyncio.run(run())
        self.assertEqual(self.rows()[0][1], "br_delivered")

    def test_off_mode_keeps_ordinary_legacy_delivery_and_cannot_replay_governed_rows(self):
        self.assertTrue(asyncio.run(self.final()).success)
        with patch.dict(os.environ, {"BROKERROUTER_HERMES_GOVERNANCE": "off"}):
            adapter = self.Adapter(self.PlatformConfig(), self.Platform.TELEGRAM)
            self.assertEqual(self.ledger.sweep_recoverable(), [])
            result, _ = asyncio.run(adapter.send_final_ledgered(self.event, "off-session", "ordinary off body", {}, reply_to=None))
            self.assertTrue(result.success)
        self.assertEqual(len(self.sends), 2)
        self.assertTrue(any(row[1] == "delivered" for row in self.rows()))


if __name__ == "__main__": unittest.main()
