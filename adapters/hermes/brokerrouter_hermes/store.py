from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass, field
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import time
from typing import Iterator

from cryptography.hazmat.primitives.ciphers.aead import AESGCM


SCHEMA_VERSION = 3
MAX_RESULT_BYTES = 4 * 1024 * 1024
RESULT_FORMAT = "utf8-text-v1"
OUTPUT_STATES = frozenset({"quarantined", "checking", "waiting_approval", "ready", "rejected", "unavailable"})
STATES = frozenset(
    {
        "staged",
        "checking",
        "waiting_approval",
        "ready",
        "claim_prepared",
        "dispatch_authorized",
        "dispatching",
        "effect_recorded",
        "completed",
        "claim_unknown",
        "execution_unknown",
        "rejected",
    }
)


class CheckpointConflict(RuntimeError):
    pass


@dataclass(frozen=True)
class Checkpoint:
    operation_key: str
    identity_hash: str
    tool_name: str
    local_payload_hash: str
    operation_id: str | None
    gateway_payload_hash: str | None
    model_binding_hash: str | None
    state: str
    claim_token: str | None = field(repr=False)
    evidence_ref: str | None


@dataclass(frozen=True)
class FrozenResult:
    """Durable execution output; ``ready`` is an observation, never a release grant."""

    operation_key: str
    sha256: str
    byte_length: int
    result_format: str
    text: str | None = field(repr=False)
    server_result_id: str | None
    output_operation_id: str | None
    output_payload_hash: str | None
    state: str
    expires_at_ns: int


@dataclass(frozen=True)
class NativeOrigin:
    operation_key: str
    identity_hash: str
    tool_name: str
    model_binding_hash: str
    original_payload_hash: str
    final_payload_hash: str | None
    effect_recorded_at_ns: int | None


class CheckpointStore:
    def __init__(self, path: Path, key: bytes) -> None:
        if type(key) is not bytes or len(key) != 32:
            raise CheckpointConflict("checkpoint encryption requires a 32-byte AES key")
        self.path = path
        self._aead = AESGCM(key)
        self._prepare_path()
        self._initialize_database()
        self._migrate()

    def _prepare_path(self) -> None:
        parent = self.path.parent
        parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if parent.is_symlink() or (self.path.exists() and self.path.is_symlink()):
            raise CheckpointConflict("checkpoint path must not contain a symlink at its final component")
        mode = stat.S_IMODE(parent.stat().st_mode)
        if mode & 0o022:
            raise CheckpointConflict("checkpoint directory must not be group/world writable")

    def _initialize_database(self) -> None:
        # Establish the file's permissions before SQLite opens it or creates WAL files.
        # O_NOFOLLOW rejects a symlink substituted before this permission check.
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid():
                raise CheckpointConflict("checkpoint must be a regular file owned by this runtime")
            os.fchmod(fd, 0o600)
        finally:
            os.close(fd)
        connection = sqlite3.connect(self.path, timeout=5.0, isolation_level=None)
        try:
            connection.execute("PRAGMA busy_timeout=5000")
            mode = connection.execute("PRAGMA journal_mode=WAL").fetchone()[0]
            if str(mode).lower() != "wal":
                raise CheckpointConflict("checkpoint database could not enable WAL mode")
            connection.execute("PRAGMA synchronous=FULL")
        finally:
            connection.close()

    @contextmanager
    def _connection(self, *, write: bool = False) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(self.path, timeout=5.0, isolation_level=None)
        try:
            connection.row_factory = sqlite3.Row
            connection.execute("PRAGMA busy_timeout=5000")
            connection.execute("PRAGMA foreign_keys=ON")
            connection.execute("PRAGMA synchronous=FULL")
            if write:
                connection.execute("BEGIN IMMEDIATE")
            yield connection
            if write:
                connection.execute("COMMIT")
        except BaseException:
            if write:
                try:
                    connection.execute("ROLLBACK")
                except sqlite3.Error:
                    pass
            raise
        finally:
            connection.close()

    def _migrate(self) -> None:
        with self._connection(write=True) as connection:
            version = connection.execute("PRAGMA user_version").fetchone()[0]
            if version not in (0, 1, 2, SCHEMA_VERSION):
                raise CheckpointConflict("unsupported checkpoint schema version")
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS brokerrouter_checkpoints (
                    operation_key TEXT PRIMARY KEY,
                    identity_hash TEXT NOT NULL,
                    tool_name TEXT NOT NULL,
                    local_payload_hash TEXT NOT NULL,
                    operation_id TEXT,
                    gateway_payload_hash TEXT,
                    state TEXT NOT NULL,
                    claim_ciphertext BLOB,
                    evidence_ref TEXT,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    CHECK(length(operation_key) BETWEEN 1 AND 200),
                    CHECK(length(tool_name) BETWEEN 1 AND 200),
                    CHECK(state IN (
                        'staged','checking','waiting_approval','ready','claim_prepared',
                        'dispatch_authorized','dispatching','effect_recorded','completed',
                        'claim_unknown','execution_unknown','rejected'
                    ))
                ) STRICT
                """
            )
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS brokerrouter_approval_notices (
                    operation_key TEXT PRIMARY KEY REFERENCES brokerrouter_checkpoints(operation_key) ON DELETE CASCADE,
                    approval_id TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('pending','sending','sent')),
                    lease_until INTEGER,
                    updated_at INTEGER NOT NULL,
                    CHECK(length(approval_id) BETWEEN 1 AND 100)
                ) STRICT
                """
            )
            if "model_binding_hash" not in {
                row["name"] for row in connection.execute("PRAGMA table_info(brokerrouter_checkpoints)")
            }:
                connection.execute("ALTER TABLE brokerrouter_checkpoints ADD COLUMN model_binding_hash TEXT")
            connection.execute(
                """
                CREATE TRIGGER IF NOT EXISTS brokerrouter_checkpoint_model_immutable
                BEFORE UPDATE OF model_binding_hash ON brokerrouter_checkpoints
                WHEN OLD.model_binding_hash IS NOT NULL AND NEW.model_binding_hash IS NOT OLD.model_binding_hash
                BEGIN
                    SELECT RAISE(ABORT,'execution model identity cannot change');
                END
                """
            )
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS brokerrouter_native_origins (
                    operation_key TEXT PRIMARY KEY,
                    identity_hash TEXT NOT NULL,
                    tool_name TEXT NOT NULL,
                    model_binding_hash TEXT NOT NULL,
                    original_payload_hash TEXT NOT NULL,
                    final_payload_hash TEXT,
                    effect_recorded_at_ns INTEGER,
                    created_at_ns INTEGER NOT NULL,
                    CHECK(length(operation_key) BETWEEN 1 AND 200),
                    CHECK(length(tool_name) BETWEEN 1 AND 200),
                    CHECK(length(model_binding_hash)=64 AND model_binding_hash NOT GLOB '*[^0-9a-f]*'),
                    CHECK(length(original_payload_hash)=64 AND original_payload_hash NOT GLOB '*[^0-9a-f]*'),
                    CHECK(final_payload_hash IS NULL OR (length(final_payload_hash)=64 AND final_payload_hash NOT GLOB '*[^0-9a-f]*')),
                    CHECK(effect_recorded_at_ns IS NULL OR final_payload_hash IS NOT NULL)
                ) STRICT
                """
            )
            connection.execute(
                """
                CREATE TRIGGER IF NOT EXISTS brokerrouter_native_origin_immutable
                BEFORE UPDATE ON brokerrouter_native_origins
                BEGIN
                    SELECT CASE WHEN
                        (NEW.operation_key,NEW.identity_hash,NEW.tool_name,NEW.model_binding_hash,NEW.original_payload_hash,NEW.created_at_ns)
                            IS NOT (OLD.operation_key,OLD.identity_hash,OLD.tool_name,OLD.model_binding_hash,OLD.original_payload_hash,OLD.created_at_ns)
                        OR (OLD.final_payload_hash IS NOT NULL AND NEW.final_payload_hash IS NOT OLD.final_payload_hash)
                        OR (OLD.effect_recorded_at_ns IS NOT NULL AND NEW.effect_recorded_at_ns IS NOT OLD.effect_recorded_at_ns)
                        THEN RAISE(ABORT,'native execution identity cannot change') END;
                END
                """
            )
            connection.execute(
                """
                CREATE TRIGGER IF NOT EXISTS brokerrouter_native_origin_no_delete
                BEFORE DELETE ON brokerrouter_native_origins
                BEGIN
                    SELECT RAISE(ABORT,'native execution identity cannot be deleted');
                END
                """
            )
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS brokerrouter_tool_results (
                    operation_key TEXT PRIMARY KEY REFERENCES brokerrouter_checkpoints(operation_key),
                    result_format TEXT NOT NULL CHECK(result_format='utf8-text-v1'),
                    sha256 TEXT NOT NULL CHECK(length(sha256)=64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
                    byte_length INTEGER NOT NULL CHECK(byte_length BETWEEN 0 AND 4194304),
                    result_ciphertext BLOB,
                    server_result_id TEXT,
                    output_operation_id TEXT,
                    output_payload_hash TEXT,
                    state TEXT NOT NULL DEFAULT 'quarantined'
                        CHECK(state IN ('quarantined','checking','waiting_approval','ready','rejected','unavailable')),
                    created_at_ns INTEGER NOT NULL,
                    expires_at_ns INTEGER NOT NULL CHECK(expires_at_ns>created_at_ns),
                    updated_at_ns INTEGER NOT NULL,
                    CHECK((state IN ('rejected','unavailable') AND result_ciphertext IS NULL)
                       OR (state NOT IN ('rejected','unavailable') AND result_ciphertext IS NOT NULL
                            AND length(result_ciphertext)>=29)),
                    CHECK(output_payload_hash IS NULL OR
                        (length(output_payload_hash)=64 AND output_payload_hash NOT GLOB '*[^0-9a-f]*')),
                    CHECK(output_operation_id IS NULL OR length(output_operation_id) BETWEEN 1 AND 100),
                    CHECK(state<>'ready' OR (output_operation_id IS NOT NULL AND output_payload_hash IS NOT NULL))
                ) STRICT
                """
            )
            # Version 3 is still under development; preserve databases created before
            # the remote-result identity was added without rewriting frozen ciphertext.
            if "server_result_id" not in {
                row["name"] for row in connection.execute("PRAGMA table_info(brokerrouter_tool_results)")
            }:
                connection.execute("ALTER TABLE brokerrouter_tool_results ADD COLUMN server_result_id TEXT")
            connection.execute(
                """
                CREATE TRIGGER IF NOT EXISTS brokerrouter_remote_result_immutable
                BEFORE UPDATE OF server_result_id ON brokerrouter_tool_results
                WHEN OLD.server_result_id IS NOT NULL AND NEW.server_result_id IS NOT OLD.server_result_id
                BEGIN
                    SELECT RAISE(ABORT,'remote result identity cannot change');
                END
                """
            )
            connection.execute(
                """
                CREATE TRIGGER IF NOT EXISTS brokerrouter_tool_result_immutable
                BEFORE UPDATE ON brokerrouter_tool_results
                BEGIN
                    SELECT CASE WHEN
                        (NEW.operation_key,NEW.result_format,NEW.sha256,NEW.byte_length,NEW.created_at_ns,NEW.expires_at_ns)
                            IS NOT (OLD.operation_key,OLD.result_format,OLD.sha256,OLD.byte_length,OLD.created_at_ns,OLD.expires_at_ns)
                        OR (NEW.result_ciphertext IS NOT OLD.result_ciphertext AND NEW.result_ciphertext IS NOT NULL)
                        OR (OLD.output_operation_id IS NOT NULL AND NEW.output_operation_id IS NOT OLD.output_operation_id)
                        OR (OLD.output_payload_hash IS NOT NULL AND NEW.output_payload_hash IS NOT OLD.output_payload_hash)
                        OR (OLD.state IN ('rejected','unavailable') AND NEW.state IS NOT OLD.state)
                        THEN RAISE(ABORT,'frozen result identity cannot change or reopen') END;
                END
                """
            )
            connection.execute(
                """
                CREATE TRIGGER IF NOT EXISTS brokerrouter_tool_result_no_delete
                BEFORE DELETE ON brokerrouter_tool_results
                BEGIN
                    SELECT RAISE(ABORT,'execution output evidence cannot be deleted');
                END
                """
            )
            connection.execute(
                """
                CREATE TRIGGER IF NOT EXISTS brokerrouter_checkpoint_no_redispatch
                BEFORE UPDATE ON brokerrouter_checkpoints
                WHEN OLD.state IN ('claim_prepared','dispatch_authorized','dispatching','effect_recorded',
                    'completed','claim_unknown','execution_unknown','rejected') AND NOT (
                        NEW.state=OLD.state
                        OR (OLD.state='claim_prepared' AND NEW.state IN ('dispatch_authorized','claim_unknown'))
                        OR (OLD.state='dispatch_authorized' AND NEW.state IN ('dispatching','execution_unknown'))
                        OR (OLD.state='dispatching' AND NEW.state IN ('effect_recorded','execution_unknown'))
                        OR (OLD.state='effect_recorded' AND NEW.state='completed')
                    )
                BEGIN
                    SELECT RAISE(ABORT,'execution fact cannot return to a dispatchable state');
                END
                """
            )
            connection.execute(
                "CREATE INDEX IF NOT EXISTS brokerrouter_result_expiry ON brokerrouter_tool_results(expires_at_ns) WHERE result_ciphertext IS NOT NULL"
            )
            connection.execute(f"PRAGMA user_version={SCHEMA_VERSION}")
        os.chmod(self.path, 0o600)

    def _seal(self, operation_key: str, token: str) -> bytes:
        nonce = os.urandom(12)
        return nonce + self._aead.encrypt(nonce, token.encode("ascii"), operation_key.encode("ascii"))

    def _open(self, operation_key: str, value: bytes | None) -> str | None:
        if value is None:
            return None
        if len(value) < 29:
            raise CheckpointConflict("invalid encrypted claim token")
        try:
            return self._aead.decrypt(value[:12], value[12:], operation_key.encode("ascii")).decode("ascii")
        except Exception as exc:
            raise CheckpointConflict("claim token cannot be decrypted") from exc

    def _row(self, row: sqlite3.Row | None) -> Checkpoint | None:
        if row is None:
            return None
        if row["state"] not in STATES:
            raise CheckpointConflict("invalid checkpoint state")
        return Checkpoint(
            operation_key=row["operation_key"],
            identity_hash=row["identity_hash"],
            tool_name=row["tool_name"],
            local_payload_hash=row["local_payload_hash"],
            operation_id=row["operation_id"],
            gateway_payload_hash=row["gateway_payload_hash"],
            model_binding_hash=row["model_binding_hash"],
            state=row["state"],
            claim_token=self._open(row["operation_key"], row["claim_ciphertext"]),
            evidence_ref=row["evidence_ref"],
        )

    def load(self, operation_key: str) -> Checkpoint | None:
        with self._connection() as connection:
            return self._row(
                connection.execute(
                    "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
                ).fetchone()
            )

    @staticmethod
    def _result_aad(checkpoint: sqlite3.Row, result: sqlite3.Row | dict[str, object]) -> bytes:
        # The purpose, exact execution identity, immutable bytes and retention are all
        # authenticated. Claim-token ciphertext and another call's output cannot be used here.
        context = {
            "operation_key": checkpoint["operation_key"],
            "identity_hash": checkpoint["identity_hash"],
            "tool_name": checkpoint["tool_name"],
            "local_payload_hash": checkpoint["local_payload_hash"],
            "input_operation_id": checkpoint["operation_id"],
            "result_format": result["result_format"],
            "sha256": result["sha256"],
            "byte_length": result["byte_length"],
            "created_at_ns": result["created_at_ns"],
            "expires_at_ns": result["expires_at_ns"],
        }
        if checkpoint["model_binding_hash"] is not None:
            context["model_binding_hash"] = checkpoint["model_binding_hash"]
        return b"brokerrouter-hermes/tool-result/v1\0" + json.dumps(
            context,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")

    def _result_row(self, checkpoint: sqlite3.Row, row: sqlite3.Row) -> FrozenResult:
        if row["state"] not in OUTPUT_STATES or row["result_format"] != RESULT_FORMAT:
            raise CheckpointConflict("invalid frozen result contract")
        text = None
        if row["result_ciphertext"] is not None:
            envelope = row["result_ciphertext"]
            if len(envelope) < 29 or envelope[0] != 1:
                raise CheckpointConflict("invalid encrypted tool result")
            try:
                clear = self._aead.decrypt(
                    envelope[1:13], envelope[13:], self._result_aad(checkpoint, row)
                )
                if len(clear) != row["byte_length"] or hashlib.sha256(clear).hexdigest() != row["sha256"]:
                    raise ValueError("tool result digest mismatch")
                text = clear.decode("utf-8")
            except Exception as exc:
                raise CheckpointConflict("tool result cannot be authenticated") from exc
        elif row["state"] not in {"rejected", "unavailable"}:
            raise CheckpointConflict("frozen result has no ciphertext")
        if checkpoint["state"] not in {"effect_recorded", "completed"}:
            raise CheckpointConflict("frozen output has no recorded execution fact")
        return FrozenResult(
            operation_key=row["operation_key"],
            sha256=row["sha256"],
            byte_length=row["byte_length"],
            result_format=row["result_format"],
            text=text,
            server_result_id=row["server_result_id"],
            output_operation_id=row["output_operation_id"],
            output_payload_hash=row["output_payload_hash"],
            state=row["state"],
            expires_at_ns=row["expires_at_ns"],
        )

    def freeze_result(
        self,
        operation_key: str,
        result: str,
        evidence_ref: str,
        *,
        ttl_seconds: int = 3600,
    ) -> FrozenResult:
        """Commit standalone output and effect_recorded together, before its receipt."""
        return self._freeze_result(operation_key, result, evidence_ref, ttl_seconds=ttl_seconds, native=False)

    def freeze_native_result(
        self, operation_key: str, result: str, evidence_ref: str, *, ttl_seconds: int = 3600
    ) -> FrozenResult:
        """Freeze final host text only for an authenticated native known effect."""
        return self._freeze_result(operation_key, result, evidence_ref, ttl_seconds=ttl_seconds, native=True)

    def _freeze_result(
        self, operation_key: str, result: str, evidence_ref: str, *, ttl_seconds: int, native: bool
    ) -> FrozenResult:
        """Commit exact output before permitting publication.

        A failed/uncertain return never permits dispatch again. Reopen this store and
        inspect the execution fact: a committed result is recoverable; an uncommitted
        dispatch remains unknown. Legacy recorded effects without output cannot be backfilled.
        """
        if type(result) is not str or len(result) > MAX_RESULT_BYTES:
            raise CheckpointConflict("tool result must be an exact bounded string")
        try:
            clear = result.encode("utf-8")
        except UnicodeEncodeError as exc:
            raise CheckpointConflict("tool result must be valid UTF-8") from exc
        if len(clear) > MAX_RESULT_BYTES:
            raise CheckpointConflict("tool result exceeds 4 MiB")
        if type(evidence_ref) is not str or len(evidence_ref) > 1000:
            raise CheckpointConflict("invalid tool result evidence")
        try:
            evidence_bytes = evidence_ref.encode("utf-8")
        except UnicodeEncodeError as exc:
            raise CheckpointConflict("invalid tool result evidence") from exc
        if (
            type(evidence_ref) is not str
            or not evidence_ref.strip()
            or len(evidence_bytes) > 1000
            or any(ord(c) < 32 for c in evidence_ref)
            or type(ttl_seconds) is not int
            or not 60 <= ttl_seconds <= 86400
        ):
            raise CheckpointConflict("invalid tool result evidence or retention")
        now = time.time_ns()
        digest = hashlib.sha256(clear).hexdigest()
        with self._connection(write=True) as connection:
            checkpoint = connection.execute(
                "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if checkpoint is None:
                raise CheckpointConflict("tool result has no execution checkpoint")
            origin = connection.execute(
                "SELECT * FROM brokerrouter_native_origins WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if native:
                self._native_checkpoint(origin, checkpoint)
                if origin["effect_recorded_at_ns"] is None or checkpoint["evidence_ref"] != evidence_ref:
                    raise CheckpointConflict("native final text has no recorded execution effect")
            elif origin is not None:
                raise CheckpointConflict("native execution requires its final publication boundary")
            old = connection.execute(
                "SELECT * FROM brokerrouter_tool_results WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if old is not None:
                frozen = self._result_row(checkpoint, old)
                if (
                    frozen.text != result
                    or frozen.sha256 != digest
                    or checkpoint["evidence_ref"] != evidence_ref
                    or frozen.expires_at_ns <= now
                ):
                    raise CheckpointConflict("frozen tool result cannot change or reopen")
                return frozen
            if (
                checkpoint["state"] not in ({"effect_recorded", "completed"} if native else {"dispatching"})
                or checkpoint["operation_id"] is None
                or checkpoint["gateway_payload_hash"] is None
                or checkpoint["claim_ciphertext"] is None
            ):
                raise CheckpointConflict("only an authorized dispatch can freeze a new result")
            # Verify the persisted claim is authentic before recording its execution result.
            self._open(operation_key, checkpoint["claim_ciphertext"])
            facts = {
                "result_format": RESULT_FORMAT,
                "sha256": digest,
                "byte_length": len(clear),
                "created_at_ns": now,
                "expires_at_ns": now + ttl_seconds * 1_000_000_000,
            }
            nonce = os.urandom(12)
            encrypted = b"\x01" + nonce + self._aead.encrypt(
                nonce, clear, self._result_aad(checkpoint, facts)
            )
            connection.execute(
                "INSERT INTO brokerrouter_tool_results(operation_key,result_format,sha256,byte_length,result_ciphertext,created_at_ns,expires_at_ns,updated_at_ns) VALUES(?,?,?,?,?,?,?,?)",
                (operation_key, RESULT_FORMAT, digest, len(clear), encrypted, now, facts["expires_at_ns"], now),
            )
            if not native:
                connection.execute(
                    "UPDATE brokerrouter_checkpoints SET state='effect_recorded',evidence_ref=?,updated_at=? WHERE operation_key=? AND state='dispatching'",
                    (evidence_ref, now, operation_key),
                )
        frozen = self.load_result(operation_key)
        if frozen is None:
            raise CheckpointConflict("tool result commit was not observable")
        return frozen

    def load_result(self, operation_key: str) -> FrozenResult | None:
        # Expired ciphertext is removed from readable rows; digest/effect facts remain.
        with self._connection(write=True) as connection:
            connection.execute(
                "UPDATE brokerrouter_tool_results SET state='unavailable',result_ciphertext=NULL,updated_at_ns=? WHERE operation_key=? AND result_ciphertext IS NOT NULL AND expires_at_ns<=?",
                (time.time_ns(), operation_key, time.time_ns()),
            )
            row = connection.execute(
                "SELECT * FROM brokerrouter_tool_results WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if row is None:
                return None
            checkpoint = connection.execute(
                "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if checkpoint is None:
                raise CheckpointConflict("tool result execution checkpoint is missing")
            return self._result_row(checkpoint, row)

    def bind_remote_result(
        self,
        operation_key: str,
        server_result_id: str,
        input_operation_id: str,
        sha256: str,
        byte_length: int,
    ) -> FrozenResult:
        """Bind a remote record to its authenticated input operation and exact bytes."""
        from uuid import UUID

        try:
            if str(UUID(server_result_id)) != server_result_id:
                raise ValueError("noncanonical result id")
        except (ValueError, TypeError, AttributeError) as exc:
            raise CheckpointConflict("invalid remote result identity") from exc
        with self._connection(write=True) as connection:
            row = connection.execute(
                "SELECT * FROM brokerrouter_tool_results WHERE operation_key=?", (operation_key,)
            ).fetchone()
            checkpoint = connection.execute(
                "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if row is None or checkpoint is None:
                raise CheckpointConflict("remote result has no frozen execution result")
            frozen = self._result_row(checkpoint, row)
            if (
                frozen.state in {"rejected", "unavailable"}
                or frozen.expires_at_ns <= time.time_ns()
                or checkpoint["operation_id"] != input_operation_id
                or frozen.sha256 != sha256
                or type(byte_length) is not int
                or frozen.byte_length != byte_length
                or (frozen.server_result_id is not None and frozen.server_result_id != server_result_id)
            ):
                raise CheckpointConflict("remote result context changed or cannot reopen")
            connection.execute(
                "UPDATE brokerrouter_tool_results SET server_result_id=?,updated_at_ns=? WHERE operation_key=?",
                (server_result_id, time.time_ns(), operation_key),
            )
        frozen = self.load_result(operation_key)
        if frozen is None:
            raise CheckpointConflict("remote result binding was not observable")
        return frozen

    def output_checked(
        self,
        operation_key: str,
        output_operation_id: str,
        state: str,
        gateway_payload_hash: str | None,
    ) -> FrozenResult:
        """Record one check response without granting permission to publish the plaintext."""
        from uuid import UUID

        try:
            if str(UUID(output_operation_id)) != output_operation_id:
                raise ValueError("noncanonical operation id")
        except (ValueError, TypeError, AttributeError) as exc:
            raise CheckpointConflict("invalid output operation identity") from exc
        if state not in {"checking", "waiting_approval", "ready", "rejected"}:
            raise CheckpointConflict("invalid output check state")
        if gateway_payload_hash is not None and (
            type(gateway_payload_hash) is not str
            or len(gateway_payload_hash) != 64
            or any(c not in "0123456789abcdef" for c in gateway_payload_hash)
        ):
            raise CheckpointConflict("invalid output payload hash")
        if state == "ready" and gateway_payload_hash is None:
            raise CheckpointConflict("ready output has no gateway payload hash")
        with self._connection(write=True) as connection:
            row = connection.execute(
                "SELECT * FROM brokerrouter_tool_results WHERE operation_key=?", (operation_key,)
            ).fetchone()
            checkpoint = connection.execute(
                "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if row is None or checkpoint is None:
                raise CheckpointConflict("output check has no frozen execution result")
            self._result_row(checkpoint, row)
            if row["state"] in {"rejected", "unavailable"} or row["expires_at_ns"] <= time.time_ns():
                raise CheckpointConflict("frozen result cannot be checked or reopened")
            if row["output_operation_id"] is not None and row["output_operation_id"] != output_operation_id:
                raise CheckpointConflict("output operation identity changed")
            if (
                row["output_payload_hash"] is not None
                and gateway_payload_hash is not None
                and row["output_payload_hash"] != gateway_payload_hash
            ):
                raise CheckpointConflict("output operation payload changed")
            connection.execute(
                "UPDATE brokerrouter_tool_results SET output_operation_id=?,output_payload_hash=COALESCE(?,output_payload_hash),state=?,result_ciphertext=CASE WHEN ?='rejected' THEN NULL ELSE result_ciphertext END,updated_at_ns=? WHERE operation_key=?",
                (output_operation_id, gateway_payload_hash, state, state, time.time_ns(), operation_key),
            )
        frozen = self.load_result(operation_key)
        if frozen is None:
            raise CheckpointConflict("output check commit was not observable")
        return frozen

    def mark_result_unavailable(self, operation_key: str) -> FrozenResult:
        with self._connection(write=True) as connection:
            changed = connection.execute(
                "UPDATE brokerrouter_tool_results SET state=CASE WHEN state='rejected' THEN state ELSE 'unavailable' END,result_ciphertext=NULL,updated_at_ns=? WHERE operation_key=?",
                (time.time_ns(), operation_key),
            ).rowcount
            if changed != 1:
                raise CheckpointConflict("tool result does not exist")
        frozen = self.load_result(operation_key)
        if frozen is None:
            raise CheckpointConflict("tool result removal commit was not observable")
        return frozen

    def expire_results(self, *, limit: int = 100) -> int:
        if type(limit) is not int or not 1 <= limit <= 1000:
            raise CheckpointConflict("invalid result maintenance batch")
        now = time.time_ns()
        with self._connection(write=True) as connection:
            return connection.execute(
                "UPDATE brokerrouter_tool_results SET state='unavailable',result_ciphertext=NULL,updated_at_ns=? WHERE operation_key IN (SELECT operation_key FROM brokerrouter_tool_results WHERE result_ciphertext IS NOT NULL AND expires_at_ns<=? ORDER BY expires_at_ns LIMIT ?)",
                (now, now, limit),
            ).rowcount

    @staticmethod
    def _native_origin(row: sqlite3.Row) -> NativeOrigin:
        return NativeOrigin(**{name: row[name] for name in NativeOrigin.__dataclass_fields__})

    @staticmethod
    def _native_checkpoint(origin: sqlite3.Row | None, checkpoint: sqlite3.Row | None) -> None:
        if (
            origin is None or checkpoint is None or origin["final_payload_hash"] is None
            or origin["identity_hash"] != checkpoint["identity_hash"]
            or origin["tool_name"] != checkpoint["tool_name"]
            or origin["model_binding_hash"] != checkpoint["model_binding_hash"]
            or origin["final_payload_hash"] != checkpoint["local_payload_hash"]
        ):
            raise CheckpointConflict("native execution context is missing or changed")

    def register_native_origin(
        self, operation_key: str, identity_hash: str, tool_name: str,
        model_binding_hash: str, original_payload_hash: str,
    ) -> NativeOrigin:
        with self._connection(write=True) as connection:
            old = connection.execute(
                "SELECT * FROM brokerrouter_native_origins WHERE operation_key=?", (operation_key,)
            ).fetchone()
            checkpoint = connection.execute(
                "SELECT state FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if old is None:
                if checkpoint is not None and checkpoint["state"] not in {"staged", "checking", "waiting_approval", "ready"}:
                    raise CheckpointConflict("an earlier execution cannot become a native origin")
                connection.execute(
                    "INSERT INTO brokerrouter_native_origins(operation_key,identity_hash,tool_name,model_binding_hash,original_payload_hash,created_at_ns) VALUES(?,?,?,?,?,?)",
                    (operation_key, identity_hash, tool_name, model_binding_hash, original_payload_hash, time.time_ns()),
                )
                old = connection.execute(
                    "SELECT * FROM brokerrouter_native_origins WHERE operation_key=?", (operation_key,)
                ).fetchone()
            if (
                old["identity_hash"] != identity_hash or old["tool_name"] != tool_name
                or old["model_binding_hash"] != model_binding_hash
                or old["original_payload_hash"] != original_payload_hash
            ):
                raise CheckpointConflict("native origin identity or original arguments changed")
            return self._native_origin(old)

    def bind_native_final(
        self, operation_key: str, identity_hash: str, tool_name: str,
        model_binding_hash: str, final_payload_hash: str,
    ) -> NativeOrigin:
        with self._connection(write=True) as connection:
            row = connection.execute(
                "SELECT * FROM brokerrouter_native_origins WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if (
                row is None or row["identity_hash"] != identity_hash or row["tool_name"] != tool_name
                or row["model_binding_hash"] != model_binding_hash
                or (row["final_payload_hash"] is not None and row["final_payload_hash"] != final_payload_hash)
            ):
                raise CheckpointConflict("native final identity or arguments changed")
            connection.execute(
                "UPDATE brokerrouter_native_origins SET final_payload_hash=? WHERE operation_key=?",
                (final_payload_hash, operation_key),
            )
            row = connection.execute(
                "SELECT * FROM brokerrouter_native_origins WHERE operation_key=?", (operation_key,)
            ).fetchone()
            return self._native_origin(row)

    def record_native_effect(self, operation_key: str, evidence_ref: str) -> Checkpoint:
        with self._connection(write=True) as connection:
            origin = connection.execute(
                "SELECT * FROM brokerrouter_native_origins WHERE operation_key=?", (operation_key,)
            ).fetchone()
            checkpoint = connection.execute(
                "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            self._native_checkpoint(origin, checkpoint)
            if (
                checkpoint["state"] != "dispatching" or origin["effect_recorded_at_ns"] is not None
                or not checkpoint["operation_id"] or not checkpoint["gateway_payload_hash"]
                or not checkpoint["claim_ciphertext"] or not evidence_ref
            ):
                raise CheckpointConflict("native effect has no authorized dispatch")
            self._open(operation_key, checkpoint["claim_ciphertext"])
            now = time.time_ns()
            connection.execute(
                "UPDATE brokerrouter_native_origins SET effect_recorded_at_ns=? WHERE operation_key=?",
                (now, operation_key),
            )
            connection.execute(
                "UPDATE brokerrouter_checkpoints SET state='effect_recorded',evidence_ref=?,updated_at=? WHERE operation_key=?",
                (evidence_ref, now, operation_key),
            )
        checkpoint = self.load(operation_key)
        if checkpoint is None:
            raise CheckpointConflict("native execution effect was not observable")
        return checkpoint

    def stage(
        self, operation_key: str, identity_hash: str, tool_name: str, payload_hash: str,
        *, model_binding_hash: str | None = None, native: bool = False,
    ) -> Checkpoint:
        now = time.time_ns()
        with self._connection(write=True) as connection:
            origin = connection.execute(
                "SELECT * FROM brokerrouter_native_origins WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if not native and origin is not None:
                raise CheckpointConflict("native execution cannot use a standalone dispatch")
            connection.execute(
                "INSERT OR IGNORE INTO brokerrouter_checkpoints(operation_key,identity_hash,tool_name,local_payload_hash,model_binding_hash,state,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?)",
                (operation_key, identity_hash, tool_name, payload_hash, model_binding_hash, "staged", now, now),
            )
            row = connection.execute(
                "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if model_binding_hash is not None:
                if row["model_binding_hash"] is None:
                    if row["state"] not in {"staged", "checking", "waiting_approval", "ready"}:
                        raise CheckpointConflict("legacy execution has no authenticated model binding")
                    # Recheck legacy ready input with the full body before allowing a
                    # claim. The gateway rejects a changed historical model/argument hash.
                    connection.execute(
                        "UPDATE brokerrouter_checkpoints SET model_binding_hash=?,state='staged',gateway_payload_hash=NULL,updated_at=? WHERE operation_key=?",
                        (model_binding_hash, now, operation_key),
                    )
                    row = connection.execute(
                        "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
                    ).fetchone()
                if row["model_binding_hash"] != model_binding_hash:
                    raise CheckpointConflict("execution model identity changed")
            if native:
                self._native_checkpoint(origin, row)
            checkpoint = self._row(row)
            assert checkpoint is not None
            if (
                checkpoint.identity_hash != identity_hash
                or checkpoint.tool_name != tool_name
                or checkpoint.local_payload_hash != payload_hash
            ):
                raise CheckpointConflict("operation identity was reused with different final arguments")
            return checkpoint

    def checked(
        self, operation_key: str, operation_id: str, state: str, gateway_payload_hash: str | None
    ) -> Checkpoint:
        if state not in {"checking", "waiting_approval", "ready", "rejected"}:
            raise CheckpointConflict("invalid check transition")
        with self._connection(write=True) as connection:
            row = connection.execute(
                "SELECT operation_id FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
            if row is None or (row[0] is not None and row[0] != operation_id):
                raise CheckpointConflict("gateway operation identity changed")
            connection.execute(
                "UPDATE brokerrouter_checkpoints SET operation_id=?,gateway_payload_hash=?,state=?,updated_at=? WHERE operation_key=? AND state IN ('staged','checking','waiting_approval','ready')",
                (operation_id, gateway_payload_hash, state, time.time_ns(), operation_key),
            )
        checkpoint = self.load(operation_key)
        if checkpoint is None or checkpoint.state != state:
            raise CheckpointConflict("check transition was not committed")
        return checkpoint

    def prepare_claim(self, operation_key: str, token: str) -> Checkpoint:
        with self._connection(write=True) as connection:
            changed = connection.execute(
                "UPDATE brokerrouter_checkpoints SET claim_ciphertext=?,state='claim_prepared',updated_at=? WHERE operation_key=? AND state='ready'",
                (self._seal(operation_key, token), time.time_ns(), operation_key),
            ).rowcount
            if changed != 1:
                raise CheckpointConflict("claim can only be prepared from ready")
        checkpoint = self.load(operation_key)
        assert checkpoint is not None
        return checkpoint

    def transition(self, operation_key: str, expected: set[str], state: str, evidence_ref: str | None = None) -> Checkpoint:
        if state not in STATES or not expected:
            raise CheckpointConflict("invalid checkpoint transition")
        placeholders = ",".join("?" for _ in expected)
        params = [state, evidence_ref, time.time_ns(), operation_key, *sorted(expected)]
        with self._connection(write=True) as connection:
            changed = connection.execute(
                f"UPDATE brokerrouter_checkpoints SET state=?,evidence_ref=COALESCE(?,evidence_ref),updated_at=? WHERE operation_key=? AND state IN ({placeholders})",
                params,
            ).rowcount
            if changed != 1:
                raise CheckpointConflict("checkpoint state changed concurrently")
        checkpoint = self.load(operation_key)
        assert checkpoint is not None
        return checkpoint

    def reserve_approval_notice(self, operation_key: str, approval_id: str) -> bool:
        now = time.time_ns()
        lease_until = now + 30_000_000_000
        with self._connection(write=True) as connection:
            connection.execute(
                "INSERT OR IGNORE INTO brokerrouter_approval_notices(operation_key,approval_id,state,updated_at) VALUES(?,?,'pending',?)",
                (operation_key, approval_id, now),
            )
            row = connection.execute(
                "SELECT approval_id FROM brokerrouter_approval_notices WHERE operation_key=?",
                (operation_key,),
            ).fetchone()
            if row is None:
                raise CheckpointConflict("approval notice was not staged")
            if row[0] != approval_id:
                connection.execute(
                    "UPDATE brokerrouter_approval_notices SET approval_id=?,state='pending',lease_until=NULL,updated_at=? WHERE operation_key=?",
                    (approval_id, now, operation_key),
                )
            changed = connection.execute(
                "UPDATE brokerrouter_approval_notices SET state='sending',lease_until=?,updated_at=? WHERE operation_key=? AND approval_id=? AND (state='pending' OR (state='sending' AND lease_until<?))",
                (lease_until, now, operation_key, approval_id, now),
            ).rowcount
            return changed == 1

    def complete_approval_notice(self, operation_key: str, approval_id: str) -> None:
        with self._connection(write=True) as connection:
            changed = connection.execute(
                "UPDATE brokerrouter_approval_notices SET state='sent',lease_until=NULL,updated_at=? WHERE operation_key=? AND approval_id=? AND state='sending'",
                (time.time_ns(), operation_key, approval_id),
            ).rowcount
            if changed != 1:
                raise CheckpointConflict("approval notice completion changed concurrently")

    def release_approval_notice(self, operation_key: str, approval_id: str) -> None:
        with self._connection(write=True) as connection:
            connection.execute(
                "UPDATE brokerrouter_approval_notices SET state='pending',lease_until=NULL,updated_at=? WHERE operation_key=? AND approval_id=? AND state='sending'",
                (time.time_ns(), operation_key, approval_id),
            )
