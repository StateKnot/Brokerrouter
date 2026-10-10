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
import threading
import time
import unittest
from unittest.mock import patch
from uuid import uuid4

from brokerrouter_hermes.artifacts import ArtifactBinding, ArtifactConflict, ArtifactSpool, ArtifactUnavailable, CHUNK_BYTES, MAX_BUNDLE_BYTES, SealedProducerFile


class ArtifactSpoolTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        os.chmod(self.root, 0o700)
        self.path = self.root / "artifact-spool.db"
        self.key = b"\x41" * 32
        self.binding = ArtifactBinding(
            operation_key="hermes-v1-" + "1" * 64,
            input_operation_id=str(uuid4()),
            identity_hash="2" * 64,
            tool_binding_hash="b" * 64,
            model_binding_hash="3" * 64,
            input_payload_hash="c" * 64,
            original_payload_hash="4" * 64,
            final_payload_hash="5" * 64,
            claim_hash="6" * 64,
            evidence_hash="7" * 64,
        )
        self.spool = ArtifactSpool(self.path, self.key)

    def tearDown(self):
        self.temp.cleanup()

    def producer(self, data, name="producer.mp4"):
        # The fixture is a broker-owned regular descriptor. It tests byte
        # freezing; neither these candidate bytes nor permissions certify MP4
        # syntax or an actual isolated producer workload.
        path = self.root / name
        path.write_bytes(data)
        return SealedProducerFile(os.open(path, os.O_RDONLY), "a" * 64, "video/mp4")

    def test_freeze_recovers_exact_encrypted_bytes_and_immutable_source(self):
        data = b"\x00\x00\x00\x18ftypmp42" + b"synthetic-private-candidate" * 300
        producer = self.producer(data)
        try:
            bundle = self.spool.freeze_once(self.binding, (producer,))
        finally:
            os.close(producer.fd)
        self.assertEqual(bundle.artifacts[0].sha256, hashlib.sha256(data).hexdigest())
        self.assertEqual(bundle.artifacts[0].byte_length, len(data))
        self.assertEqual(bundle.total_bytes, len(data))
        (self.root / "producer.mp4").unlink()
        reopened = ArtifactSpool(self.path, self.key)
        self.assertEqual(reopened.recover(self.binding), bundle)
        self.assertEqual(reopened.read_private_chunk_for_upload(self.binding, 0, 0), data)
        for path in self.root.glob("artifact-spool.db*"):
            self.assertNotIn(b"synthetic-private-candidate", path.read_bytes())
        self.assertNotIn("producer.mp4", repr(bundle))

    def test_retry_has_identical_manifest_but_changed_real_bytes_or_source_conflict(self):
        producer = self.producer(b"first immutable candidate")
        try:
            first = self.spool.freeze_once(self.binding, (producer,))
            self.assertEqual(self.spool.freeze_once(self.binding, (producer,)), first)
            for name in ("input_operation_id", "identity_hash", "tool_binding_hash", "model_binding_hash", "original_payload_hash", "final_payload_hash", "input_payload_hash", "claim_hash", "evidence_hash"):
                value = str(uuid4()) if name == "input_operation_id" else "d" * 64
                with self.subTest(source=name), self.assertRaises(ArtifactConflict):
                    self.spool.recover(replace(self.binding, **{name:value}))
            with self.assertRaises(ArtifactConflict):
                self.spool.freeze_once(self.binding, (producer,), ttl_seconds=7200)
            (self.root / "producer.mp4").write_bytes(b"changed real candidate")
            with self.assertRaises(ArtifactConflict):
                self.spool.freeze_once(self.binding, (producer,))
            self.assertEqual(self.spool.recover(self.binding), first)
            self.assertEqual(self.spool.read_private_chunk_for_upload(self.binding, 0, 0), b"first immutable candidate")
        finally:
            os.close(producer.fd)

    def test_two_instances_capture_once_and_cannot_replace_each_others_bytes(self):
        first = self.producer(b"identical frozen bytes", "a.mp4")
        second = self.producer(b"identical frozen bytes", "b.mp4")
        other = ArtifactSpool(self.path, self.key)
        start = threading.Barrier(2)
        results = []
        failures = []

        def capture(spool, producer):
            try:
                start.wait(5)
                results.append(spool.freeze_once(self.binding, (producer,)))
            except BaseException as error:
                failures.append(error)

        threads = [threading.Thread(target=capture, args=args) for args in ((self.spool, first), (other, second))]
        try:
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(10)
            self.assertFalse(any(thread.is_alive() for thread in threads))
            self.assertEqual(failures, [])
            self.assertEqual(len(results), 2)
            self.assertEqual(results[0], results[1])
            (self.root / "b.mp4").write_bytes(b"a later changed candidate")
            with self.assertRaises(ArtifactConflict):
                other.freeze_once(self.binding, (second,))
            self.assertEqual(other.read_private_chunk_for_upload(self.binding, 0, 0), b"identical frozen bytes")
        finally:
            os.close(first.fd)
            os.close(second.fd)

    def test_unsupported_or_shared_writable_unbounded_fds_never_freeze_a_candidate(self):
        producer = self.producer(b"candidate")
        writable = os.open(self.root / "producer.mp4", os.O_RDWR)
        directory = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY)
        try:
            for candidates in ({"fd":producer.fd}, [producer], (), (replace(producer, media_type="audio/mpeg"),), (replace(producer, fd=writable),), (replace(producer, fd=directory),), (producer,) * 9):
                with self.subTest(type=type(candidates)), self.assertRaises(ArtifactConflict):
                    self.spool.freeze_once(self.binding, candidates)
            os.link(self.root / "producer.mp4", self.root / "shared.mp4")
            with self.assertRaises(ArtifactConflict):
                self.spool.freeze_once(self.binding, (producer,))
            (self.root / "shared.mp4").unlink()
            with (self.root / "producer.mp4").open("wb") as file:
                file.truncate(MAX_BUNDLE_BYTES + 1)
            with self.assertRaises(ArtifactConflict):
                self.spool.freeze_once(self.binding, (producer,))
            with self.assertRaises(ArtifactUnavailable):
                self.spool.recover(self.binding)
        finally:
            for fd in (producer.fd, writable, directory):
                os.close(fd)

    def test_real_file_mutation_during_read_prevents_atomic_freeze(self):
        producer = self.producer(b"original bytes")
        pread = os.pread

        def read_then_mutate(fd, size, offset):
            part = pread(fd, size, offset)
            if offset == 0:
                with (self.root / "producer.mp4").open("ab") as writer:
                    writer.write(b"changed")
            return part

        try:
            with patch("os.pread", side_effect=read_then_mutate), self.assertRaises(ArtifactConflict):
                self.spool.freeze_once(self.binding, (producer,))
            with self.assertRaises(ArtifactUnavailable):
                self.spool.recover(self.binding)
        finally:
            os.close(producer.fd)

    def test_wrong_key_and_swapped_ordinal_ciphertext_never_return_candidate_bytes(self):
        producer = self.producer(b"A" * CHUNK_BYTES + b"B" * CHUNK_BYTES)
        try:
            self.spool.freeze_once(self.binding, (producer,))
        finally:
            os.close(producer.fd)
        with self.assertRaises(ArtifactUnavailable):
            ArtifactSpool(self.path, b"\x42" * 32).recover(self.binding)
        # Offline corruption fixture deliberately bypasses live immutable SQL
        # protections. Only the public recovery result is asserted.
        with sqlite3.connect(self.path) as db:
            rows = db.execute("SELECT chunk_ordinal,ciphertext FROM artifact_chunks ORDER BY chunk_ordinal").fetchall()
            db.execute("DROP TRIGGER artifact_chunk_immutable")
            for ordinal, ciphertext in rows:
                db.execute("UPDATE artifact_chunks SET ciphertext=? WHERE chunk_ordinal=?", (ciphertext, 1-ordinal))
        with self.assertRaises(ArtifactUnavailable):
            self.spool.recover(self.binding)
        with self.assertRaises(ArtifactUnavailable):
            self.spool.read_private_chunk_for_upload(self.binding, 0, 0)

    def test_expiry_clears_only_ciphertext_and_does_not_extend_on_replay(self):
        producer = self.producer(b"expires without new execution")
        try:
            frozen = self.spool.freeze_once(self.binding, (producer,), ttl_seconds=60)
            with patch("brokerrouter_hermes.artifacts.time.time_ns", return_value=frozen.expires_at_ns+1):
                self.assertEqual(self.spool.expire(), 1)
                self.assertEqual(self.spool.expire(), 0)
                with self.assertRaises(ArtifactUnavailable):
                    self.spool.recover(self.binding)
                with self.assertRaises(ArtifactUnavailable):
                    self.spool.freeze_once(self.binding, (producer,), ttl_seconds=60)
            with self.assertRaises(ArtifactUnavailable):
                self.spool.recover(self.binding)
            with sqlite3.connect(self.path) as db:
                db.create_function("br_artifact_now_ns", 0, time.time_ns)
                self.assertEqual(db.execute("SELECT manifest_hash,expires_at_ns FROM artifact_bundles").fetchone(), (frozen.manifest_sha256, frozen.expires_at_ns))
                for sql in ("DELETE FROM artifact_bundles", "UPDATE artifact_bundles SET expires_at_ns=expires_at_ns+1", "DELETE FROM artifact_chunks", "UPDATE artifact_chunks SET ciphertext=x'00'"):
                    with self.assertRaises(sqlite3.IntegrityError):
                        db.execute(sql)
        finally:
            os.close(producer.fd)

    def test_private_spool_capacity_is_not_silently_truncated(self):
        small = ArtifactSpool(self.root / "small.db", self.key, maximum_spool_bytes=32)
        producer = self.producer(b"more than four bytes")
        try:
            with self.assertRaises(ArtifactConflict):
                small.freeze_once(self.binding, (producer,))
            with self.assertRaises(ArtifactUnavailable):
                small.recover(self.binding)
        finally:
            os.close(producer.fd)

    def test_ciphertext_from_another_execution_or_purpose_is_never_accepted(self):
        producer = self.producer(b"same candidate bytes")
        other_binding = replace(self.binding, operation_key="hermes-v1-" + "8" * 64,
                                claim_hash="9" * 64, input_operation_id=str(uuid4()))
        try:
            self.spool.freeze_once(self.binding, (producer,))
            self.spool.freeze_once(other_binding, (producer,))
        finally:
            os.close(producer.fd)
        with sqlite3.connect(self.path) as db:
            source_cipher = db.execute("SELECT ciphertext FROM artifact_chunks WHERE operation_key=?", (self.binding.operation_key,)).fetchone()[0]
            db.execute("DROP TRIGGER artifact_chunk_immutable")
            db.execute("UPDATE artifact_chunks SET ciphertext=? WHERE operation_key=?", (source_cipher, other_binding.operation_key))
        with self.assertRaises(ArtifactUnavailable):
            self.spool.read_private_chunk_for_upload(other_binding, 0, 0)
        self.assertEqual(self.spool.read_private_chunk_for_upload(self.binding, 0, 0), b"same candidate bytes")
        with sqlite3.connect(self.path) as db:
            seal = db.execute("SELECT seal FROM artifact_bundles WHERE operation_key=?", (self.binding.operation_key,)).fetchone()[0]
            db.execute("UPDATE artifact_chunks SET ciphertext=? WHERE operation_key=?", (seal, self.binding.operation_key))
        with self.assertRaises(ArtifactUnavailable):
            self.spool.read_private_chunk_for_upload(self.binding, 0, 0)

    def test_authenticated_expiry_and_manifest_cannot_be_rewritten_offline(self):
        for mutation in ("expiry", "manifest"):
            with self.subTest(mutation=mutation):
                spool = ArtifactSpool(self.root / f"{mutation}.db", self.key)
                producer = self.producer(b"metadata authenticated bytes")
                try:
                    spool.freeze_once(self.binding, (producer,))
                finally:
                    os.close(producer.fd)
                with sqlite3.connect(spool.path) as db:
                    db.execute("DROP TRIGGER artifact_bundle_immutable")
                    if mutation == "expiry":
                        db.execute("UPDATE artifact_bundles SET expires_at_ns=expires_at_ns+60000000000,ttl_seconds=ttl_seconds+60")
                    else:
                        manifest = json.loads(db.execute("SELECT manifest_json FROM artifact_bundles").fetchone()[0])
                        manifest["artifacts"][0]["producer_revision"] = "f" * 64
                        encoded = json.dumps(manifest, ensure_ascii=True, sort_keys=True, separators=(",", ":"))
                        db.execute("UPDATE artifact_bundles SET manifest_json=?,manifest_hash=?", (encoded, hashlib.sha256(encoded.encode()).hexdigest()))
                with self.assertRaises(ArtifactUnavailable):
                    spool.recover(self.binding)

    def test_invalid_types_bindings_and_ordinals_cannot_access_existing_bytes(self):
        producer = self.producer(b"private chunk")
        try:
            self.spool.freeze_once(self.binding, (producer,))
        finally:
            os.close(producer.fd)
        for source in (asdict(self.binding), replace(self.binding, claim_hash="X"*64), replace(self.binding, input_operation_id="not-a-uuid"), replace(self.binding, operation_key="line\nbreak")):
            with self.subTest(source=type(source)), self.assertRaises(ArtifactConflict):
                self.spool.recover(source)
        for artifact, chunk in ((-1,0),(1,0),(0,-1),(0,1),(True,0),(0,True)):
            with self.subTest(artifact=artifact,chunk=chunk), self.assertRaises(ArtifactConflict):
                self.spool.read_private_chunk_for_upload(self.binding, artifact, chunk)
        with self.assertRaises(ArtifactConflict):
            ArtifactSpool(Path("relative-spool.db"), self.key)

    def test_alias_path_substitution_leaves_previously_frozen_descriptor_bytes_intact(self):
        producer = self.producer(b"original frozen descriptor bytes")
        try:
            original = self.spool.freeze_once(self.binding, (producer,))
            replacement = self.root / "replacement.mp4"
            replacement.write_bytes(b"path substituted after freeze")
            os.replace(replacement, self.root / "producer.mp4")
            self.assertEqual(self.spool.recover(self.binding), original)
            self.assertEqual(self.spool.read_private_chunk_for_upload(self.binding, 0, 0), b"original frozen descriptor bytes")
        finally:
            os.close(producer.fd)

    def test_oversized_corrupt_record_is_unavailable_and_cannot_replace_the_seal(self):
        for column in ("ciphertext", "seal", "manifest_json"):
            with self.subTest(column=column):
                spool = ArtifactSpool(self.root / f"oversized-{column}.db", self.key)
                producer = self.producer(b"bounded private candidate")
                try:
                    spool.freeze_once(self.binding, (producer,))
                finally:
                    os.close(producer.fd)
                with sqlite3.connect(spool.path) as db:
                    if column == "ciphertext":
                        db.execute("DROP TRIGGER artifact_chunk_immutable")
                        db.execute("UPDATE artifact_chunks SET ciphertext=zeroblob(?)", (CHUNK_BYTES+29,))
                    else:
                        db.execute("DROP TRIGGER artifact_bundle_immutable")
                        if column == "seal":
                            db.execute("UPDATE artifact_bundles SET seal=zeroblob(?)", (CHUNK_BYTES+29,))
                        else:
                            db.execute("UPDATE artifact_bundles SET manifest_json=?", (" "*16385,))
                with self.assertRaises(ArtifactUnavailable):
                    spool.recover(self.binding)

    def killed_capture(self, boundary):
        origin = {
            "turn_id":str(uuid4()), "session_id":"synthetic-session",
            "task_id":"synthetic-task", "call_id":"synthetic-call",
            "model_id":"logical-model", "tool_name":"candidate_producer", "arguments":{},
        }
        script = r'''
from dataclasses import asdict
import hashlib, json, os, signal, sqlite3, sys
from pathlib import Path
from brokerrouter_hermes.artifacts import ArtifactBinding, ArtifactSpool, CHUNK_BYTES, SealedProducerFile
from brokerrouter_hermes.config import Config
from brokerrouter_hermes.gate import GovernanceGate, ToolContext
from test_output_gate import ResultServer
root, boundary, origin = Path(sys.argv[1]), sys.argv[2], ToolContext(**json.loads(sys.argv[3]))
config = Config(base_url="https://gateway.invalid", approvals_url="https://console.invalid", virtual_key="sk_br_"+"1"*64,
                state_db=root/"native-state.db", state_key=b"\x17"*32, request_timeout_seconds=1,
                approval_poll_seconds=0.25, approval_wait_seconds=1)
gate = GovernanceGate(config, transport=ResultServer(origin.turn_id))
gate.native_open(origin)
def dispatch():
    with (root/"tool-marker").open("xb") as marker:
        marker.write(b"1")
    (root/"killed-producer.mp4").write_bytes(b"K"*CHUNK_BYTES+b"tail-private-candidate")
    return "unreleased synthetic text"
gate.run_execution(origin, dispatch)
checkpoint = gate.store.load(origin.operation_key)
native = gate.store.register_native_origin(origin.operation_key, origin.identity_hash, origin.tool_name, origin.model_binding_hash, hashlib.sha256(b"{}").hexdigest())
binding = ArtifactBinding(operation_key=origin.operation_key, input_operation_id=checkpoint.operation_id,
    identity_hash=checkpoint.identity_hash, tool_binding_hash=hashlib.sha256(origin.tool_name.encode()).hexdigest(),
    model_binding_hash=checkpoint.model_binding_hash, original_payload_hash=native.original_payload_hash,
    final_payload_hash=native.final_payload_hash, input_payload_hash=checkpoint.gateway_payload_hash,
    claim_hash=hashlib.sha256(checkpoint.claim_token.encode()).hexdigest(), evidence_hash=hashlib.sha256(checkpoint.evidence_ref.encode()).hexdigest())
(root/"frozen-binding.json").write_text(json.dumps(asdict(binding)))
spool = ArtifactSpool(root/"artifact-spool.db", b"\x41"*32)
if boundary == "before_commit":
    with sqlite3.connect(spool.path) as db:
        db.execute("CREATE TRIGGER terminate_capture BEFORE INSERT ON artifact_chunks WHEN NEW.chunk_ordinal=1 BEGIN SELECT terminate_owned_test_process(); END")
    connect = sqlite3.connect
    def crash():
        (root/"kill-boundary").write_text("before_commit")
        os.kill(os.getpid(), signal.SIGKILL)
    def crashing_connection(*args, **kwargs):
        db = connect(*args, **kwargs)
        db.create_function("terminate_owned_test_process", 0, crash)
        return db
    sqlite3.connect = crashing_connection
fd = os.open(root/"killed-producer.mp4", os.O_RDONLY)
spool.freeze_once(binding, (SealedProducerFile(fd, "a"*64, "video/mp4"),))
(root/"kill-boundary").write_text("after_commit")
os.kill(os.getpid(), signal.SIGKILL)
'''
        result = subprocess.run([sys.executable, "-c", script, str(self.root), boundary, json.dumps(origin)], capture_output=True, timeout=30)
        self.assertEqual(result.returncode, -signal.SIGKILL, result.stderr.decode())
        self.assertEqual((self.root / "kill-boundary").read_text(), boundary)
        self.assertEqual((self.root / "tool-marker").read_bytes(), b"1")
        binding = ArtifactBinding(**json.loads((self.root / "frozen-binding.json").read_text()))
        from brokerrouter_hermes.config import Config
        from brokerrouter_hermes.gate import GovernanceGate, GovernanceHalt, ToolContext
        from test_output_gate import ResultServer
        context = ToolContext(**origin)
        config = Config(base_url="https://gateway.invalid", approvals_url="https://console.invalid", virtual_key="sk_br_"+"1"*64,
                        state_db=self.root/"native-state.db", state_key=b"\x17"*32, request_timeout_seconds=1,
                        approval_poll_seconds=0.25, approval_wait_seconds=1)
        server = ResultServer(context.turn_id)
        gate = GovernanceGate(config, transport=server)
        self.assertEqual(gate.store.load(context.operation_key).state, "completed")
        with self.assertRaises(GovernanceHalt) as stopped:
            gate.native_open(context)
        self.assertEqual(stopped.exception.code, "result_unavailable_after_restart")
        with self.assertRaises(GovernanceHalt):
            gate.run_execution(context, lambda: (self.root/"tool-marker").write_bytes(b"2"))
        self.assertEqual((self.root / "tool-marker").read_bytes(), b"1")
        self.assertEqual(server.requests, [])
        return binding

    def test_real_sigkill_before_commit_has_no_partial_bundle_and_never_reexecutes_tool(self):
        binding = self.killed_capture("before_commit")
        with self.assertRaises(ArtifactUnavailable):
            ArtifactSpool(self.path, self.key).recover(binding)

    def test_real_sigkill_after_commit_response_loss_recovers_exact_original_chunks(self):
        binding = self.killed_capture("after_commit")
        # Later path substitution is irrelevant to already authenticated bytes.
        (self.root / "killed-producer.mp4").write_bytes(b"replacement after process termination")
        restored = ArtifactSpool(self.path, self.key)
        bundle = restored.recover(binding)
        self.assertEqual(bundle.artifacts[0].byte_length, CHUNK_BYTES + len(b"tail-private-candidate"))
        self.assertEqual(restored.read_private_chunk_for_upload(binding, 0, 0), b"K" * CHUNK_BYTES)
        self.assertEqual(restored.read_private_chunk_for_upload(binding, 0, 1), b"tail-private-candidate")


if __name__ == "__main__":
    unittest.main()
