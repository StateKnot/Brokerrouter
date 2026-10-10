"""Private, immutable candidate bytes; this module grants no publication authority.

Run this spool in a broker workload/mount unavailable to Agent tools. File modes
and a read-only FD do not prove workload isolation or valid MP4. The server's
independent scanner, review and current authorization remain mandatory.
"""
from __future__ import annotations

from contextlib import contextmanager
from dataclasses import asdict, dataclass
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import time
from typing import Iterator
from uuid import UUID, uuid5

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers.aead import AESGCM


CHUNK_BYTES = 4 * 1024 * 1024
MAX_BUNDLE_BYTES = 64 * 1024 * 1024
MAX_ARTIFACTS = 8
_APPLICATION_ID = int.from_bytes(b"BRAS", "big")
_NAMESPACE = UUID("9da98417-73c6-44c1-9662-8964286b00ca")
_FORMAT = "brokerrouter-hermes-mp4-spool-v1"


class ArtifactConflict(RuntimeError):
    pass


class ArtifactUnavailable(RuntimeError):
    pass


def _canonical(value) -> str:
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":"), allow_nan=False)


def _hash(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _digest(value) -> str:
    return _hash(_canonical(value).encode("ascii"))


def _sha(value) -> bool:
    return type(value) is str and len(value) == 64 and all(c in "0123456789abcdef" for c in value)


@dataclass(frozen=True)
class ArtifactBinding:
    operation_key: str
    input_operation_id: str
    identity_hash: str
    tool_binding_hash: str
    model_binding_hash: str
    input_payload_hash: str
    original_payload_hash: str
    final_payload_hash: str
    claim_hash: str
    evidence_hash: str

    def validate(self) -> None:
        try:
            valid = (
                type(self.operation_key) is str
                and 1 <= len(self.operation_key.encode("utf-8")) <= 200
                and all(ord(c) >= 32 and ord(c) != 127 for c in self.operation_key)
                and type(self.input_operation_id) is str
                and str(UUID(self.input_operation_id)) == self.input_operation_id
                and all(_sha(value) for name, value in asdict(self).items()
                        if name not in {"operation_key", "input_operation_id"})
            )
        except (ValueError, UnicodeError):
            valid = False
        if not valid:
            raise ArtifactConflict("invalid immutable artifact source")


@dataclass(frozen=True)
class SealedProducerFile:
    """An already-open descriptor transferred by the fixed trusted producer broker.

    It remains the caller's responsibility to close the descriptor. Never derive
    this capability from a model path or pass it into an Agent-visible workload.
    """
    fd: int
    producer_revision: str
    media_type: str


@dataclass(frozen=True)
class FrozenArtifact:
    id: str
    ordinal: int
    sha256: str
    byte_length: int
    chunk_count: int
    media_type: str
    producer_revision: str


@dataclass(frozen=True)
class FrozenArtifactBundle:
    operation_key: str
    manifest_sha256: str
    artifacts: tuple[FrozenArtifact, ...]
    total_bytes: int
    expires_at_ns: int


class ArtifactSpool:
    def __init__(self, path: Path, key: bytes, *, maximum_spool_bytes: int = 256 * 1024 * 1024):
        if type(key) is not bytes or len(key) != 32 or type(maximum_spool_bytes) is not int or not 1 <= maximum_spool_bytes <= 4 * 1024**3:
            raise ArtifactConflict("invalid private spool configuration")
        self.path = Path(path)
        if not self.path.is_absolute():
            raise ArtifactConflict("private spool path must be absolute")
        self.maximum_spool_bytes = maximum_spool_bytes
        self._aead = AESGCM(key)
        self.path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        parent = self.path.parent.lstat()
        if not stat.S_ISDIR(parent.st_mode) or parent.st_uid != os.geteuid() or stat.S_IMODE(parent.st_mode) & 0o077:
            raise ArtifactConflict("spool directory must be broker-owned and private")
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_nlink != 1 or stat.S_IMODE(info.st_mode) & 0o077:
                raise ArtifactConflict("spool must be a private broker-owned regular file")
        finally:
            os.close(fd)
        with self._connection() as db:
            if str(db.execute("PRAGMA journal_mode=WAL").fetchone()[0]).lower() != "wal":
                raise ArtifactConflict("spool cannot establish durable WAL")
        with self._connection(write=True) as db:
            app = db.execute("PRAGMA application_id").fetchone()[0]
            version = db.execute("PRAGMA user_version").fetchone()[0]
            tables = db.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall()
            if (app, version) != (_APPLICATION_ID, 1):
                if app != 0 or version != 0 or tables:
                    raise ArtifactConflict("unsupported artifact spool schema")
                db.execute("CREATE TABLE artifact_bundles(operation_key TEXT PRIMARY KEY,binding_json TEXT NOT NULL,binding_hash TEXT NOT NULL,manifest_json TEXT NOT NULL,manifest_hash TEXT NOT NULL,total_bytes INTEGER NOT NULL,ttl_seconds INTEGER NOT NULL,created_at_ns INTEGER NOT NULL,expires_at_ns INTEGER NOT NULL,seal BLOB NOT NULL) STRICT")
                db.execute("CREATE TABLE artifact_chunks(operation_key TEXT NOT NULL,artifact_ordinal INTEGER NOT NULL,chunk_ordinal INTEGER NOT NULL,byte_length INTEGER NOT NULL,sha256 TEXT NOT NULL,ciphertext BLOB,PRIMARY KEY(operation_key,artifact_ordinal,chunk_ordinal),FOREIGN KEY(operation_key) REFERENCES artifact_bundles(operation_key)) STRICT")
                db.execute("CREATE TRIGGER artifact_bundle_immutable BEFORE UPDATE ON artifact_bundles BEGIN SELECT RAISE(ABORT,'artifact source and manifest are immutable'); END")
                db.execute("CREATE TRIGGER artifact_chunk_immutable BEFORE UPDATE ON artifact_chunks WHEN NEW.operation_key IS NOT OLD.operation_key OR NEW.artifact_ordinal IS NOT OLD.artifact_ordinal OR NEW.chunk_ordinal IS NOT OLD.chunk_ordinal OR NEW.byte_length IS NOT OLD.byte_length OR NEW.sha256 IS NOT OLD.sha256 OR (NEW.ciphertext IS NOT OLD.ciphertext AND NOT (NEW.ciphertext IS NULL AND OLD.ciphertext IS NOT NULL AND EXISTS(SELECT 1 FROM artifact_bundles b WHERE b.operation_key=OLD.operation_key AND b.expires_at_ns<=br_artifact_now_ns()))) BEGIN SELECT RAISE(ABORT,'artifact bytes are immutable; only expired ciphertext can be cleared'); END")
                for table in ("artifact_bundles", "artifact_chunks"):
                    db.execute(f"CREATE TRIGGER {table}_no_delete BEFORE DELETE ON {table} BEGIN SELECT RAISE(ABORT,'artifact facts cannot be deleted'); END")
                db.execute(f"PRAGMA application_id={_APPLICATION_ID}")
                db.execute("PRAGMA user_version=1")

    @contextmanager
    def _connection(self, *, write: bool = False) -> Iterator[sqlite3.Connection]:
        db = sqlite3.connect(self.path, timeout=5, isolation_level=None)
        try:
            db.row_factory = sqlite3.Row
            db.create_function("br_artifact_now_ns", 0, time.time_ns)
            db.execute("PRAGMA busy_timeout=5000")
            db.execute("PRAGMA foreign_keys=ON")
            db.execute("PRAGMA synchronous=FULL")
            if write:
                db.execute("BEGIN IMMEDIATE")
            yield db
            if write:
                db.execute("COMMIT")
        except BaseException as error:
            if write:
                try:
                    db.execute("ROLLBACK")
                except sqlite3.Error:
                    pass
            if isinstance(error, sqlite3.Error):
                raise ArtifactUnavailable("artifact persistence unavailable; execution cannot be repeated") from None
            raise
        finally:
            db.close()

    @staticmethod
    def _capture(producer: SealedProducerFile, maximum_bytes: int) -> list[bytes]:
        if type(producer) is not SealedProducerFile or type(producer.fd) is not int or not _sha(producer.producer_revision) or type(producer.media_type) is not str or producer.media_type != "video/mp4":
            raise ArtifactConflict("unsupported typed producer output")
        try:
            before = os.fstat(producer.fd)
            if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or not 1 <= before.st_size <= maximum_bytes or fcntl.fcntl(producer.fd, fcntl.F_GETFL) & os.O_ACCMODE != os.O_RDONLY:
                raise ArtifactConflict("candidate must be a bounded read-only regular descriptor")
            chunks = []
            for offset in range(0, before.st_size, CHUNK_BYTES):
                expected = min(CHUNK_BYTES, before.st_size - offset)
                parts = bytearray()
                while len(parts) < expected:
                    part = os.pread(producer.fd, expected - len(parts), offset + len(parts))
                    if not part:
                        raise ArtifactConflict("producer bytes changed before freeze")
                    parts.extend(part)
                chunks.append(bytes(parts))
            after = os.fstat(producer.fd)
            facts = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_nlink, s.st_mtime_ns, s.st_ctime_ns)
            if facts(before) != facts(after) or os.pread(producer.fd, 1, before.st_size):
                raise ArtifactConflict("producer bytes changed during freeze")
            return chunks
        except OSError:
            raise ArtifactConflict("producer descriptor is unavailable") from None

    @staticmethod
    def _aad(row, purpose: str, ordinal: int | None = None, chunk: int | None = None, sha: str | None = None, length: int | None = None) -> bytes:
        return _canonical({"purpose":purpose,"format":_FORMAT,"binding_hash":row["binding_hash"],"manifest_hash":row["manifest_hash"],"created_at_ns":row["created_at_ns"],"expires_at_ns":row["expires_at_ns"],"artifact_ordinal":ordinal,"chunk_ordinal":chunk,"sha256":sha,"byte_length":length}).encode("ascii")

    def _encrypt(self, clear: bytes, aad: bytes) -> bytes:
        nonce = os.urandom(12)
        return nonce + self._aead.encrypt(nonce, clear, aad)

    def _decrypt(self, cipher, aad: bytes) -> bytes:
        try:
            if type(cipher) is not bytes or len(cipher) < 28:
                raise ArtifactUnavailable("frozen bytes are missing")
            return self._aead.decrypt(cipher[:12], cipher[12:], aad)
        except (InvalidTag, ValueError, TypeError):
            raise ArtifactUnavailable("frozen bytes failed authentication") from None

    @staticmethod
    def _row(db, operation_key: str):
        # length(BLOB) uses SQLite metadata. CASE avoids materializing a corrupt
        # oversized value before authentication. Source/manifest have fixed,
        # bounded structures; only the empty-message seal can be 28 bytes.
        return db.execute("SELECT operation_key,CASE WHEN length(binding_json)<=4096 THEN binding_json END AS binding_json,CASE WHEN length(binding_hash)=64 THEN binding_hash END AS binding_hash,CASE WHEN length(manifest_json)<=16384 THEN manifest_json END AS manifest_json,CASE WHEN length(manifest_hash)=64 THEN manifest_hash END AS manifest_hash,total_bytes,ttl_seconds,created_at_ns,expires_at_ns,CASE WHEN length(seal)=28 THEN seal END AS seal FROM artifact_bundles WHERE operation_key=?", (operation_key,)).fetchone()

    def freeze_once(self, binding: ArtifactBinding, producers: tuple[SealedProducerFile, ...], *, ttl_seconds: int = 3600) -> FrozenArtifactBundle:
        if type(binding) is not ArtifactBinding:
            raise ArtifactConflict("typed artifact source required")
        binding.validate()
        if type(producers) is not tuple or not 1 <= len(producers) <= MAX_ARTIFACTS or type(ttl_seconds) is not int or not 60 <= ttl_seconds <= 86400:
            raise ArtifactConflict("invalid candidate set or retention")
        source_hash = _digest(asdict(binding))
        # Serialize capture across processes before allocating candidate bytes.
        # The database write lock bounds each spool to one <=64 MiB capture,
        # including concurrency from separate ArtifactSpool instances.
        with self._connection(write=True) as db:
            old = self._row(db, binding.operation_key)
            if old is not None and old["binding_hash"] != source_hash:
                raise ArtifactConflict("original execution source changed")
            if old is not None and old["expires_at_ns"] <= time.time_ns():
                raise ArtifactUnavailable("frozen artifact set expired; execution cannot be repeated")
            captured = []
            artifacts = []
            total = 0
            for ordinal, producer in enumerate(producers):
                chunks = self._capture(producer, MAX_BUNDLE_BYTES - total)
                length = sum(map(len, chunks))
                total += length
                sha = hashlib.sha256()
                for chunk in chunks:
                    sha.update(chunk)
                digest = sha.hexdigest()
                artifacts.append(FrozenArtifact(str(uuid5(_NAMESPACE, f"{source_hash}:{ordinal}:{digest}")), ordinal, digest, length, len(chunks), producer.media_type, producer.producer_revision))
                captured.append(chunks)
            manifest = {"format":_FORMAT,"artifacts":[asdict(a) for a in artifacts],"total_bytes":total}
            manifest_hash = _digest(manifest)
            if old is not None:
                if old["binding_hash"] != source_hash or old["manifest_hash"] != manifest_hash or old["ttl_seconds"] != ttl_seconds:
                    raise ArtifactConflict("frozen source, bytes or retention changed")
                return self._recover(db, binding, verify_bytes=True)
            used = db.execute("SELECT COALESCE(sum(length(ciphertext)),0) FROM artifact_chunks").fetchone()[0]
            encrypted_size = total + 28 * sum(len(c) for c in captured)
            if used + encrypted_size > self.maximum_spool_bytes:
                raise ArtifactConflict("private spool capacity exhausted")
            now = time.time_ns()
            row = {"binding_hash":source_hash,"manifest_hash":manifest_hash,"created_at_ns":now,"expires_at_ns":now + ttl_seconds * 1_000_000_000}
            seal = self._encrypt(b"", self._aad(row, "artifact-bundle-seal"))
            db.execute("INSERT INTO artifact_bundles VALUES(?,?,?,?,?,?,?,?,?,?)", (binding.operation_key, _canonical(asdict(binding)), source_hash, _canonical(manifest), manifest_hash, total, ttl_seconds, now, row["expires_at_ns"], seal))
            for ordinal, chunks in enumerate(captured):
                for number, clear in enumerate(chunks):
                    digest = _hash(clear)
                    cipher = self._encrypt(clear, self._aad(row, "artifact-upload-chunk", ordinal, number, digest, len(clear)))
                    db.execute("INSERT INTO artifact_chunks VALUES(?,?,?,?,?,?)", (binding.operation_key, ordinal, number, len(clear), digest, cipher))
            return FrozenArtifactBundle(binding.operation_key, manifest_hash, tuple(artifacts), total, row["expires_at_ns"])

    def _recover(self, db, binding: ArtifactBinding, *, verify_bytes: bool) -> FrozenArtifactBundle:
        binding.validate()
        row = self._row(db, binding.operation_key)
        if row is None or row["expires_at_ns"] <= time.time_ns():
            raise ArtifactUnavailable("frozen artifact set is missing or expired; execution cannot be repeated")
        if row["binding_json"] != _canonical(asdict(binding)) or row["binding_hash"] != _digest(asdict(binding)):
            raise ArtifactConflict("original execution source changed")
        try:
            manifest = json.loads(row["manifest_json"])
            if _canonical(manifest) != row["manifest_json"] or _digest(manifest) != row["manifest_hash"] or set(manifest) != {"format", "artifacts", "total_bytes"} or manifest["format"] != _FORMAT:
                raise ValueError()
            artifacts = tuple(FrozenArtifact(**a) for a in manifest["artifacts"])
            if not 1 <= len(artifacts) <= MAX_ARTIFACTS or type(row["total_bytes"]) is not int or not 1 <= row["total_bytes"] <= MAX_BUNDLE_BYTES or row["total_bytes"] != manifest["total_bytes"] or sum(a.byte_length for a in artifacts) != row["total_bytes"]:
                raise ValueError()
            for ordinal, a in enumerate(artifacts):
                if type(a.ordinal) is not int or a.ordinal != ordinal or type(a.byte_length) is not int or not 1 <= a.byte_length <= MAX_BUNDLE_BYTES or type(a.chunk_count) is not int or a.chunk_count != (a.byte_length + CHUNK_BYTES - 1) // CHUNK_BYTES or type(a.media_type) is not str or a.media_type != "video/mp4" or not _sha(a.sha256) or not _sha(a.producer_revision) or a.id != str(uuid5(_NAMESPACE, f"{row['binding_hash']}:{ordinal}:{a.sha256}")):
                    raise ValueError()
            if row["expires_at_ns"] != row["created_at_ns"] + row["ttl_seconds"] * 1_000_000_000 or not 60 <= row["ttl_seconds"] <= 86400 or self._decrypt(row["seal"], self._aad(row, "artifact-bundle-seal")) != b"":
                raise ValueError()
        except (ValueError, TypeError, KeyError):
            raise ArtifactUnavailable("frozen artifact manifest failed authentication") from None
        if verify_bytes:
            expected_count = sum(a.chunk_count for a in artifacts)
            if db.execute("SELECT count(*) FROM artifact_chunks WHERE operation_key=?", (binding.operation_key,)).fetchone()[0] != expected_count:
                raise ArtifactUnavailable("frozen artifact chunks are incomplete")
            for a in artifacts:
                sha = hashlib.sha256()
                for number in range(a.chunk_count):
                    sha.update(self._read_chunk(db, row, a, number))
                if sha.hexdigest() != a.sha256:
                    raise ArtifactUnavailable("frozen whole-file digest differs")
        if row["expires_at_ns"] <= time.time_ns():
            raise ArtifactUnavailable("frozen artifact set expired during recovery")
        return FrozenArtifactBundle(binding.operation_key, row["manifest_hash"], artifacts, row["total_bytes"], row["expires_at_ns"])

    def _read_chunk(self, db, row, artifact: FrozenArtifact, number: int) -> bytes:
        if type(number) is not int or not 0 <= number < artifact.chunk_count:
            raise ArtifactConflict("invalid private upload chunk ordinal")
        expected = min(CHUNK_BYTES, artifact.byte_length - number * CHUNK_BYTES)
        chunk = db.execute("SELECT byte_length,CASE WHEN length(sha256)=64 THEN sha256 END AS sha256,CASE WHEN length(ciphertext)=? THEN ciphertext END AS ciphertext FROM artifact_chunks WHERE operation_key=? AND artifact_ordinal=? AND chunk_ordinal=?", (expected+28, row["operation_key"], artifact.ordinal, number)).fetchone()
        if chunk is None or chunk["byte_length"] != expected or not _sha(chunk["sha256"]):
            raise ArtifactUnavailable("frozen artifact chunk is missing or inconsistent")
        clear = self._decrypt(chunk["ciphertext"], self._aad(row, "artifact-upload-chunk", artifact.ordinal, number, chunk["sha256"], expected))
        if len(clear) != expected or _hash(clear) != chunk["sha256"]:
            raise ArtifactUnavailable("frozen artifact chunk digest differs")
        return clear

    def recover(self, binding: ArtifactBinding) -> FrozenArtifactBundle:
        if type(binding) is not ArtifactBinding:
            raise ArtifactConflict("typed artifact source required")
        with self._connection() as db:
            return self._recover(db, binding, verify_bytes=True)

    def read_private_chunk_for_upload(self, binding: ArtifactBinding, artifact_ordinal: int, chunk_ordinal: int) -> bytes:
        if type(binding) is not ArtifactBinding or type(artifact_ordinal) is not int:
            raise ArtifactConflict("typed source and artifact ordinal required")
        with self._connection() as db:
            bundle = self._recover(db, binding, verify_bytes=False)
            if not 0 <= artifact_ordinal < len(bundle.artifacts):
                raise ArtifactConflict("invalid private upload artifact ordinal")
            row = self._row(db, binding.operation_key)
            clear = self._read_chunk(db, row, bundle.artifacts[artifact_ordinal], chunk_ordinal)
            if row["expires_at_ns"] <= time.time_ns():
                raise ArtifactUnavailable("frozen artifact set expired during private upload read")
            return clear

    def expire(self) -> int:
        with self._connection(write=True) as db:
            return db.execute("UPDATE artifact_chunks SET ciphertext=NULL WHERE ciphertext IS NOT NULL AND operation_key IN (SELECT operation_key FROM artifact_bundles WHERE expires_at_ns<=br_artifact_now_ns())").rowcount
