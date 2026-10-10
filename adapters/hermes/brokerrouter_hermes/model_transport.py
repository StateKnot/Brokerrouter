"""Bound primary model wire facts in the native Hermes state database.

An API ordinal identifies a local logical slot only. The gateway response header
is the sole source of a service request UUID. These facts are not final-output
approval grants, and no response body is stored a second time here.
"""
from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import json
import re
import time
from typing import Any
from uuid import UUID, uuid4

import httpx

from .client import _gateway_tls
from .config import Config
from .gate import GatewayTurnBinding, GovernanceHalt


@dataclass(frozen=True, repr=False)
class ModelOrigin:
    scope: str
    turn_id: str
    session_id: str
    message_id: str
    run_id: str
    generation: int
    model: str


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def _sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def bind_model_origin(agent: Any, context: Any) -> None:
    config = Config.from_env()
    if config is None:
        return
    try:
        raw = context._brokerrouter_turn_binding
        if type(raw) is not dict or set(raw) != {"turn_id", "run_id"}:
            raise ValueError("missing binding")
        binding = GatewayTurnBinding(str(raw["turn_id"]), context.session_id,
            context.inbound_message_id, raw["run_id"], context.run_generation).validate()
        turn = str(UUID(binding.turn_id))
        gate = agent._brokerrouter_governance_gate
        if turn != agent._brokerrouter_turn_id or gate.config != config:
            raise ValueError("model turn was not bound by the gateway")
        model = agent.model
        if type(model) is not str or not model or len(model.encode("utf-8")) > 200:
            raise ValueError("missing model")
        origin = ModelOrigin(_sha(config.virtual_key.encode("utf-8")), turn,
            binding.session_id, binding.message_id, binding.run_id, binding.generation, model)
        # A successful fresh server bind may begin a different user turn on a
        # pooled Agent. Each request snapshots this immutable origin.
        agent._brokerrouter_model_origin = origin
        _bind_origin(origin)
        verify_model_client(agent)
    except Exception as exc:
        raise GovernanceHalt("model_origin_invalid") from exc


@contextmanager
def _transaction():
    from gateway import delivery_ledger as ledger
    with ledger._DB_LOCK, ledger._transaction() as connection:
        connection.execute("PRAGMA synchronous=FULL")
        connection.execute("BEGIN IMMEDIATE")
        connection.execute("""CREATE TABLE IF NOT EXISTS brokerrouter_model_origins(
            origin TEXT PRIMARY KEY,model TEXT NOT NULL,created_at REAL NOT NULL)""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_model_origin_no_update
            BEFORE UPDATE ON brokerrouter_model_origins
            BEGIN SELECT RAISE(ABORT,'immutable model origin'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_model_origin_no_replace
            BEFORE INSERT ON brokerrouter_model_origins WHEN EXISTS(
                SELECT 1 FROM brokerrouter_model_origins WHERE origin=NEW.origin)
            BEGIN SELECT RAISE(ABORT,'model origin cannot be replaced'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_model_origin_no_delete
            BEFORE DELETE ON brokerrouter_model_origins
            BEGIN SELECT RAISE(ABORT,'model origin cannot be deleted'); END""")
        connection.execute("""CREATE TABLE IF NOT EXISTS brokerrouter_model_calls(
            slot TEXT PRIMARY KEY, origin TEXT NOT NULL, ordinal INTEGER NOT NULL CHECK(ordinal>=0),
            method TEXT NOT NULL, path TEXT NOT NULL, payload_hash TEXT NOT NULL CHECK(length(payload_hash)=64),
            model TEXT NOT NULL, idempotency_key TEXT NOT NULL UNIQUE,
            request_id TEXT, accepted INTEGER NOT NULL DEFAULT 0 CHECK(accepted IN (0,1)),
            created_at REAL NOT NULL)""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_model_immutable
            BEFORE UPDATE ON brokerrouter_model_calls WHEN
            NEW.slot IS NOT OLD.slot OR NEW.origin IS NOT OLD.origin OR NEW.ordinal IS NOT OLD.ordinal OR
            NEW.method IS NOT OLD.method OR NEW.path IS NOT OLD.path OR
            NEW.payload_hash IS NOT OLD.payload_hash OR NEW.model IS NOT OLD.model OR
            NEW.idempotency_key IS NOT OLD.idempotency_key OR NEW.created_at IS NOT OLD.created_at OR
            (OLD.request_id IS NOT NULL AND NEW.request_id IS NOT OLD.request_id) OR
            NEW.accepted < OLD.accepted
            BEGIN SELECT RAISE(ABORT,'immutable model wire fact'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_model_no_replace
            BEFORE INSERT ON brokerrouter_model_calls WHEN EXISTS(
                SELECT 1 FROM brokerrouter_model_calls WHERE slot=NEW.slot)
            BEGIN SELECT RAISE(ABORT,'model wire fact cannot be replaced'); END""")
        connection.execute("""CREATE TRIGGER IF NOT EXISTS brokerrouter_model_no_delete
            BEFORE DELETE ON brokerrouter_model_calls
            BEGIN SELECT RAISE(ABORT,'model wire fact cannot be deleted'); END""")
        yield connection


def _origin(agent: Any, config: Config) -> ModelOrigin:
    origin = getattr(agent, "_brokerrouter_model_origin", None)
    if not isinstance(origin, ModelOrigin) or origin.scope != _sha(config.virtual_key.encode("utf-8")):
        raise GovernanceHalt("model_turn_binding_missing")
    if origin.turn_id != getattr(agent, "_brokerrouter_turn_id", None) or origin.model != agent.model:
        raise GovernanceHalt("model_origin_changed")
    return origin


def _origin_json(origin: ModelOrigin) -> str:
    # Identity must exclude model; model drift cannot buy a fresh call slot/key.
    return _json({name: value for name, value in origin.__dict__.items() if name != "model"})


def _bind_origin(origin: ModelOrigin) -> None:
    identity = _origin_json(origin)
    with _transaction() as connection:
        row = connection.execute("SELECT model FROM brokerrouter_model_origins WHERE origin=?", (identity,)).fetchone()
        if row is None:
            connection.execute("INSERT INTO brokerrouter_model_origins(origin,model,created_at) VALUES(?,?,?)",
                (identity, origin.model, time.time()))
        elif row[0] != origin.model:
            raise GovernanceHalt("model_origin_changed")


def verify_model_client(agent: Any) -> None:
    config = Config.from_env()
    if config is None:
        return
    origin = _origin(agent, config)
    if agent.provider != "openai" or agent.api_mode != "chat_completions" or getattr(
            agent.client, "_brokerrouter_model_scope", None) != origin.scope:
        raise GovernanceHalt("model_protocol_unsupported")
    from hermes_cli.plugins import has_middleware
    from agent import relay_runtime
    turn = relay_runtime.active_turn()
    runtime = turn.lease.live_runtime() if turn is not None else None
    if any(has_middleware(kind) for kind in ("llm_request", "llm_execution")) or (
            runtime is not None and runtime.managed_execution_enabled()):
        raise GovernanceHalt("model_untrusted_continuation")


def _register(origin: ModelOrigin, ordinal: int, request: httpx.Request, body: bytes) -> tuple[str, str]:
    if ordinal > 64:
        raise GovernanceHalt("model_source_request_limit")
    encoded_origin = _origin_json(origin)
    slot = _sha(_json({"origin": encoded_origin, "ordinal": ordinal}).encode("utf-8"))
    identity = (encoded_origin, ordinal, request.method, request.url.path, _sha(body), origin.model)
    with _transaction() as connection:
        bound = connection.execute("SELECT model FROM brokerrouter_model_origins WHERE origin=?", (encoded_origin,)).fetchone()
        if bound is None or bound[0] != origin.model:
            raise GovernanceHalt("model_origin_changed")
        row = connection.execute("""SELECT origin,ordinal,method,path,payload_hash,model,idempotency_key
            FROM brokerrouter_model_calls WHERE slot=?""", (slot,)).fetchone()
        if row is not None:
            if tuple(row[:6]) != identity:
                raise GovernanceHalt("model_request_changed")
            return slot, row[6]
        prior = connection.execute("SELECT ordinal,accepted FROM brokerrouter_model_calls WHERE origin=? ORDER BY ordinal",
            (encoded_origin,)).fetchall()
        if any(row[1] != 1 for row in prior):
            raise GovernanceHalt("model_previous_request_unresolved")
        if ordinal != ((prior[-1][0] + 1) if prior else 1):
            raise GovernanceHalt("model_logical_request_drift")
        key = "hermes-model-v1-" + uuid4().hex
        connection.execute("""INSERT INTO brokerrouter_model_calls(
            slot,origin,ordinal,method,path,payload_hash,model,idempotency_key,created_at)
            VALUES(?,?,?,?,?,?,?,?,?)""", (slot, *identity, key, time.time()))
        return slot, key


def _record_response(slot: str, request_id: str, accepted: bool) -> None:
    with _transaction() as connection:
        row = connection.execute("SELECT request_id FROM brokerrouter_model_calls WHERE slot=?", (slot,)).fetchone()
        if row is None or (row[0] is not None and row[0] != request_id):
            raise GovernanceHalt("model_response_identity_changed")
        connection.execute("UPDATE brokerrouter_model_calls SET request_id=?,accepted=MAX(accepted,?) WHERE slot=?",
            (request_id, int(accepted), slot))


def complete_model_response(agent: Any, response: Any) -> None:
    config = Config.from_env()
    if config is None:
        return
    origin = _origin(agent, config)
    choices = getattr(response, "choices", None)
    from agent.chat_completion_helpers import PARTIAL_STREAM_STUB_ID
    # A bare 200/stream-open header proves identity, not completed text. Stop
    # before Hermes' automatic continuation on a disconnected or truncated
    # response. Filtered/legacy responses are not certified completed sources.
    if getattr(response, "id", None) == PARTIAL_STREAM_STUB_ID or not choices or any(getattr(choice, "finish_reason", None) not in
            {"stop", "tool_calls"} for choice in choices):
        raise GovernanceHalt("model_response_incomplete")
    identifier = getattr(agent, "_current_api_request_id", None)
    match = re.fullmatch(r".{1,200}:api:([0-9]{1,9})", identifier) if type(identifier) is str else None
    if match is None:
        raise GovernanceHalt("model_logical_request_missing")
    slot = _sha(_json({"origin": _origin_json(origin), "ordinal": int(match[1])}).encode("utf-8"))
    if getattr(agent, "_brokerrouter_model_response_slot", None) != slot:
        raise GovernanceHalt("model_response_identity_missing")
    try:
        with _transaction() as connection:
            row = connection.execute("SELECT request_id FROM brokerrouter_model_calls WHERE slot=?", (slot,)).fetchone()
            if row is None or row[0] is None:
                raise GovernanceHalt("model_response_identity_missing")
            connection.execute("UPDATE brokerrouter_model_calls SET accepted=1 WHERE slot=?", (slot,))
    except Exception as exc:
        raise GovernanceHalt("model_response_fact_unavailable") from exc


def source_requests(agent: Any) -> tuple[str, ...]:
    config = Config.from_env()
    if config is None:
        return ()
    origin = _origin(agent, config)
    try:
        with _transaction() as connection:
            rows = connection.execute("SELECT request_id,accepted,model FROM brokerrouter_model_calls WHERE origin=? ORDER BY ordinal",
                (_origin_json(origin),)).fetchall()
        if not rows or len(rows) > 64 or any(row[0] is None or row[1] != 1 or row[2] != origin.model for row in rows):
            raise GovernanceHalt("model_source_request_unresolved")
        return tuple(str(UUID(row[0])) for row in rows)
    except Exception as exc:
        raise GovernanceHalt("model_source_fact_unavailable") from exc


def create_governed_model_client(agent: Any, client_kwargs: dict) -> Any | None:
    """Required mode owns the certified OpenAI Chat HTTP path; off is upstream."""
    try:
        config = Config.from_env()
    except Exception as exc:
        raise GovernanceHalt("model_transport_configuration_invalid") from exc
    if config is None:
        return None
    try:
        if agent.provider != "openai" or agent.api_mode != "chat_completions":
            raise ValueError("uncertified primary protocol")
        kwargs = dict(client_kwargs)
        base = config.base_url.rstrip("/") + "/v1"
        if str(kwargs.get("base_url", "")).rstrip("/") != base or kwargs.get("api_key") != config.virtual_key:
            raise ValueError("gateway endpoint/key mismatch")
        if "http_client" in kwargs or kwargs.get("ssl_verify") is False:
            raise ValueError("foreign model transport")
        kwargs.pop("ssl_ca_cert", None)
        kwargs.pop("ssl_verify", None)
        expected_url = httpx.URL(base + "/chat/completions")

        def on_request(request: httpx.Request) -> None:
            try:
                if Config.from_env() != config:
                    raise GovernanceHalt("model_transport_configuration_changed")
                if (request.url.scheme, request.url.host, request.url.port) != (
                        expected_url.scheme, expected_url.host, expected_url.port) or request.url.query:
                    raise GovernanceHalt("model_endpoint_changed")
                request.headers["authorization"] = "Bearer " + config.virtual_key
                request.headers["accept-encoding"] = "identity"
                if request.method == "GET" and request.url.path == httpx.URL(base + "/models").path:
                    return
                if request.method != "POST" or request.url.path != expected_url.path:
                    raise GovernanceHalt("model_protocol_unsupported")
                origin = _origin(agent, config)
                identifier = getattr(agent, "_current_api_request_id", None)
                match = re.fullmatch(r".{1,200}:api:([0-9]{1,9})", identifier) if type(identifier) is str else None
                if match is None:
                    raise GovernanceHalt("model_logical_request_missing")
                body = request.read()
                if not body or len(body) > 2 * 1024 * 1024:
                    raise GovernanceHalt("model_request_oversized")
                payload = json.loads(body)
                if type(payload) is not dict or payload.get("model") != origin.model:
                    raise GovernanceHalt("model_request_model_changed")
                # Strict JSON rejects NaN/Infinity and takes a bounded immutable
                # copy of the exact SDK wire bytes before committing its identity.
                _json(payload)
                slot, key = _register(origin, int(match[1]), request, body)
                request.headers["x-brokerrouter-turn-id"] = origin.turn_id
                request.headers["idempotency-key"] = key
                request.extensions["brokerrouter_model_slot"] = slot
            except Exception as exc:
                raise GovernanceHalt("model_request_fact_unavailable") from exc

        def on_response(response: httpx.Response) -> None:
            slot = response.request.extensions.get("brokerrouter_model_slot")
            if slot is None:
                return
            try:
                if 300 <= response.status_code < 400:
                    raise GovernanceHalt("model_redirect_refused")
                raw = response.headers.get("x-brokerrouter-request-id")
                if raw is None:
                    if response.status_code >= 400:
                        return  # any subsequent attempt retains the original key
                    raise GovernanceHalt("model_response_identity_missing")
                request_id = str(UUID(raw))
                _record_response(slot, request_id, False)
                if response.headers.get("content-encoding", "identity").strip().lower() != "identity":
                    raise GovernanceHalt("model_compressed_response_refused")
                if 200 <= response.status_code < 300:
                    agent._brokerrouter_model_response_slot = slot
            except BaseException as exc:
                response.close()
                if isinstance(exc, GovernanceHalt):
                    raise
                raise GovernanceHalt("model_response_fact_unavailable") from exc

        http = httpx.Client(verify=_gateway_tls(config.gateway_ca_file), trust_env=False,
            follow_redirects=False, timeout=kwargs.get("timeout", httpx.Timeout(180, connect=10, pool=10)),
            event_hooks={"request": [on_request], "response": [on_response]})
        kwargs["http_client"] = http
        kwargs.setdefault("max_retries", 0)
        from openai import OpenAI
        try:
            client = OpenAI(**kwargs)
            client._brokerrouter_model_scope = _sha(config.virtual_key.encode("utf-8"))
            return client
        except BaseException:
            http.close()
            raise
    except Exception as exc:
        raise GovernanceHalt("model_transport_configuration_invalid") from exc
