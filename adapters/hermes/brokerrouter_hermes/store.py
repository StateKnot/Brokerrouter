from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass, field
import os
from pathlib import Path
import sqlite3
import stat
import time
from typing import Iterator

from cryptography.hazmat.primitives.ciphers.aead import AESGCM


SCHEMA_VERSION = 2
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
    state: str
    claim_token: str | None = field(repr=False)
    evidence_ref: str | None


class CheckpointStore:
    def __init__(self, path: Path, key: bytes) -> None:
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
            if version not in (0, 1, SCHEMA_VERSION):
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

    def stage(self, operation_key: str, identity_hash: str, tool_name: str, payload_hash: str) -> Checkpoint:
        now = time.time_ns()
        with self._connection(write=True) as connection:
            connection.execute(
                "INSERT OR IGNORE INTO brokerrouter_checkpoints(operation_key,identity_hash,tool_name,local_payload_hash,state,created_at,updated_at) VALUES(?,?,?,?,?,?,?)",
                (operation_key, identity_hash, tool_name, payload_hash, "staged", now, now),
            )
            row = connection.execute(
                "SELECT * FROM brokerrouter_checkpoints WHERE operation_key=?", (operation_key,)
            ).fetchone()
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
