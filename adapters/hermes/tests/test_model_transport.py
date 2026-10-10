"""Fixed primary SDK and native conversation requests over real zero-fee TLS.

The local server stands in for the gateway authority, not a paid model. Native
Agent, primary factory, OpenAI SDK, HTTPX, bind operation and SQLite remain real.
"""
from __future__ import annotations

from contextlib import closing
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from uuid import uuid4

import test_transport_tls as tls


@unittest.skipUnless(os.environ.get("BROKERROUTER_HERMES_NATIVE_CONTRACT") == "1", "requires full fixed native Agent/SDK")
class ModelTransportTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from run_agent import AIAgent
        from agent.agent_runtime_helpers import create_openai_client
        from brokerrouter_hermes.gate import GovernanceHalt, bind_gateway_agent
        from brokerrouter_hermes import model_transport
        cls.Agent, cls.factory = AIAgent, staticmethod(create_openai_client)
        cls.Halt, cls.bind = GovernanceHalt, staticmethod(bind_gateway_agent)
        cls.module = model_transport
        cls.posts, cls.ids, cls.behavior, cls.counts = [], {}, "normal", {}
        cls.lock = threading.Lock()

        def post(handler):
            body = handler.rfile.read(int(handler.headers.get("content-length", "0")))
            payload = json.loads(body)
            headers = {name.lower(): value for name, value in handler.headers.items()}
            if handler.path.endswith("/bind"):
                handler.send_response(200)
                handler.send_header("content-type", "application/json")
                handler.end_headers()
                handler.wfile.write(json.dumps({"id": handler.path.split("/")[-2], "state": "active", **payload}).encode())
                return
            with cls.lock:
                cls.posts.append((handler.path, headers, payload))
                key = headers.get("idempotency-key", "missing")
                cls.counts[key] = count = cls.counts.get(key, 0) + 1
                request_id = cls.ids.setdefault(key, str(uuid4()))
                behavior = cls.behavior
            if behavior == "drop" and count == 1:
                handler.connection.shutdown(socket.SHUT_RDWR)
                handler.connection.close()
                return
            if behavior == "kill-child" and count == 1:
                # The request is visible outside the child, before any response.
                cls.accepted.set()
                cls.resume.wait(10)
                return
            status = 503 if behavior == "retry" and count == 1 else 200
            handler.send_response(status)
            handler.send_header("content-type", "text/event-stream" if payload.get("stream") else "application/json")
            if behavior == "redirect":
                handler.send_header("location", cls.url + "/foreign")
            if behavior not in {"missing", "retry"} or count > 1:
                handler.send_header("x-brokerrouter-request-id", "not-a-uuid" if behavior == "invalid" else
                    str(uuid4()) if behavior == "changed" else request_id)
            if behavior == "compressed":
                handler.send_header("content-encoding", "gzip")
            handler.end_headers()
            if behavior == "drop-body":
                return
            if status >= 400:
                handler.wfile.write(b'{"error":{"message":"local retry","type":"fixture","code":"fixture"}}')
                return
            if payload.get("stream"):
                for delta in ({"role": "assistant", "content": "local no-fee "}, {"content": "model fixture"}):
                    handler.wfile.write(("data: " + json.dumps({"id": "provider-is-not-source", "object": "chat.completion.chunk",
                        "created": 0, "model": "contract-model", "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}) + "\n\n").encode())
                if behavior != "incomplete":
                    handler.wfile.write(("data: " + json.dumps({"id": "provider-is-not-source", "object": "chat.completion.chunk",
                        "created": 0, "model": "contract-model", "choices": [{"index": 0, "delta": {}, "finish_reason":
                            behavior if behavior in {"length", "content_filter", "function_call"} else "stop"}]}) + "\n\n").encode())
                handler.wfile.write(b'data: [DONE]\n\n')
            else:
                handler.wfile.write(json.dumps({"id": "provider-is-not-source", "object": "chat.completion", "created": 0,
                    "model": "contract-model", "choices": [{"index": 0, "message": {"role": "assistant", "content": "local no-fee model fixture"},
                    "finish_reason": behavior if behavior in {"length", "content_filter", "function_call"} else "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}).encode())

        cls.old_post = getattr(tls.Handler, "do_POST", None)
        tls.Handler.do_POST = post
        tls.TransportTlsTests.setUpClass()
        cls.url, cls.ca = tls.TransportTlsTests.url, tls.TransportTlsTests.ca

    @classmethod
    def tearDownClass(cls):
        tls.TransportTlsTests.tearDownClass()
        if cls.old_post is None:
            del tls.Handler.do_POST
        else:
            tls.Handler.do_POST = cls.old_post

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        (self.home / "config.yaml").write_text(
            "model:\n  default: contract-model\n  provider: openai\n  context_length: 32768\ncompression:\n  enabled: false\n"
            "approvals:\n  timeout: 1800\n")
        self.environment = patch.dict(os.environ, {"HERMES_HOME": str(self.home), "BROKERROUTER_HERMES_GOVERNANCE": "off"})
        self.environment.start()
        self.agents, self.clients = [], []
        self.key = "sk_br_" + "11" * 32
        self.context = SimpleNamespace(_brokerrouter_turn_binding={"turn_id": str(uuid4()), "run_id": "stable-run"},
            session_id="stable-session", inbound_message_id="stable-message", run_generation=1,
            _status_adapter=object(), _voice_ack_loop=object())
        type(self).posts, type(self).ids, type(self).counts, type(self).behavior = [], {}, {}, "normal"
        self.required = {"BROKERROUTER_HERMES_GOVERNANCE": "required", "BROKERROUTER_BASE_URL": self.url,
            "BROKERROUTER_HERMES_APPROVALS_URL": self.url, "BROKERROUTER_VIRTUAL_KEY": self.key,
            "BROKERROUTER_HERMES_STATE_DB": str(self.home / "gate.db"), "BROKERROUTER_HERMES_STATE_KEY": "11" * 32,
            "BROKERROUTER_HERMES_GATEWAY_CA_FILE": str(self.ca)}

    def tearDown(self):
        for client in self.clients: client.close()
        for agent in self.agents:
            gate = getattr(agent, "_brokerrouter_governance_gate", None)
            if gate is not None: gate.client._transport._client.close()
            agent.close()
        self.environment.stop()
        self.temp.cleanup()

    def agent(self, *, bind=True):
        # The off constructor only prepares normal host internals; no network is
        # sent. Replace its client through the real required primary factory.
        with patch.dict(os.environ, {"BROKERROUTER_HERMES_GOVERNANCE": "off"}):
            agent = self.Agent(model="contract-model", provider="openai", api_mode="chat_completions",
                base_url=self.url + "/v1", api_key=self.key, enabled_toolsets=[], skip_context_files=True,
                skip_memory=True, skip_background_review=True, quiet_mode=True, session_id="stable-session")
        self.agents.append(agent)
        os.environ.update(self.required)
        old_client = agent.client
        agent.client = self.client(agent)
        old_client.close()
        if bind:
            self.bind(agent, self.context, SimpleNamespace(_close_native_stream_boundary=lambda *_: None))
            self.module.bind_model_origin(agent, self.context)
        agent._current_api_request_id = "local-conversation:api:1"
        return agent

    def client(self, agent, **overrides):
        client = self.factory(agent, {"base_url": self.url + "/v1", "api_key": self.key, "timeout": 2, **overrides},
                              reason="native-zero-fee-model", shared=False)
        self.clients.append(client)
        return client

    def request(self, agent, *, text="local zero fee", **kwargs):
        response = agent.client.chat.completions.create(model="contract-model", messages=[{"role": "user", "content": text}], **kwargs)
        self.module.complete_model_response(agent, response)
        return response

    def rows(self):
        from gateway import delivery_ledger as ledger
        with closing(ledger._connect()) as connection:
            return connection.execute("SELECT slot,ordinal,idempotency_key,request_id,accepted,payload_hash FROM brokerrouter_model_calls ORDER BY ordinal").fetchall()

    def test_actual_sdk_headers_source_uuid_and_distinct_logical_keys(self):
        agent = self.agent()
        first = self.request(agent, extra_headers={"Idempotency-Key": "spoof", "X-Brokerrouter-Turn-Id": str(uuid4())})
        self.assertEqual(first.id, "provider-is-not-source")
        self.assertEqual(self.posts[-1][1]["x-brokerrouter-turn-id"], self.context._brokerrouter_turn_binding["turn_id"])
        key = self.posts[-1][1]["idempotency-key"]
        self.assertNotEqual(key, "spoof")
        self.request(agent, extra_headers={"Idempotency-Key": "another-spoof"})
        self.assertEqual(self.posts[-1][1]["idempotency-key"], key)
        agent._current_api_request_id = "local-conversation:api:2"
        self.request(agent, text="next logical call")
        self.assertNotEqual(self.posts[-1][1]["idempotency-key"], key)
        self.assertEqual(self.module.source_requests(agent), tuple(row[3] for row in self.rows()))
        self.assertEqual(len(self.rows()), 2)

    def test_actual_sdk_retry_uses_the_first_durable_key(self):
        agent = self.agent()
        agent.client = self.client(agent, max_retries=1)
        type(self).behavior = "retry"
        self.request(agent)
        self.assertEqual(len(self.posts), 2)
        self.assertEqual(self.posts[0][1]["idempotency-key"], self.posts[1][1]["idempotency-key"])
        self.assertEqual(len(self.rows()), 1)

    def test_unknown_response_restart_preserves_key_and_blocks_new_slot(self):
        from openai import APIConnectionError
        agent = self.agent()
        type(self).behavior = "drop"
        with self.assertRaises(APIConnectionError): self.request(agent)
        key = self.posts[0][1]["idempotency-key"]
        agent._current_api_request_id = "different-local-turn:api:2"
        with self.assertRaises(self.Halt): self.request(agent, text="cannot blindly pay again")
        self.assertEqual(len(self.posts), 1)
        restarted = self.agent()
        restarted._current_api_request_id = "different-local-turn:api:1"
        self.request(restarted)
        self.assertEqual(self.posts[-1][1]["idempotency-key"], key)
        self.assertEqual(len(self.rows()), 1)

    def test_received_200_identity_without_body_is_unresolved_and_cannot_advance(self):
        agent = self.agent(); type(self).behavior = "drop-body"
        with self.assertRaises(Exception): self.request(agent)
        self.assertIsNotNone(self.rows()[0][3])
        self.assertEqual(self.rows()[0][4], 0)
        with self.assertRaises(self.Halt): self.module.source_requests(agent)
        agent._current_api_request_id = "local:api:2"
        with self.assertRaises(self.Halt): self.request(agent)
        self.assertEqual(len(self.posts), 1)

    def test_payload_model_and_logical_slot_drift_fail_before_network(self):
        agent = self.agent(); self.request(agent)
        with self.assertRaises(self.Halt): self.request(agent, text="changed body")
        agent.model = "changed-model"
        with self.assertRaises(self.Halt): self.request(agent)
        agent.model = "contract-model"; agent._current_api_request_id = "local:api:3"
        with self.assertRaises(self.Halt): self.request(agent)
        self.assertEqual(len(self.posts), 1)

    def test_restart_model_drift_cannot_mint_a_new_origin_or_key(self):
        agent = self.agent(); self.request(agent)
        restarted = self.agent()
        restarted.model = "changed-model"
        with self.assertRaises(self.Halt): self.module.bind_model_origin(restarted, self.context)
        self.assertEqual(len(self.posts), 1)
        self.assertEqual(len(self.rows()), 1)

    def test_missing_binding_foreign_endpoint_key_and_client_are_closed(self):
        agent = self.agent(bind=False)
        with self.assertRaises(self.Halt): self.request(agent)
        for overrides in ({"base_url": "https://foreign.invalid/v1"}, {"api_key": "foreign-key"},
                          {"http_client": object()}, {"ssl_verify": False}):
            with self.subTest(kind=next(iter(overrides))):
                with self.assertRaises(self.Halt): self.client(agent, **overrides)
        self.assertEqual(self.posts, [])

    def test_missing_invalid_changed_and_compressed_response_identity_halt(self):
        for mode in ("missing", "invalid", "compressed"):
            with self.subTest(mode=mode):
                # Fresh bound turn for each independent refused response.
                self.context._brokerrouter_turn_binding["turn_id"] = str(uuid4())
                agent = self.agent(); type(self).behavior = mode
                with self.assertRaises(self.Halt): self.request(agent)
        type(self).behavior = "normal"
        self.context._brokerrouter_turn_binding["turn_id"] = str(uuid4())
        agent = self.agent(); self.request(agent)
        type(self).behavior = "changed"
        with self.assertRaises(self.Halt): self.request(agent)

    def test_durable_claim_and_response_record_failure_never_return_unsafe_success(self):
        agent = self.agent()
        with patch.object(self.module, "_register", side_effect=OSError("PRIVATE_SQL_FAILURE")):
            with self.assertRaises(self.Halt): self.request(agent)
        self.assertEqual(self.posts, [])
        with patch.object(self.module, "_record_response", side_effect=OSError("PRIVATE_ACK_FAILURE")):
            with self.assertRaises(self.Halt): self.request(agent)
        self.assertEqual(len(self.posts), 1)
        with self.assertRaises(self.Halt): self.module.source_requests(agent)

    def test_model_facts_cannot_be_replaced_deleted_or_rebound(self):
        from gateway import delivery_ledger as ledger
        agent = self.agent(); self.request(agent)
        with closing(ledger._connect()) as connection:
            for statement in ("UPDATE brokerrouter_model_calls SET payload_hash='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
                              "UPDATE brokerrouter_model_calls SET idempotency_key='changed'",
                              "UPDATE brokerrouter_model_calls SET request_id='changed'",
                              "UPDATE brokerrouter_model_calls SET accepted=0", "DELETE FROM brokerrouter_model_calls",
                              "INSERT OR REPLACE INTO brokerrouter_model_calls SELECT * FROM brokerrouter_model_calls"):
                with self.subTest(statement=statement):
                    with self.assertRaises(sqlite3.DatabaseError): connection.execute(statement)
                    connection.rollback()
            for statement in ("UPDATE brokerrouter_model_origins SET model='changed'", "DELETE FROM brokerrouter_model_origins",
                              "INSERT OR REPLACE INTO brokerrouter_model_origins SELECT * FROM brokerrouter_model_origins"):
                with self.subTest(statement=statement):
                    with self.assertRaises(sqlite3.DatabaseError): connection.execute(statement)
                    connection.rollback()
        self.assertEqual(len(self.rows()), 1)

    def test_actual_native_loop_standard_and_streaming_requests_are_bound(self):
        for streaming in (False, True):
            with self.subTest(streaming=streaming):
                self.context._brokerrouter_turn_binding["turn_id"] = str(uuid4())
                agent = self.agent(); agent._disable_streaming = not streaming
                result = agent.run_conversation("local zero fee turn", task_id="native-wire-task")
                self.assertIn("local no-fee model fixture", result["final_response"])
                self.assertEqual(self.posts[-1][1]["x-brokerrouter-turn-id"], self.context._brokerrouter_turn_binding["turn_id"])
                self.assertTrue(self.module.source_requests(agent))

    def test_actual_incomplete_native_stream_cannot_pay_for_a_continuation(self):
        agent = self.agent(); agent._disable_streaming = False
        type(self).behavior = "incomplete"
        with self.assertRaises(self.Halt):
            agent.run_conversation("local incomplete stream", task_id="native-wire-task")
        self.assertEqual(len(self.posts), 1)
        self.assertEqual(self.rows()[0][4], 0)

    def test_actual_truncated_native_stream_cannot_pay_for_a_continuation(self):
        agent = self.agent(); agent._disable_streaming = False
        type(self).behavior = "length"
        with self.assertRaises(self.Halt):
            agent.run_conversation("local truncated stream", task_id="native-wire-task")
        self.assertEqual(len(self.posts), 1)
        self.assertEqual(self.rows()[0][4], 0)
        with self.assertRaises(self.Halt): self.module.source_requests(agent)

    def test_actual_filtered_and_legacy_function_results_are_not_completed_sources(self):
        agent = self.agent()
        for reason in ("content_filter", "function_call"):
            with self.subTest(reason=reason):
                type(self).behavior = reason
                with self.assertRaises(self.Halt): self.request(agent)
                self.assertEqual(self.rows()[0][4], 0)
                with self.assertRaises(self.Halt): self.module.source_requests(agent)
        agent._current_api_request_id = "local:api:2"
        with self.assertRaises(self.Halt): self.request(agent, text="no fresh paid continuation")
        self.assertEqual(len(self.posts), 2)

    def test_actual_primary_dispatch_rejects_native_protocol_and_unknown_continuations(self):
        from agent.turn_api_call import perform_api_call
        agent = self.agent()
        values = dict(api_kwargs={}, _original_api_kwargs={}, _llm_middleware_trace=[], _moa_prepared_request=None,
            _retry=SimpleNamespace(), thinking_spinner=None, retry_count=0, api_call_count=1, api_request_id="local:api:1",
            effective_task_id="native-task", turn_id="local-turn", interrupted=False)
        agent.api_mode = "anthropic_messages"
        with self.assertRaises(self.Halt): perform_api_call(agent, **values)
        agent.api_mode = "chat_completions"
        with patch("hermes_cli.plugins.has_middleware", return_value=True):
            with self.assertRaises(self.Halt): perform_api_call(agent, **values)
        managed_turn = SimpleNamespace(lease=SimpleNamespace(live_runtime=lambda:
            SimpleNamespace(managed_execution_enabled=lambda: True)))
        with patch("agent.relay_runtime.active_turn", return_value=managed_turn):
            with self.assertRaises(self.Halt): perform_api_call(agent, **values)
        agent.client = object()
        with self.assertRaises(self.Halt): perform_api_call(agent, **values)
        self.agents.remove(agent)  # object fixture is intentionally not closeable
        self.assertEqual(self.posts, [])

    def test_required_native_constructor_uses_the_owned_primary_sdk(self):
        os.environ.update(self.required)
        agent = self.Agent(model="contract-model", provider="openai", api_mode="chat_completions",
            base_url=self.url + "/v1", api_key=self.key, enabled_toolsets=[], skip_context_files=True,
            skip_memory=True, skip_background_review=True, quiet_mode=True, session_id="stable-session")
        self.agents.append(agent)
        self.bind(agent, self.context, SimpleNamespace(_close_native_stream_boundary=lambda *_: None))
        self.module.bind_model_origin(agent, self.context)
        agent._current_api_request_id = "native:api:1"
        self.request(agent)
        self.assertEqual(self.posts[-1][1]["x-brokerrouter-turn-id"], agent._brokerrouter_turn_id)

    def test_off_primary_factory_preserves_the_native_unbound_protocol(self):
        agent = self.agent(bind=False)
        with patch.dict(os.environ, {"BROKERROUTER_HERMES_GOVERNANCE": "off"}):
            client = self.client(agent)
            # The fixture's private CA is only for this explicitly off control.
            import ssl, httpx
            client.close()
            http = httpx.Client(verify=ssl.create_default_context(cafile=str(self.ca)), trust_env=False)
            client = self.client(agent, http_client=http)
            result = client.chat.completions.create(model="contract-model", messages=[{"role": "user", "content": "local"}])
            self.assertEqual(result.object, "chat.completion")
        self.assertNotIn("x-brokerrouter-turn-id", self.posts[-1][1])
        self.assertNotIn("idempotency-key", self.posts[-1][1])

    def test_actual_wire_sigkill_before_response_cannot_create_a_new_paid_identity(self):
        type(self).behavior = "kill-child"
        type(self).accepted, type(self).resume = threading.Event(), threading.Event()
        # A child runs the same real Agent/factory/bind/SDK, then is killed after
        # the TLS authority receives it but before returning any header or body.
        environment = dict(os.environ, **self.required, MODEL_FIXTURE_HOME=str(self.home),
            MODEL_FIXTURE_TURN=self.context._brokerrouter_turn_binding["turn_id"], PYTHONDONTWRITEBYTECODE="1")
        script = """import os,unittest
from test_model_transport import ModelTransportTest
from types import SimpleNamespace
# Reuse only the native construction method; no fake Agent or SDK.
t=ModelTransportTest();t.home=__import__('pathlib').Path(os.environ['MODEL_FIXTURE_HOME']);t.key=os.environ['BROKERROUTER_VIRTUAL_KEY'];t.url=os.environ['BROKERROUTER_BASE_URL'];t.required={k:v for k,v in os.environ.items() if k.startswith('BROKERROUTER_')};t.agents=[];t.clients=[]
from run_agent import AIAgent
from agent.agent_runtime_helpers import create_openai_client
from brokerrouter_hermes.gate import bind_gateway_agent
from brokerrouter_hermes import model_transport
t.Agent=AIAgent;t.factory=create_openai_client;t.bind=bind_gateway_agent;t.module=model_transport
t.context=SimpleNamespace(_brokerrouter_turn_binding={'turn_id':os.environ['MODEL_FIXTURE_TURN'],'run_id':'stable-run'},session_id='stable-session',inbound_message_id='stable-message',run_generation=1,_status_adapter=object(),_voice_ack_loop=object())
a=t.agent();t.request(a)
"""
        child = subprocess.Popen([sys.executable, "-B", "-c", script], env=environment,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            if not self.accepted.wait(15):
                child.kill(); _, error = child.communicate(timeout=5)
                self.fail("actual child never reached model authority: " + error.decode()[-1200:])
            child.kill(); child.communicate(timeout=5)
        finally:
            self.resume.set()
        self.assertLess(child.returncode, 0)
        key = self.posts[0][1]["idempotency-key"]
        type(self).behavior = "normal"
        restarted = self.agent(); restarted._current_api_request_id = "new-local-conversation:api:1"
        self.request(restarted)
        self.assertEqual(self.posts[-1][1]["idempotency-key"], key)
        self.assertEqual(len(self.rows()), 1)
