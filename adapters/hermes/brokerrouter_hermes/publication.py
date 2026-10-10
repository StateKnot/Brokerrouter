"""One-attempt native final delivery in Hermes' existing delivery ledger.

These are local send facts, not approval grants. The dedicated server final-text
record/release contract must run before this bracket in the complete output path.
Unknown platform effects are never retried or inferred to be successful.
"""
from __future__ import annotations

import asyncio
from contextlib import contextmanager
import hashlib
import json
import time
from typing import Any
from uuid import UUID

from .config import Config


PREFIX = "br-final-v1:"


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def _scope(config: Config) -> str:
    return hashlib.sha256(config.virtual_key.encode("utf-8")).hexdigest()


@contextmanager
def _transaction(ledger):
    # Reuse the exact state.db/schema and owner implementation. FULL and an
    # immediate transaction make the claim durable and serialize other processes.
    with ledger._DB_LOCK, ledger._transaction() as connection:
        connection.execute("PRAGMA synchronous=FULL")
        connection.execute("BEGIN IMMEDIATE")
        connection.execute(
            """CREATE TRIGGER IF NOT EXISTS brokerrouter_final_immutable
               BEFORE UPDATE ON delivery_obligations
               WHEN OLD.obligation_id LIKE 'br-final-v1:%' AND (
                 NEW.obligation_id IS NOT OLD.obligation_id OR
                 NEW.session_key IS NOT OLD.session_key OR NEW.platform IS NOT OLD.platform OR
                 NEW.chat_id IS NOT OLD.chat_id OR NEW.thread_id IS NOT OLD.thread_id OR
                 NEW.content IS NOT OLD.content OR NEW.adapter_profile IS NOT OLD.adapter_profile OR
                 NEW.created_at IS NOT OLD.created_at OR
                 (OLD.state!='br_pending' AND (NEW.owner_pid IS NOT OLD.owner_pid OR
                    NEW.owner_started_at IS NOT OLD.owner_started_at OR NEW.attempts IS NOT OLD.attempts)) OR
                 (OLD.state IN ('br_delivered','br_unknown','br_held') AND
                    (NEW.last_error IS NOT OLD.last_error OR NEW.updated_at IS NOT OLD.updated_at)) OR
                 NOT (NEW.state=OLD.state OR
                      (OLD.state='br_pending' AND NEW.state IN ('br_attempting','br_held')) OR
                      (OLD.state='br_attempting' AND NEW.state IN ('br_delivered','br_unknown'))))
               BEGIN SELECT RAISE(ABORT,'immutable governed final delivery'); END"""
        )
        connection.execute(
            """CREATE TRIGGER IF NOT EXISTS brokerrouter_final_no_replace
               BEFORE INSERT ON delivery_obligations
               WHEN NEW.obligation_id LIKE 'br-final-v1:%' AND EXISTS(
                 SELECT 1 FROM delivery_obligations WHERE obligation_id=NEW.obligation_id)
               BEGIN SELECT RAISE(ABORT,'governed final delivery cannot be replaced'); END"""
        )
        connection.execute(
            """CREATE TRIGGER IF NOT EXISTS brokerrouter_final_no_delete
               BEFORE DELETE ON delivery_obligations WHEN OLD.obligation_id LIKE 'br-final-v1:%'
               BEGIN SELECT RAISE(ABORT,'governed final delivery fact cannot be deleted'); END"""
        )
        yield connection


def _identity(config: Config, event: Any, session_key: str, text: str, adapter: Any,
              metadata: dict, reply_to: str | None):
    event_metadata = getattr(event, "metadata", None)
    binding = event_metadata.get("brokerrouter_turn") if isinstance(event_metadata, dict) else None
    if not isinstance(binding, dict) or set(binding) != {"turn_id", "run_id"}:
        raise ValueError("final delivery requires a turn identity")
    turn_id = str(UUID(str(binding["turn_id"])))
    run_id = binding["run_id"]
    message = getattr(event, "ledger_message_id", None)
    if message is None:
        message = getattr(event, "message_id", None)
    message = str(message or "")
    if not all(type(value) is str and value and len(value.encode("utf-8")) <= 500
               for value in (session_key, message, run_id)):
        raise ValueError("final delivery identity is incomplete")
    if type(text) is not str or len(_json({"text": text}).encode("utf-8")) > 512 * 1024:
        raise ValueError("final text exceeds supported wire size")
    encoded = text.encode("utf-8")
    identity = {"turn_id": turn_id, "run_id": run_id, "session_key": session_key, "message_id": message}
    scope = _scope(config)
    oid = PREFIX + scope + ":" + hashlib.sha256(_json(identity).encode("utf-8")).hexdigest()
    source = event.source
    platform = str(getattr(source.platform, "value", source.platform))
    chat = str(source.chat_id)
    thread = getattr(source, "thread_id", None)
    thread = str(thread) if thread is not None else None
    profile = str(getattr(adapter, "_owner_profile", None) or "default")
    # The ledger only needs immutable identity/hash/route and ACK observations.
    # It does not hold a second plaintext result that startup could send blindly.
    fact = _json({"format": "brokerrouter-final-sha256-v1", **identity,
                  "text_sha256": hashlib.sha256(encoded).hexdigest(), "bytes": len(encoded),
                  "reply_to": reply_to, "metadata_sha256": hashlib.sha256(_json(metadata).encode("utf-8")).hexdigest()})
    return oid, (session_key, platform, chat, thread, fact, profile)


def _claim(ledger, oid: str, values: tuple):
    now, (pid, started) = time.time(), ledger._owner_stamp()
    with _transaction(ledger) as connection:
        row = connection.execute(
            """SELECT session_key,platform,chat_id,thread_id,content,adapter_profile,state,last_error
               FROM delivery_obligations WHERE obligation_id=?""", (oid,)
        ).fetchone()
        if row is not None:
            if tuple(row[:6]) != values:
                raise ValueError("final delivery identity drifted")
            if row[6] == "br_delivered":
                ack = json.loads(row[7] or "{}")
                return "delivered", ack
            if row[6] != "br_pending":
                return "held", None
        else:
            connection.execute(
                """INSERT INTO delivery_obligations(
                   obligation_id,session_key,platform,chat_id,thread_id,content,adapter_profile,
                   state,attempts,created_at,updated_at,owner_pid,owner_started_at)
                   VALUES(?,?,?,?,?,?,?,'br_pending',0,?,?,?,?)""",
                (oid, *values, now, now, pid, started),
            )
        connection.execute(
            """UPDATE delivery_obligations SET state='br_attempting',attempts=1,
               updated_at=?,owner_pid=?,owner_started_at=? WHERE obligation_id=? AND state='br_pending'""",
            (now, pid, started, oid),
        )
    return "claimed", None


def _finish(ledger, oid: str, result: Any):
    success = result is not None and getattr(result, "success", None) is True
    raw = getattr(result, "raw_response", None)
    if isinstance(raw, dict) and raw.get("partial_overflow"):
        success = False
    ack = None
    if success:
        ids = (getattr(result, "message_id", None), *getattr(result, "continuation_message_ids", ()))
        if any(value is not None and (type(value) is not str or len(value.encode("utf-8")) > 500) for value in ids):
            success = False
        else:
            ack = _json({"message_id": ids[0], "continuation_message_ids": list(ids[1:])})
    pid, started = ledger._owner_stamp()
    with _transaction(ledger) as connection:
        cursor = connection.execute(
            """UPDATE delivery_obligations SET state=?,updated_at=?,last_error=?
               WHERE obligation_id=? AND state='br_attempting'
                 AND owner_pid IS ? AND owner_started_at IS ?""",
            ("br_delivered" if success else "br_unknown", time.time(), ack,
             oid, pid, started),
        )
        if cursor.rowcount != 1:
            raise ValueError("final delivery ACK did not match its claim")
    return success


async def send_final_once(adapter: Any, event: Any, session_key: str, text: str,
                          metadata: dict, *, reply_to: str | None, is_ephemeral_response: bool):
    from gateway.platforms.base import SendResult

    try:
        config = Config.from_env()
    except Exception:
        return SendResult(success=False, error="brokerrouter_delivery_configuration_invalid"), adapter
    if config is None:
        return None
    # Only the host's explicit ephemeral lane is outside final model delivery.
    # User prefixes cannot identify a control reply: /steer and /queue can turn
    # their slash-prefixed arguments into an ordinary model invocation.
    if is_ephemeral_response:
        return None
    delivery = adapter._final_delivery_adapter(event.source)
    from gateway import delivery_ledger as ledger
    try:
        metadata_wire = _json(metadata)
        if type(metadata) is not dict or len(metadata_wire.encode("utf-8")) > 64 * 1024:
            raise ValueError("unsupported final delivery metadata")
        metadata = json.loads(metadata_wire)
        oid, values = _identity(config, event, session_key, text, delivery, metadata, reply_to)
        state, ack = await asyncio.to_thread(_claim, ledger, oid, values)
    except Exception:
        return SendResult(success=False, error="brokerrouter_delivery_fact_unavailable"), delivery
    if state == "delivered":
        return SendResult(success=True, message_id=ack.get("message_id"),
                          continuation_message_ids=tuple(ack.get("continuation_message_ids", ())),
                          raw_response={"brokerrouter_already_delivered": True}), delivery
    if state != "claimed":
        return SendResult(success=False, error="brokerrouter_delivery_unknown_or_held"), delivery
    try:
        # One platform call. Upstream retry/fallback can duplicate an accepted
        # chunk or change approved bytes and is intentionally outside this lane.
        result = await delivery.send(chat_id=values[2], content=text,
                                     reply_to=reply_to, metadata=metadata)
    except BaseException as exc:
        try:
            await asyncio.shield(asyncio.to_thread(_finish, ledger, oid, None))
        except Exception:
            pass  # durable attempting itself is an unknown send on restart
        if not isinstance(exc, Exception):
            raise
        return SendResult(success=False, error="brokerrouter_delivery_unknown_or_held"), delivery
    try:
        success = await asyncio.to_thread(_finish, ledger, oid, result)
    except Exception:
        success = False
    if not success:
        return SendResult(success=False, error="brokerrouter_delivery_unknown_or_held"), delivery
    return result, delivery


def hold_required_recovery(ledger) -> bool:
    """Stop unapproved/ambiguous replay; never clear a newer session by old key.

    Returns whether the caller is required-mode. Off still cannot sweep purpose
    states. The final server contract will later permit exact publication recovery
    after a current POST release and session message/run/generation CAS.
    """
    config = Config.from_env()
    if config is None:
        return False
    prefix = PREFIX + _scope(config) + ":%"
    with _transaction(ledger) as connection:
        rows = connection.execute(
            """SELECT obligation_id,state,owner_pid,owner_started_at FROM delivery_obligations
               WHERE obligation_id LIKE ? AND state IN ('br_pending','br_attempting')""", (prefix,)
        ).fetchall()
        for oid, state, owner_pid, owner_started in rows:
            if not ledger._owner_alive(owner_pid, owner_started):
                connection.execute(
                    "UPDATE delivery_obligations SET state=?,updated_at=? WHERE obligation_id=? AND state=?",
                    ("br_unknown" if state == "br_attempting" else "br_held", time.time(), oid, state),
                )
        connection.execute(
            """UPDATE delivery_obligations SET state='br_legacy_held',updated_at=?
               WHERE obligation_id NOT LIKE 'br-final-v1:%' AND state IN ('pending','attempting','failed')""",
            (time.time(),),
        )
    return True
