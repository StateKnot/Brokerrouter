"""Actual fixed Agent/SDK/session functions; only authority and sinks are local."""
from pathlib import Path
import unittest
import os

import test_model_transport as fixtures


@unittest.skipUnless(os.environ.get("BROKERROUTER_HERMES_NATIVE_CONTRACT") == "1", "requires complete fixed native final host")
class FinalPublicationBoundaryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        fixtures.ModelTransportTest.setUpClass()

    @classmethod
    def tearDownClass(cls):
        fixtures.ModelTransportTest.tearDownClass()

    def setUp(self):
        self.case = fixtures.ModelTransportTest()
        self.case.setUp()

    def tearDown(self):
        self.case.tearDown()

    def test_actual_native_stream_is_not_published_before_dedicated_final_release(self):
        agent = self.case.agent()
        agent._disable_streaming = False
        seen = []
        agent.stream_delta_callback = seen.append
        result = agent.run_conversation("local unpublished final", task_id="native-final-boundary")
        self.assertIn("local no-fee model fixture", result["final_response"])
        self.assertEqual(len(self.case.posts), 1)
        self.assertEqual(seen, [], "real model deltas reached an ordinary sink before 0031 release")

    def test_actual_session_db_does_not_store_unreleased_assistant_body(self):
        from hermes_state import SessionDB
        db = SessionDB(self.case.home / "ordinary-history.db")
        try:
            agent = self.case.agent()
            agent._session_db = db
            agent._disable_streaming = True
            result = agent.run_conversation("local unpublished history", task_id="native-final-boundary")
            self.assertIn("local no-fee model fixture", result["final_response"])
            rows = db.get_messages(agent.session_id)
            leaked = [row for row in rows if row["role"] == "assistant" and row.get("content")]
            self.assertEqual(leaked, [], "real ordinary SQLite already held the unreleased final")
        finally:
            db.close()

    def test_actual_session_only_resume_clear_has_no_required_owner_proof(self):
        from gateway.config import GatewayConfig, Platform
        from gateway.session import SessionSource, SessionStore
        store = SessionStore(self.case.home / "sessions", GatewayConfig())
        try:
            # Required env is enabled through the real Agent/bind fixture.
            self.case.agent()
            entry = store.get_or_create_session(SessionSource(platform=Platform.LOCAL, chat_id="fixed-final-chat"))
            self.assertTrue(store.mark_resume_pending(entry.session_key, "newer-run-held"))
            # A stale completed result knows only this routing key, not the
            # current true turn/session/message/run/generation owner.
            self.assertFalse(store.clear_resume_pending(entry.session_key))
            self.assertTrue(entry.resume_pending)
        finally:
            store.close_all_db_handles()


if __name__ == "__main__":
    unittest.main()

