"""Dedicated final text quarantine/release; local observations are never grants."""
from __future__ import annotations

from contextlib import contextmanager
import asyncio
from dataclasses import asdict, dataclass
from datetime import datetime
import hashlib
import json
import os
import time
from typing import Any
from uuid import UUID, uuid4

from cryptography.hazmat.primitives.ciphers.aead import AESGCM

from .client import GatewayFailure, TransportFailure
from .config import Config
from .gate import GovernanceHalt
from .model_transport import ModelOrigin, _origin, _origin_json, _transaction as model_transaction, source_requests


MAX_WIRE_BYTES = 512 * 1024
MAX_WORK_BYTES = 2 * 1024 * 1024
OWNER_KEY = "brokerrouter_final_owner_v1"
NATIVE_OWNER_KEY = "brokerrouter_final_native_owner_v1"


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def _sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def required() -> bool:
    try:
        return Config.from_env() is not None
    except Exception as exc:
        raise GovernanceHalt("final_configuration_invalid") from exc


def _uuid(value: Any) -> str:
    try:
        if type(value) is not str or str(UUID(value)) != value:
            raise ValueError("uuid")
        return value
    except Exception as exc:
        raise GovernanceHalt("final_identity_invalid") from exc


def _expiry(value: Any) -> int:
    try:
        if type(value) is not str or len(value) > 60:
            raise ValueError("expiry")
        stamp = datetime.fromisoformat(value.replace("Z", "+00:00"))
        if stamp.tzinfo is None:
            raise ValueError("expiry")
        expiry = int(stamp.timestamp() * 1_000_000_000)
        if expiry <= time.time_ns():
            raise ValueError("expired")
        return expiry
    except Exception as exc:
        raise GovernanceHalt("final_result_expired") from exc


@dataclass(frozen=True, repr=False)
class ReleasedFinal:
    origin: ModelOrigin
    publication_id: str
    output_operation_id: str
    text_sha256: str
    text: str
    expires_at_ns: int


@contextmanager
def _transaction():
    with model_transaction() as connection:
        connection.execute("""CREATE TABLE IF NOT EXISTS brokerrouter_final_candidates(
            origin TEXT PRIMARY KEY, model TEXT NOT NULL, session_key TEXT NOT NULL,
            owner_token TEXT NOT NULL, owner_pid INTEGER NOT NULL, owner_started_at INTEGER,
            sources TEXT, text_sha256 TEXT, bytes INTEGER,
            result_ciphertext BLOB, created_at_ns INTEGER NOT NULL, expires_at_ns INTEGER NOT NULL,
            publication_id TEXT, output_operation_id TEXT, payload_hash TEXT, remote_expiry TEXT,
            work_sequence INTEGER NOT NULL DEFAULT 0, work_hash TEXT, work_ciphertext BLOB)""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_candidate_immutable
            BEFORE UPDATE ON brokerrouter_final_candidates WHEN
            NEW.origin IS NOT OLD.origin OR NEW.model IS NOT OLD.model OR
            NEW.session_key IS NOT OLD.session_key OR NEW.created_at_ns IS NOT OLD.created_at_ns OR
            NEW.expires_at_ns IS NOT OLD.expires_at_ns OR
            (OLD.text_sha256 IS NOT NULL AND (NEW.sources IS NOT OLD.sources OR
                NEW.text_sha256 IS NOT OLD.text_sha256 OR NEW.bytes IS NOT OLD.bytes OR
                (NEW.result_ciphertext IS NOT NULL AND NEW.result_ciphertext IS NOT OLD.result_ciphertext))) OR
            (OLD.publication_id IS NOT NULL AND NEW.publication_id IS NOT OLD.publication_id) OR
            (OLD.output_operation_id IS NOT NULL AND NEW.output_operation_id IS NOT OLD.output_operation_id) OR
            (OLD.payload_hash IS NOT NULL AND NEW.payload_hash IS NOT OLD.payload_hash) OR
            (OLD.remote_expiry IS NOT NULL AND NEW.remote_expiry IS NOT OLD.remote_expiry) OR
            (OLD.text_sha256 IS NOT NULL AND (NEW.work_sequence IS NOT OLD.work_sequence OR
                NEW.work_hash IS NOT OLD.work_hash OR
                (NEW.work_ciphertext IS NOT NULL AND NEW.work_ciphertext IS NOT OLD.work_ciphertext) OR
                NEW.owner_token IS NOT OLD.owner_token OR NEW.owner_pid IS NOT OLD.owner_pid OR
                NEW.owner_started_at IS NOT OLD.owner_started_at)) OR
            NEW.work_sequence < OLD.work_sequence
            BEGIN SELECT RAISE(ABORT,'immutable final candidate'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_candidate_no_replace
            BEFORE INSERT ON brokerrouter_final_candidates WHEN EXISTS(
                SELECT 1 FROM brokerrouter_final_candidates WHERE origin=NEW.origin)
            BEGIN SELECT RAISE(ABORT,'final candidate cannot be replaced'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_candidate_no_delete
            BEFORE DELETE ON brokerrouter_final_candidates
            BEGIN SELECT RAISE(ABORT,'final candidate cannot be deleted'); END""")
        connection.execute("""CREATE TABLE IF NOT EXISTS brokerrouter_final_notices(
            origin TEXT NOT NULL,approval_id TEXT NOT NULL,created_at_ns INTEGER NOT NULL,
            PRIMARY KEY(origin,approval_id))""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_notice_immutable
            BEFORE UPDATE ON brokerrouter_final_notices
            BEGIN SELECT RAISE(ABORT,'immutable final notice'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_notice_no_replace
            BEFORE INSERT ON brokerrouter_final_notices WHEN EXISTS(
                SELECT 1 FROM brokerrouter_final_notices WHERE origin=NEW.origin AND approval_id=NEW.approval_id)
            BEGIN SELECT RAISE(ABORT,'final notice cannot be replaced'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_notice_no_delete
            BEFORE DELETE ON brokerrouter_final_notices
            BEGIN SELECT RAISE(ABORT,'final notice cannot be deleted'); END""")
        # Keep immutable execution/source/hash facts; expiry removes only bytes.
        connection.execute("""UPDATE brokerrouter_final_candidates SET result_ciphertext=NULL,work_ciphertext=NULL
            WHERE expires_at_ns<=? AND (result_ciphertext IS NOT NULL OR work_ciphertext IS NOT NULL)""", (time.time_ns(),))
        yield connection


def _row(connection, origin: ModelOrigin):
    cursor = connection.execute("SELECT * FROM brokerrouter_final_candidates WHERE origin=?", (_origin_json(origin),))
    row = cursor.fetchone()
    return dict(zip((item[0] for item in cursor.description), row)) if row is not None else None


def _owner(origin: ModelOrigin) -> dict:
    return asdict(origin)


def bind_resume_owner(store: Any, session_key: str, origin: ModelOrigin, native_token: str, current) -> None:
    """Persist a verified bind candidate before changing the routing entry in memory."""
    with store._lock:
        entry = store._entry_locked(session_key)
        if (entry is None or entry.session_id != origin.session_id or type(native_token) is not str
                or not native_token or entry.active_turn_token != native_token or not current()):
            raise GovernanceHalt("final_routing_session_changed")
        candidate = entry.to_dict()
        candidate["metadata"] = {**entry.metadata, OWNER_KEY: _owner(origin), NATIVE_OWNER_KEY:native_token}
        store._save_entry(session_key, entry_data=candidate, lock_held=True)
        entry.metadata = candidate["metadata"]


def _routing_ok(entry: Any, origin: ModelOrigin, agent: Any) -> bool:
    token = getattr(agent,"_brokerrouter_final_native_owner",None)
    return (entry is not None and entry.session_id == origin.session_id
            and entry.metadata.get(OWNER_KEY) == _owner(origin)
            and type(token) is str and token and entry.metadata.get(NATIVE_OWNER_KEY) == token
            and entry.active_turn_token in (None,token)
            and getattr(agent,"_brokerrouter_final_current",lambda:False)())


def clear_resume_owner(store: Any, session_key: str, released: ReleasedFinal, agent: Any) -> bool:
    """An ACK may clear only the exact verified/released routing owner."""
    config = Config.from_env()
    if config is None or not isinstance(released, ReleasedFinal):
        return False
    if released.origin.scope != _sha(config.virtual_key.encode("utf-8")):
        return False
    with store._lock:
        entry = store._entry_locked(session_key)
        if not _routing_ok(entry,released.origin,agent) or not entry.resume_pending:
            return False
        with _transaction() as connection:
            row = _row(connection, released.origin)
            if (row is None or row["publication_id"] != released.publication_id or row["session_key"] != session_key
                    or row["text_sha256"] != released.text_sha256):
                return False
        candidate = entry.to_dict()
        candidate.update(resume_pending=False, resume_reason=None, last_resume_marked_at=None)
        store._save_entry(session_key, entry_data=candidate, lock_held=True)
        entry.resume_pending = False
        entry.resume_reason = None
        entry.last_resume_marked_at = None
        return True


def prepare(agent: Any, context: Any, store: Any, runner: Any) -> ReleasedFinal | None:
    """After true bind, before any model/history/middleware, recover or own this turn."""
    config = Config.from_env()
    if config is None:
        return None
    origin = _origin(agent, config)
    # Required history comes only from ordinary released storage, never a held
    # pooled Agent's previous working transcript.
    agent._session_messages = None
    agent._brokerrouter_released_final = None
    for name in ("stream_delta_callback", "_stream_callback", "reasoning_callback", "interim_assistant_callback",
                 "tool_progress_callback", "tool_start_callback", "tool_complete_callback", "step_callback",
                 "event_callback", "status_callback", "notice_callback", "clarify_callback", "thinking_callback",
                 "background_review_callback", "_on_session_title"):
        setattr(agent, name, None)
    agent.verbose_logging = False
    agent.save_trajectories = False
    agent.thinking_progress = False
    agent.memory_notifications = "off"
    session_key = context.session_key
    if type(session_key) is not str or not session_key or len(session_key.encode("utf-8")) > 500:
        raise GovernanceHalt("final_routing_identity_invalid")
    native_token = getattr(context,"_brokerrouter_native_turn_owner",None)
    owner_key = getattr(context,"_brokerrouter_run_owner_key",None)
    if type(owner_key) is not str or not owner_key:
        raise GovernanceHalt("final_native_run_owner_missing")
    current = lambda: runner._is_session_run_current(owner_key,origin.generation)
    agent._brokerrouter_final_current = current
    from gateway import delivery_ledger as ledger
    pid, started = ledger._owner_stamp()
    token = uuid4().hex
    now = time.time_ns()
    with _transaction() as connection:
        row = _row(connection, origin)
        if row is None:
            connection.execute("""INSERT INTO brokerrouter_final_candidates(origin,model,session_key,owner_token,
                owner_pid,owner_started_at,created_at_ns,expires_at_ns) VALUES(?,?,?,?,?,?,?,?)""",
                (_origin_json(origin), origin.model, session_key, token, pid, started, now, now + 3600_000_000_000))
        else:
            if row["model"] != origin.model or row["session_key"] != session_key or row["expires_at_ns"] <= now:
                raise GovernanceHalt("final_origin_changed_or_expired")
            if row["text_sha256"] is None:
                effect = connection.execute("SELECT 1 FROM brokerrouter_model_calls WHERE origin=? LIMIT 1",
                                            (_origin_json(origin),)).fetchone()
                if effect or ledger._owner_alive(row["owner_pid"], row["owner_started_at"]):
                    raise GovernanceHalt("final_result_unavailable_after_restart")
                connection.execute("UPDATE brokerrouter_final_candidates SET owner_token=?,owner_pid=?,owner_started_at=? WHERE origin=?",
                                   (token, pid, started, _origin_json(origin)))
    agent._brokerrouter_final_owner_token = token
    bind_resume_owner(store,session_key,origin,native_token,current)
    agent._brokerrouter_final_native_owner = native_token
    if row is not None and row["text_sha256"] is not None:
        return release(agent)
    return None


def ordinary_pending(agent: Any) -> bool:
    # Even after release, a generic helper cannot authorize some other transcript
    # held by this Agent. Only append_ready writes the precise frozen projection.
    return required()


def _working_cipher(config: Config, origin: ModelOrigin, row: dict, messages: list) -> tuple:
    clear = _json(messages).encode("utf-8")
    if len(clear) > MAX_WORK_BYTES:
        raise GovernanceHalt("final_private_checkpoint_unsupported")
    sequence = row["work_sequence"] + 1
    digest = _sha(clear)
    aad = b"brokerrouter-hermes/final-working/v1\0" + _json({"origin":_owner(origin),
        "sequence":sequence,"hash":digest,"expires":row["expires_at_ns"]}).encode("utf-8")
    nonce = os.urandom(12)
    return sequence,digest,nonce + AESGCM(config.state_key).encrypt(nonce,clear,aad)


def hold_working(agent: Any, messages: list) -> bool | None:
    """A bounded encrypted private checkpoint, never ordinary history or a grant."""
    config = Config.from_env()
    if getattr(agent,"_brokerrouter_model_origin",None) is None:
        # Tool-only contract/native callers do not possess primary final sources.
        # They cannot publish an ordinary transcript or assert durable final ACK.
        return None
    origin = _origin(agent, config)
    with _transaction() as connection:
        row = _row(connection, origin)
        # A direct native Agent with no gateway publication controller still
        # cannot persist raw body to ordinary DB. It does not claim a durable
        # publication checkpoint; the real gateway must call prepare first.
        if row is None:
            return None
        if row["owner_token"] != getattr(agent, "_brokerrouter_final_owner_token", None):
            raise GovernanceHalt("final_private_owner_changed")
        if row["text_sha256"] is not None or row["expires_at_ns"] <= time.time_ns():
            raise GovernanceHalt("final_private_checkpoint_closed")
        sequence,digest,encrypted = _working_cipher(config,origin,row,messages)
        connection.execute("UPDATE brokerrouter_final_candidates SET work_sequence=?,work_hash=?,work_ciphertext=? WHERE origin=?",
                           (sequence, digest, encrypted, _origin_json(origin)))
    return True


def note_return(agent: Any, result: dict) -> None:
    if required():
        if result.get("failed") or result.get("interrupted") or result.get("completed") is not True:
            raise GovernanceHalt("final_native_result_incomplete")
        agent._brokerrouter_final_return_token = getattr(agent, "_brokerrouter_final_owner_token", None)


def _aad(origin: ModelOrigin, row: dict) -> bytes:
    return b"brokerrouter-hermes/final-text/v1\0" + _json({"origin":_owner(origin),"sources":row["sources"],
        "text_sha256":row["text_sha256"],"bytes":row["bytes"],"created":row["created_at_ns"],
        "expires":row["expires_at_ns"]}).encode("utf-8")


def freeze(agent: Any, text: str, messages: list) -> ReleasedFinal:
    config = Config.from_env()
    origin = _origin(agent, config)
    sources = tuple(sorted(source_requests(agent)))
    body = {"turn_id":origin.turn_id,"model_id":origin.model,"source_request_ids":sources,"text":text}
    if type(text) is not str or len(_json(body).encode("utf-8")) > MAX_WIRE_BYTES:
        raise GovernanceHalt("final_text_wire_unsupported")
    clear = text.encode("utf-8")
    digest = _sha(clear)
    with _transaction() as connection:
        row = _row(connection, origin)
        if row is None or row["expires_at_ns"] <= time.time_ns():
            raise GovernanceHalt("final_private_checkpoint_missing")
        if row["text_sha256"] is not None:
            if row["text_sha256"] != digest or row["bytes"] != len(clear) or row["sources"] != _json(sources):
                raise GovernanceHalt("final_frozen_text_changed")
        else:
            if (not getattr(agent, "_brokerrouter_final_owner_token", None)
                    or row["owner_token"] != agent._brokerrouter_final_owner_token
                    or getattr(agent, "_brokerrouter_final_return_token", None) != row["owner_token"]):
                raise GovernanceHalt("final_freeze_requires_known_native_return")
            row.update(text_sha256=digest,bytes=len(clear),sources=_json(sources))
            sequence,work_hash,work_cipher = _working_cipher(config,origin,row,messages)
            nonce = os.urandom(12)
            encrypted = nonce + AESGCM(config.state_key).encrypt(nonce, clear, _aad(origin,row))
            connection.execute("""UPDATE brokerrouter_final_candidates SET sources=?,text_sha256=?,bytes=?,result_ciphertext=?,
                work_sequence=?,work_hash=?,work_ciphertext=? WHERE origin=?""",
                (row["sources"],digest,len(clear),encrypted,sequence,work_hash,work_cipher,_origin_json(origin)))
    return release(agent)


def _open(config: Config, origin: ModelOrigin, row: dict) -> str:
    encrypted = row["result_ciphertext"]
    if (row["expires_at_ns"] <= time.time_ns() or type(encrypted) is not bytes
            or not 28 <= len(encrypted) <= MAX_WIRE_BYTES + 28):
        raise GovernanceHalt("final_private_result_unavailable")
    try:
        clear = AESGCM(config.state_key).decrypt(encrypted[:12],encrypted[12:],_aad(origin,row))
        if len(clear) != row["bytes"] or _sha(clear) != row["text_sha256"]:
            raise ValueError("hash")
        return clear.decode("utf-8")
    except Exception as exc:
        raise GovernanceHalt("final_private_result_invalid") from exc


def _final_messages(agent: Any) -> list:
    config = Config.from_env()
    origin = _origin(agent,config)
    with _transaction() as connection:
        row = _row(connection,origin)
    try:
        if row is None or row["text_sha256"] is None or row["expires_at_ns"] <= time.time_ns():
            raise ValueError("missing")
        encrypted = row["work_ciphertext"]
        if type(encrypted) is not bytes or not 28 <= len(encrypted) <= MAX_WORK_BYTES + 28:
            raise ValueError("length")
        aad = b"brokerrouter-hermes/final-working/v1\0" + _json({"origin":_owner(origin),
            "sequence":row["work_sequence"],"hash":row["work_hash"],"expires":row["expires_at_ns"]}).encode("utf-8")
        clear = AESGCM(config.state_key).decrypt(encrypted[:12],encrypted[12:],aad)
        if _sha(clear) != row["work_hash"]:
            raise ValueError("hash")
        messages = json.loads(clear)
        if type(messages) is not list:
            raise ValueError("messages")
        return messages
    except Exception as exc:
        raise GovernanceHalt("final_private_history_invalid") from exc


def _metadata(origin: ModelOrigin, row: dict, response: dict) -> None:
    if (response.get("turn_id") != origin.turn_id or response.get("model_id") != origin.model
            or response.get("source_request_ids") != json.loads(row["sources"])
            or response.get("text_sha256") != row["text_sha256"]
            or type(response.get("bytes")) is not int or response["bytes"] != row["bytes"]):
        raise GovernanceHalt("final_gateway_metadata_invalid")
    _expiry(response.get("expires_at"))
    if row["remote_expiry"] is not None and row["remote_expiry"] != response["expires_at"]:
        raise GovernanceHalt("final_gateway_expiry_changed")


def release(agent: Any) -> ReleasedFinal:
    config = Config.from_env()
    origin = _origin(agent,config)
    with _transaction() as connection:
        row = _row(connection,origin)
    if row is None:
        raise GovernanceHalt("final_private_checkpoint_missing")
    text = _open(config,origin,row)
    transport = agent._brokerrouter_governance_gate.client._transport
    try:
        record = transport.request("POST","/v1/governance/final-publications",{
            "turn_id":origin.turn_id,"model_id":origin.model,"source_request_ids":json.loads(row["sources"]),"text":text})
        _metadata(origin,row,record)
        publication_id = _uuid(record.get("id"))
        if record.get("state") != "recorded" or "text" in record:
            raise GovernanceHalt("final_gateway_record_invalid")
        if row["publication_id"] is not None and row["publication_id"] != publication_id:
            raise GovernanceHalt("final_gateway_publication_changed")
        with _transaction() as connection:
            connection.execute("UPDATE brokerrouter_final_candidates SET publication_id=?,remote_expiry=? WHERE origin=?",
                               (publication_id,record["expires_at"],_origin_json(origin)))
        row.update(publication_id=publication_id,remote_expiry=record["expires_at"])
    except (GatewayFailure,TransportFailure) as exc:
        raise GovernanceHalt("final_record_not_persisted") from exc
    gate = agent._brokerrouter_governance_gate
    deadline = gate._monotonic() + config.approval_wait_seconds
    while gate._monotonic() < deadline:
        try:
            response = transport.request("POST",f"/v1/governance/final-publications/{publication_id}/release",{})
        except (GatewayFailure,TransportFailure) as exc:
            raise GovernanceHalt("final_release_unavailable") from exc
        _metadata(origin,row,response)
        output_id = _uuid(response.get("id"))
        payload = {"publication_id":publication_id,"source_request_ids":json.loads(row["sources"]),
            "session_id":origin.session_id,"message_id":origin.message_id,"run_id":origin.run_id,
            "generation":origin.generation,"text":text}
        expected_hash = _sha(_json({"turn":origin.turn_id,"model":origin.model,"tool":None,"payload":payload}).encode("utf-8"))
        if (response.get("publication_id") != publication_id or response.get("output_operation_id") != output_id
                or response.get("payload_hash") != expected_hash):
            raise GovernanceHalt("final_gateway_output_identity_invalid")
        if row["output_operation_id"] is not None and row["output_operation_id"] != output_id:
            raise GovernanceHalt("final_gateway_output_changed")
        state = response.get("state")
        if state not in {"ready","checking","waiting_approval"} or (state != "ready" and "text" in response):
            raise GovernanceHalt("final_gateway_state_invalid")
        with _transaction() as connection:
            connection.execute("UPDATE brokerrouter_final_candidates SET output_operation_id=?,payload_hash=? WHERE origin=?",
                               (output_id,expected_hash,_origin_json(origin)))
        row.update(output_operation_id=output_id,payload_hash=expected_hash)
        if state == "ready":
            if type(response.get("text")) is not str or response["text"] != text:
                raise GovernanceHalt("final_gateway_result_changed")
            released = ReleasedFinal(origin,publication_id,output_id,row["text_sha256"],text,
                                    min(row["expires_at_ns"],_expiry(response["expires_at"])))
            agent._brokerrouter_released_final = released
            return released
        if state == "checking":
            evaluations = response.get("evaluations")
            if type(evaluations) is not list or not 1 <= len(evaluations) <= 128:
                raise GovernanceHalt("final_gateway_evaluations_invalid")
            for evaluation in evaluations:
                _uuid(evaluation)
        else:
            approval = response.get("approval")
            if (type(approval) is not dict or approval.get("operation_id") != output_id
                    or approval.get("turn_id") != origin.turn_id or approval.get("state") != "pending"
                    or approval.get("scope") != "current_turn_only" or approval.get("resume_requires_recheck") is not True
                    or approval.get("options") != ["continue_this_turn","reject","feedback"]):
                raise GovernanceHalt("final_gateway_approval_invalid")
            approval_id = _uuid(approval.get("id"))
            callback = getattr(agent,"_brokerrouter_approval_callback",None)
            if callback is None:
                raise GovernanceHalt("final_approval_channel_unavailable")
            with _transaction() as connection:
                noticed = connection.execute("SELECT 1 FROM brokerrouter_final_notices WHERE origin=? AND approval_id=?",
                                             (_origin_json(origin),approval_id)).fetchone()
                if noticed is None:
                    connection.execute("INSERT INTO brokerrouter_final_notices VALUES(?,?,?)",(_origin_json(origin),approval_id,time.time_ns()))
            if noticed is None:
                try:
                    callback({"approval_id":approval_id,"turn_id":origin.turn_id,"review_url":f"{config.approvals_url}/#approvals"})
                except Exception as exc:
                    raise GovernanceHalt("final_approval_notice_unavailable") from exc
        with gate._human_wait():
            gate._sleep(min(config.approval_poll_seconds,max(0,deadline-gate._monotonic())))
    raise GovernanceHalt("final_approval_wait_timeout")


def safe_messages(messages: list, released: ReleasedFinal) -> list:
    """Ordinary history keeps released tool facts and exact final, not scratch text."""
    projected = []
    for item in messages:
        if type(item) is not dict or item.get("role") not in {"user","assistant","tool"}:
            continue
        row = {name:value for name,value in item.items() if name in
               {"role","content","tool_calls","tool_call_id","name","tool_name","platform_message_id"}}
        if row["role"] == "assistant":
            if not row.get("tool_calls"):
                continue
            row["content"] = ""
        projected.append(json.loads(_json(row)))
    projected.append({"role":"assistant","content":released.text,"finish_reason":"stop"})
    return projected


def _history_rows(connection, publication_id: str) -> list:
    return [tuple(row) for row in connection.execute("""SELECT id,session_id,role,content,tool_calls,tool_call_id,
        tool_name,finish_reason,platform_message_id FROM messages
        WHERE json_extract(display_metadata,'$.brokerrouter_publication_id')=? ORDER BY id""", (publication_id,))]


def _valid_history(connection, released: ReleasedFinal, prior) -> bool:
    identity = (released.origin.scope,_origin_json(released.origin),released.origin.session_id,released.text_sha256)
    rows = _history_rows(connection,released.publication_id)
    return (prior is not None and tuple(prior[:4]) == identity and rows and rows[-1][0] == prior[5]
            and all(row[1] == released.origin.session_id for row in rows)
            and rows[-1][2] == "assistant" and type(rows[-1][3]) is str
            and rows[-1][3] == released.text
            and _sha(_json(rows).encode("utf-8")) == prior[4])


def append_ready(agent: Any, store: Any, session_key: str, released: ReleasedFinal) -> list:
    """Use the native message writer transaction for a durable final append cursor."""
    from hermes_state_messages import _INSERT_MESSAGE_SQL, _parse_tool_calls
    current = release(agent)
    if current != released:
        raise GovernanceHalt("final_history_release_changed")
    origin = released.origin
    projected = safe_messages(_final_messages(agent),released)
    db = store._db_for_session_id(origin.session_id)
    with store._lock:
        entry = store._entry_locked(session_key)
        if not _routing_ok(entry,origin,agent):
            raise GovernanceHalt("final_routing_owner_changed")
        if db is None:
            raise GovernanceHalt("final_history_database_unavailable")
        stamp = time.time()
        params = [db._message_row_params(origin.session_id,msg["role"],{
                    **msg,"display_metadata":db._encode_display_metadata({"brokerrouter_publication_id":released.publication_id})},
                    _parse_tool_calls(msg.get("tool_calls")),stamp,keep_reasoning=False) for msg in projected]

        def write(connection):
            connection.execute("""CREATE TABLE IF NOT EXISTS brokerrouter_final_history(
                publication_id TEXT PRIMARY KEY,scope TEXT NOT NULL,origin TEXT NOT NULL,session_id TEXT NOT NULL,
                text_sha256 TEXT NOT NULL,projection_sha256 TEXT NOT NULL,last_message_id INTEGER NOT NULL)""")
            connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_history_immutable
                BEFORE UPDATE ON brokerrouter_final_history
                BEGIN SELECT RAISE(ABORT,'immutable final history cursor'); END""")
            connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_history_no_replace
                BEFORE INSERT ON brokerrouter_final_history WHEN EXISTS(
                    SELECT 1 FROM brokerrouter_final_history WHERE publication_id=NEW.publication_id)
                BEGIN SELECT RAISE(ABORT,'final history cursor cannot be replaced'); END""")
            connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_final_history_no_delete
                BEFORE DELETE ON brokerrouter_final_history
                BEGIN SELECT RAISE(ABORT,'final history cursor cannot be deleted'); END""")
            identity = (origin.scope,_origin_json(origin),origin.session_id,released.text_sha256)
            prior = connection.execute("SELECT scope,origin,session_id,text_sha256,projection_sha256,last_message_id FROM brokerrouter_final_history WHERE publication_id=?",
                                       (released.publication_id,)).fetchone()
            if prior is not None:
                if not _valid_history(connection,released,prior):
                    raise GovernanceHalt("final_history_identity_changed")
                return
            # Native main-turn lease has ended. A concurrent new turn must not
            # accept an old append, even if the routing entry hasn't changed yet.
            db._check_transcript_write_guards(connection,origin.session_id,None,reject_active_turn_lease=True)
            last_id = 0
            for values in params:
                last_id = connection.execute(_INSERT_MESSAGE_SQL,values).lastrowid
            db._bump_session_counters(connection,origin.session_id,len(params),
                sum(len(msg.get("tool_calls") or []) for msg in projected),unit=False)
            rows = _history_rows(connection,released.publication_id)
            connection.execute("INSERT INTO brokerrouter_final_history VALUES(?,?,?,?,?,?,?)",
                               (released.publication_id,*identity,_sha(_json(rows).encode("utf-8")),last_id))

        db._execute_write(write,patience_s=db._TRANSCRIPT_WRITE_PATIENCE_S)
    return projected


def history_is_ready(store: Any, session_key: str, released: ReleasedFinal) -> bool:
    db = store._db_for_session_id(released.origin.session_id)
    with store._lock:
        entry = store._entry_locked(session_key)
        if (entry is None or entry.session_id != released.origin.session_id
                or entry.metadata.get(OWNER_KEY) != _owner(released.origin)):
            return False
        if db is None:
            return False
        with db._read_ctx() as connection:
            try:
                row = connection.execute("SELECT scope,origin,session_id,text_sha256,projection_sha256,last_message_id FROM brokerrouter_final_history WHERE publication_id=?",
                                         (released.publication_id,)).fetchone()
                return bool(_valid_history(connection,released,row))
            except Exception:
                return False


async def finalize_for_gateway(runner: Any, event: Any, session_key: str, owner_key: str, run_generation: int,
                               result: dict, text: str) -> tuple[str,list]:
    """The fixed outer final boundary; ordinary observers follow this return."""
    agent = result.get("_brokerrouter_final_agent")
    if agent is None or not runner._is_session_run_current(owner_key,run_generation):
        raise GovernanceHalt("final_controller_or_generation_missing")
    origin = _origin(agent,Config.from_env())
    if origin.generation != run_generation:
        raise GovernanceHalt("final_generation_changed")
    if result.get("_brokerrouter_final_restored") is True:
        released = await asyncio.to_thread(release,agent)
    else:
        offset = result.get("history_offset")
        messages = result.get("messages")
        if type(offset) is not int or offset < 0 or type(messages) is not list or offset > len(messages):
            raise GovernanceHalt("final_native_history_invalid")
        released = await asyncio.to_thread(freeze,agent,text,messages[offset:])
    if released.text != text or not runner._is_session_run_current(owner_key,run_generation):
        raise GovernanceHalt("final_run_changed_during_release")
    projected = await asyncio.to_thread(append_ready,agent,runner.session_store,session_key,released)
    if not runner._is_session_run_current(owner_key,run_generation):
        raise GovernanceHalt("final_run_changed_after_history")
    event._brokerrouter_final_agent = agent
    # The next native turn reloads this committed ordinary projection. Its own
    # fresh prepare clears any private pooled working snapshot first.
    agent._session_messages = None
    result["messages"] = projected
    result.pop("last_reasoning",None)
    result.pop("_pre_transform_response",None)
    result["already_sent"] = False
    result["agent_persisted"] = True
    return released.text,projected


def authorize_send(adapter: Any, event: Any, session_key: str, text: str) -> ReleasedFinal | None:
    """The actual sender requires a fresh service release and native history fact."""
    if not required():
        return None
    agent = getattr(event,"_brokerrouter_final_agent",None)
    if agent is None:
        raise GovernanceHalt("final_sender_controller_missing")
    released = release(agent)
    binding = getattr(event,"metadata",None)
    binding = binding.get("brokerrouter_turn") if type(binding) is dict else None
    message = str(getattr(event,"message_id",None) or "")
    store = getattr(adapter,"_session_store",None)
    if store is not None:
        with store._lock:
            entry = store._entry_locked(session_key)
            source = getattr(event,"source",None)
            expected = getattr(entry,"origin",None)
            names = ("platform","chat_id","thread_id","profile","scope_id")
            route_ok = (expected is not None and source is not None and
                        all(getattr(expected,name,None)==getattr(source,name,None) for name in names)
                        and _routing_ok(entry,released.origin,agent))
    else:
        route_ok = False
    if (type(binding) is not dict or set(binding) != {"turn_id","run_id"}
            or binding["turn_id"] != released.origin.turn_id or binding["run_id"] != released.origin.run_id
            or message != released.origin.message_id or type(text) is not str or text != released.text
            or not getattr(agent,"_brokerrouter_final_current",lambda:False)()
            or not route_ok or not history_is_ready(store,session_key,released)):
        raise GovernanceHalt("final_sender_identity_changed")
    return released


def observe_send(adapter: Any, event: Any, session_key: str, released: ReleasedFinal | None, result: Any,
                 metadata: dict, reply_to: str | None) -> None:
    if released is None or getattr(result,"success",None) is not True:
        return
    raw = getattr(result,"raw_response",None)
    if type(raw) is dict and raw.get("partial_overflow"):
        return
    from .publication import _identity, _transaction as delivery_transaction
    from gateway import delivery_ledger as ledger
    config = Config.from_env()
    delivery = adapter._final_delivery_adapter(event.source)
    oid,values = _identity(config,event,session_key,released.text,delivery,metadata,reply_to)
    with delivery_transaction(ledger) as connection:
        row = connection.execute("""SELECT session_key,platform,chat_id,thread_id,content,adapter_profile,state,last_error
            FROM delivery_obligations WHERE obligation_id=?""",(oid,)).fetchone()
    if row is None or tuple(row[:6]) != values or row[6] != "br_delivered":
        return
    ack = json.loads(row[7] or "{}")
    if (ack.get("message_id") != getattr(result,"message_id",None)
            or ack.get("continuation_message_ids") != list(getattr(result,"continuation_message_ids",()))):
        return
    store = getattr(adapter,"_session_store",None)
    if store is not None and history_is_ready(store,session_key,released):
        clear_resume_owner(store,session_key,released,getattr(event,"_brokerrouter_final_agent",None))
