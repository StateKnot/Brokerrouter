from __future__ import annotations

import hashlib
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
from uuid import uuid4

from brokerrouter_hermes.store import CheckpointConflict, CheckpointStore, MAX_RESULT_BYTES


class FrozenResultTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        os.chmod(self.temp.name, 0o700)
        self.path = Path(self.temp.name) / "state.db"
        self.key = b"\x19" * 32
        self.store = CheckpointStore(self.path, self.key)
        self.operation_key = "hermes-v1-" + "a" * 64
        self.evidence = "hermes://brokerrouter/synthetic/tool-returned"
        self.dispatching()

    def tearDown(self):
        self.temp.cleanup()

    def dispatching(self, operation_key=None):
        operation_key = operation_key or self.operation_key
        self.store.stage(operation_key, "b" * 64, "synthetic_tool", "c" * 64)
        self.store.checked(operation_key, str(uuid4()), "ready", "d" * 64)
        self.store.prepare_claim(operation_key, "e" * 64)
        self.store.transition(operation_key, {"claim_prepared"}, "dispatch_authorized")
        self.store.transition(operation_key, {"dispatch_authorized"}, "dispatching")
        return operation_key

    def sql(self, statement, params=()):
        with sqlite3.connect(self.path) as connection:
            return connection.execute(statement, params).fetchall()

    def corrupt(self, statement, params=()):
        # Simulate offline file corruption, beyond the write protections of the live store.
        self.sql("DROP TRIGGER brokerrouter_tool_result_immutable")
        self.sql(statement, params)

    def test_exact_string_is_encrypted_and_execution_fact_commits_with_it(self):
        text = "synthetic-private-output-" + "中文🙂\0" * 20
        frozen = self.store.freeze_result(self.operation_key, text, self.evidence)
        self.assertEqual(frozen.text, text)
        self.assertEqual(frozen.sha256, hashlib.sha256(text.encode()).hexdigest())
        self.assertEqual(frozen.byte_length, len(text.encode()))
        self.assertEqual(frozen.state, "quarantined")
        self.assertNotIn(text, repr(frozen))
        self.assertEqual(self.store.load(self.operation_key).state, "effect_recorded")
        self.assertEqual(self.store.load(self.operation_key).evidence_ref, self.evidence)
        for file in self.path.parent.glob("state.db*"):
            self.assertNotIn(text.encode(), file.read_bytes())
        reopened = CheckpointStore(self.path, self.key)
        self.assertEqual(reopened.load_result(self.operation_key), frozen)
        reopened.transition(self.operation_key, {"effect_recorded"}, "completed")
        self.assertEqual(reopened.load_result(self.operation_key).text, text)

    def test_empty_result_is_valid_and_not_confused_with_missing_result(self):
        frozen = self.store.freeze_result(self.operation_key, "", self.evidence)
        self.assertEqual(frozen.text, "")
        self.assertEqual(frozen.byte_length, 0)
        self.assertEqual(frozen.sha256, hashlib.sha256(b"").hexdigest())

    def test_non_string_invalid_utf8_and_byte_overflow_leave_dispatch_unknown(self):
        class PretendString(str):
            pass

        for result in (None, 7, {"text": "value"}, b"bytes", PretendString("value"), "\ud800", "中" * (MAX_RESULT_BYTES // 3 + 1)):
            with self.subTest(type=type(result)):
                with self.assertRaises(CheckpointConflict):
                    self.store.freeze_result(self.operation_key, result, self.evidence)
                self.assertIsNone(self.store.load_result(self.operation_key))
                self.assertEqual(self.store.load(self.operation_key).state, "dispatching")
        self.store.transition(self.operation_key, {"dispatching"}, "execution_unknown")
        with self.assertRaises(CheckpointConflict):
            self.store.freeze_result(self.operation_key, "later", self.evidence)

    def test_invalid_evidence_and_retention_do_not_record_a_result(self):
        for evidence, ttl in (("", 3600), ("line\nbreak", 3600), ("\ud800", 3600), ("x" * 1001, 3600), (self.evidence, 59), (self.evidence, 86401), (self.evidence, True)):
            with self.subTest(ttl=ttl):
                with self.assertRaises(CheckpointConflict):
                    self.store.freeze_result(self.operation_key, "value", evidence, ttl_seconds=ttl)
        self.assertIsNone(self.store.load_result(self.operation_key))

    def test_same_output_retry_is_idempotent_but_content_or_evidence_changes_conflict(self):
        first = self.store.freeze_result(self.operation_key, "first", self.evidence)
        self.assertEqual(self.store.freeze_result(self.operation_key, "first", self.evidence), first)
        for content, evidence in (("other", self.evidence), ("first", self.evidence + "/changed")):
            with self.assertRaises(CheckpointConflict):
                self.store.freeze_result(self.operation_key, content, evidence)
        self.assertEqual(self.store.load_result(self.operation_key), first)
        self.assertEqual(self.sql("SELECT count(*) FROM brokerrouter_tool_results")[0][0], 1)

    def test_concurrent_identical_freezes_record_one_effect(self):
        barrier = threading.Barrier(8)
        results = []
        failures = []

        def freeze():
            try:
                barrier.wait(timeout=5)
                results.append(self.store.freeze_result(self.operation_key, "same", self.evidence))
            except BaseException as exc:
                failures.append(exc)

        threads = [threading.Thread(target=freeze) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=10)
        self.assertEqual(failures, [])
        self.assertEqual(len(results), 8)
        self.assertTrue(all(result == results[0] for result in results))
        self.assertEqual(self.sql("SELECT count(*) FROM brokerrouter_tool_results")[0][0], 1)
        self.assertEqual(self.store.load(self.operation_key).state, "effect_recorded")

    def test_competing_result_bytes_cannot_overwrite_the_first_execution_fact(self):
        barrier = threading.Barrier(2)
        committed = []
        conflicted = []

        def freeze(content):
            try:
                barrier.wait(timeout=5)
                committed.append(self.store.freeze_result(self.operation_key, content, self.evidence))
            except CheckpointConflict:
                conflicted.append(content)

        threads = [threading.Thread(target=freeze, args=(content,)) for content in ("first", "second")]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=10)
        self.assertEqual(len(committed), 1)
        self.assertEqual(len(conflicted), 1)
        self.assertEqual(self.store.load_result(self.operation_key), committed[0])
        self.assertNotEqual(committed[0].text, conflicted[0])

    def test_commit_observation_loss_recovers_frozen_bytes_without_new_dispatch(self):
        with patch.object(self.store, "load_result", side_effect=sqlite3.OperationalError("lost observation")):
            with self.assertRaises(sqlite3.OperationalError):
                self.store.freeze_result(self.operation_key, "committed", self.evidence)
        recovered = CheckpointStore(self.path, self.key)
        self.assertEqual(recovered.load(self.operation_key).state, "effect_recorded")
        self.assertEqual(recovered.load_result(self.operation_key).text, "committed")
        self.assertEqual(recovered.stage(self.operation_key, "b" * 64, "synthetic_tool", "c" * 64).state, "effect_recorded")
        with self.assertRaises((CheckpointConflict, sqlite3.IntegrityError)):
            recovered.transition(self.operation_key, {"effect_recorded"}, "ready")

    def test_process_killed_after_commit_before_observation_recovers_exact_result(self):
        script = """
import os, signal, sys
from pathlib import Path
from unittest.mock import patch
from brokerrouter_hermes.store import CheckpointStore
store = CheckpointStore(Path(sys.argv[1]), b'\\x19' * 32)
def terminate(_operation):
    os.kill(os.getpid(), signal.SIGKILL)
with patch.object(store, 'load_result', side_effect=terminate):
    store.freeze_result(sys.argv[2], 'committed-before-kill', sys.argv[3])
"""
        child = subprocess.run(
            [sys.executable, "-c", script, str(self.path), self.operation_key, self.evidence],
            capture_output=True,
            timeout=30,
        )
        self.assertEqual(child.returncode, -9, child.stderr.decode())
        recovered = CheckpointStore(self.path, self.key)
        self.assertEqual(recovered.load_result(self.operation_key).text, "committed-before-kill")
        self.assertEqual(recovered.load(self.operation_key).state, "effect_recorded")
        with self.assertRaises((CheckpointConflict, sqlite3.IntegrityError)):
            recovered.transition(self.operation_key, {"effect_recorded"}, "dispatching")

    def test_transaction_failure_rolls_back_both_bytes_and_execution_fact(self):
        self.sql("""CREATE TRIGGER fail_result_commit BEFORE UPDATE ON brokerrouter_checkpoints
            WHEN NEW.state='effect_recorded' BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END""")
        with self.assertRaises(sqlite3.IntegrityError):
            self.store.freeze_result(self.operation_key, "uncommitted", self.evidence)
        self.assertIsNone(self.store.load_result(self.operation_key))
        self.assertEqual(self.store.load(self.operation_key).state, "dispatching")
        self.store.transition(self.operation_key, {"dispatching"}, "execution_unknown")
        with self.assertRaises((CheckpointConflict, sqlite3.IntegrityError)):
            self.store.transition(self.operation_key, {"execution_unknown"}, "ready")

    def test_wrong_key_and_modified_ciphertext_fail_closed(self):
        self.store.freeze_result(self.operation_key, "authentic", self.evidence)
        wrong = CheckpointStore(self.path, b"\x20" * 32)
        with self.assertRaises(CheckpointConflict):
            wrong.load_result(self.operation_key)
        envelope = self.sql("SELECT result_ciphertext FROM brokerrouter_tool_results")[0][0]
        modified = envelope[:-1] + bytes([envelope[-1] ^ 1])
        self.corrupt("UPDATE brokerrouter_tool_results SET result_ciphertext=?", (modified,))
        with self.assertRaises(CheckpointConflict):
            self.store.load_result(self.operation_key)

    def test_short_aes_key_is_rejected_before_opening_checkpoint_files(self):
        path = self.path.parent / "invalid-key.db"
        with self.assertRaises(CheckpointConflict):
            CheckpointStore(path, b"\x19" * 16)
        self.assertFalse(path.exists())

    def test_claim_and_another_calls_ciphertext_cannot_be_reused_as_result(self):
        self.store.freeze_result(self.operation_key, "first", self.evidence)
        other = self.dispatching("hermes-v1-" + "f" * 64)
        self.store.freeze_result(other, "second", self.evidence)
        envelope = self.sql("SELECT result_ciphertext FROM brokerrouter_tool_results WHERE operation_key=?", (other,))[0][0]
        self.corrupt("UPDATE brokerrouter_tool_results SET result_ciphertext=? WHERE operation_key=?", (envelope, self.operation_key))
        with self.assertRaises(CheckpointConflict):
            self.store.load_result(self.operation_key)
        claim = self.sql("SELECT claim_ciphertext FROM brokerrouter_checkpoints WHERE operation_key=?", (other,))[0][0]
        # Add a syntactically valid result envelope version to demonstrate purpose separation.
        self.sql("UPDATE brokerrouter_tool_results SET result_ciphertext=? WHERE operation_key=?", (b"\x01" + claim, self.operation_key))
        with self.assertRaises(CheckpointConflict):
            self.store.load_result(self.operation_key)

    def test_result_digest_identity_and_expiry_are_authenticated(self):
        self.store.freeze_result(self.operation_key, "authentic", self.evidence)
        self.corrupt("UPDATE brokerrouter_tool_results SET expires_at_ns=expires_at_ns+1000")
        with self.assertRaises(CheckpointConflict):
            self.store.load_result(self.operation_key)

    def test_output_observation_is_separate_from_completed_execution_and_can_require_new_approval(self):
        self.store.freeze_result(self.operation_key, "held", self.evidence)
        self.store.transition(self.operation_key, {"effect_recorded"}, "completed")
        output_id = str(uuid4())
        ready = self.store.output_checked(self.operation_key, output_id, "ready", "1" * 64)
        self.assertEqual(ready.state, "ready")
        held = self.store.output_checked(self.operation_key, output_id, "waiting_approval", None)
        self.assertEqual(held.state, "waiting_approval")
        self.assertEqual(held.output_payload_hash, "1" * 64)
        self.assertEqual(self.store.load(self.operation_key).state, "completed")
        for operation, digest in ((str(uuid4()), "1" * 64), (output_id, "2" * 64)):
            with self.assertRaises(CheckpointConflict):
                self.store.output_checked(self.operation_key, operation, "ready", digest)

    def test_rejection_and_expiry_destroy_bytes_preserving_effect_and_cannot_reopen(self):
        self.store.freeze_result(self.operation_key, "held", self.evidence)
        rejected = self.store.output_checked(self.operation_key, str(uuid4()), "rejected", None)
        self.assertIsNone(rejected.text)
        self.assertEqual(rejected.state, "rejected")
        with self.assertRaises(CheckpointConflict):
            self.store.output_checked(self.operation_key, str(uuid4()), "ready", "1" * 64)
        with self.assertRaises(CheckpointConflict):
            self.store.freeze_result(self.operation_key, "held", self.evidence)
        other = self.dispatching("hermes-v1-" + "0" * 64)
        frozen = self.store.freeze_result(other, "expired", self.evidence, ttl_seconds=60)
        with patch("brokerrouter_hermes.store.time.time_ns", return_value=frozen.expires_at_ns):
            expired = self.store.load_result(other)
        self.assertEqual(expired.state, "unavailable")
        self.assertIsNone(expired.text)
        self.assertEqual(expired.sha256, frozen.sha256)
        self.assertEqual(self.store.load(other).state, "effect_recorded")
        self.assertIsNone(self.sql("SELECT result_ciphertext FROM brokerrouter_tool_results WHERE operation_key=?", (other,))[0][0])

    def test_retry_does_not_extend_retention_or_reset_output_state(self):
        first = self.store.freeze_result(self.operation_key, "held", self.evidence, ttl_seconds=60)
        output_id = str(uuid4())
        self.store.output_checked(self.operation_key, output_id, "waiting_approval", None)
        retry = self.store.freeze_result(self.operation_key, "held", self.evidence, ttl_seconds=86400)
        self.assertEqual(retry.expires_at_ns, first.expires_at_ns)
        self.assertEqual(retry.state, "waiting_approval")
        self.assertEqual(retry.output_operation_id, output_id)

    def test_maintenance_is_bounded_and_removal_retains_immutable_evidence(self):
        first = self.store.freeze_result(self.operation_key, "first", self.evidence, ttl_seconds=60)
        other = self.dispatching("hermes-v1-" + "0" * 64)
        second = self.store.freeze_result(other, "second", self.evidence, ttl_seconds=60)
        with patch("brokerrouter_hermes.store.time.time_ns", return_value=max(first.expires_at_ns, second.expires_at_ns)):
            self.assertEqual(self.store.expire_results(limit=1), 1)
        removed = self.store.mark_result_unavailable(other)
        self.assertEqual(removed.sha256, second.sha256)
        self.assertEqual(removed.state, "unavailable")
        with self.assertRaises(sqlite3.IntegrityError):
            self.sql("DELETE FROM brokerrouter_tool_results WHERE operation_key=?", (other,))
        with self.assertRaises(sqlite3.IntegrityError):
            self.sql("UPDATE brokerrouter_tool_results SET sha256=? WHERE operation_key=?", ("a" * 64, other))

    def test_remote_result_binding_is_stable_and_checks_exact_execution_context(self):
        frozen = self.store.freeze_result(self.operation_key, "held", self.evidence)
        input_id = self.store.load(self.operation_key).operation_id
        result_id = str(uuid4())
        bound = self.store.bind_remote_result(
            self.operation_key, result_id, input_id, frozen.sha256, frozen.byte_length
        )
        self.assertEqual(bound.server_result_id, result_id)
        reopened = CheckpointStore(self.path, self.key)
        self.assertEqual(reopened.load_result(self.operation_key).server_result_id, result_id)
        for remote_id, operation, digest, length in (
            (str(uuid4()), input_id, frozen.sha256, frozen.byte_length),
            (result_id, str(uuid4()), frozen.sha256, frozen.byte_length),
            (result_id, input_id, "0" * 64, frozen.byte_length),
            (result_id, input_id, frozen.sha256, True),
        ):
            with self.assertRaises(CheckpointConflict):
                reopened.bind_remote_result(self.operation_key, remote_id, operation, digest, length)
        with self.assertRaises(sqlite3.IntegrityError):
            self.sql("UPDATE brokerrouter_tool_results SET server_result_id=NULL WHERE operation_key=?", (self.operation_key,))
        reopened.mark_result_unavailable(self.operation_key)
        with self.assertRaises(CheckpointConflict):
            reopened.bind_remote_result(self.operation_key, result_id, input_id, frozen.sha256, frozen.byte_length)

    def test_v3_upgrade_adds_remote_identity_without_rewriting_frozen_bytes(self):
        original = self.store.freeze_result(self.operation_key, "held", self.evidence)
        ciphertext = self.sql("SELECT result_ciphertext FROM brokerrouter_tool_results")[0][0]
        self.sql("DROP TRIGGER brokerrouter_remote_result_immutable")
        self.sql("ALTER TABLE brokerrouter_tool_results DROP COLUMN server_result_id")
        upgraded = CheckpointStore(self.path, self.key)
        self.assertEqual(upgraded.load_result(self.operation_key), original)
        self.assertEqual(self.sql("SELECT result_ciphertext FROM brokerrouter_tool_results")[0][0], ciphertext)

    def test_v2_upgrade_keeps_old_execution_fact_and_does_not_invent_lost_result(self):
        self.store.transition(self.operation_key, {"dispatching"}, "effect_recorded", self.evidence)
        self.store.transition(self.operation_key, {"effect_recorded"}, "completed")
        self.sql("DROP TABLE brokerrouter_tool_results")
        self.sql("DROP TRIGGER brokerrouter_checkpoint_no_redispatch")
        self.sql("PRAGMA user_version=2")
        upgraded = CheckpointStore(self.path, self.key)
        self.assertEqual(self.sql("PRAGMA user_version")[0][0], 3)
        self.assertEqual(upgraded.load(self.operation_key).state, "completed")
        self.assertEqual(upgraded.load(self.operation_key).claim_token, "e" * 64)
        self.assertIsNone(upgraded.load_result(self.operation_key))
        with self.assertRaises(CheckpointConflict):
            upgraded.freeze_result(self.operation_key, "invented", self.evidence)


if __name__ == "__main__":
    unittest.main()
