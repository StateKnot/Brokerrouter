"""Real fixed Agent/SDK/TLS/native SQLite and sender, zero-fee authority fixture.

This checks the host boundary and immutable recovery. The separate PostgreSQL/
workbench deployment acceptance proves the service policy and human roles.
"""
from __future__ import annotations
import asyncio
from contextlib import closing
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from uuid import uuid4

import test_model_transport as fixtures
import test_transport_tls as tls


@unittest.skipUnless(os.environ.get("BROKERROUTER_HERMES_NATIVE_CONTRACT") == "1", "requires complete fixed Hermes final host")
class FinalPublicationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        fixtures.ModelTransportTest.setUpClass()
        cls.primary = staticmethod(tls.Handler.do_POST)
        from brokerrouter_hermes import final_publication
        from gateway.run import GatewayRunner
        from gateway.config import GatewayConfig, Platform
        from gateway.session import SessionSource
        from gateway.platforms.base import BasePlatformAdapter, PlatformConfig, SendResult
        cls.module, cls.Runner, cls.Config, cls.Source = final_publication, GatewayRunner, GatewayConfig, SessionSource
        cls.Base, cls.Platform, cls.PlatformConfig, cls.SendResult = BasePlatformAdapter, Platform, PlatformConfig, SendResult
        cls.Halt = fixtures.ModelTransportTest.Halt

        def post(handler):
            if not (handler.path.startswith("/v1/governance/final-publications") or handler.path.endswith("/bind")):
                return cls.primary(handler)
            body = json.loads(handler.rfile.read(int(handler.headers["content-length"])))
            status = 200
            if handler.path.endswith("/bind"):
                turn = handler.path.split("/")[-2]
                cls.bound[turn] = body
                response = {"id":turn,"state":"active",**body}
            elif handler.path == "/v1/governance/final-publications":
                cls.final_calls.append(("record",body))
                turn = body["turn_id"]
                prior = cls.final_records.get(turn)
                if prior is not None and prior["request"] != body:
                    status,response = 409,{"error":"immutable final"}
                elif not set(body["source_request_ids"]) <= set(fixtures.ModelTransportTest.ids.values()):
                    status,response = 409,{"error":"unknown primary source"}
                else:
                    if prior is None:
                        prior = {"request":body,"id":str(uuid4()),"output":str(uuid4()),"approval":str(uuid4()),
                                 "expires":(datetime.now(timezone.utc)+timedelta(minutes=30)).isoformat()}
                        cls.final_records[turn] = prior
                    response = cls.metadata(prior)
                    response.update(id=prior["id"],state="recorded")
                    if cls.record_loss:
                        cls.record_loss = False
                        handler.connection.close()
                        return
            else:
                cls.final_calls.append(("release",body))
                publication = handler.path.split("/")[-2]
                prior = next(row for row in cls.final_records.values() if row["id"] == publication)
                turn = prior["request"]["turn_id"]
                bound = cls.bound[turn]
                stage = cls.states.pop(0) if len(cls.states)>1 else cls.states[0]
                payload = {"publication_id":publication,"source_request_ids":prior["request"]["source_request_ids"],
                           **{k:bound[k] for k in ("session_id","message_id","run_id","generation")},
                           "text":prior["request"]["text"]}
                digest = hashlib.sha256(json.dumps({"turn":turn,"model":prior["request"]["model_id"],
                    "tool":None,"payload":payload},ensure_ascii=False,sort_keys=True,separators=(",",":")).encode()).hexdigest()
                response = {**cls.metadata(prior),"id":prior["output"],"publication_id":publication,
                            "output_operation_id":prior["output"],"payload_hash":digest,"state":stage}
                if stage == "ready":
                    response["text"] = prior["request"]["text"]
                elif stage == "checking":
                    response["evaluations"] = [str(uuid4())]
                elif stage == "waiting_approval":
                    response["approval"] = {"id":prior["approval"],"operation_id":prior["output"],"turn_id":turn,
                        "state":"pending","scope":"current_turn_only","resume_requires_recheck":True,
                        "options":["continue_this_turn","reject","feedback"]}
                else:
                    status,response = 409,{"error":"final rejected"}
            if cls.mutator is not None:
                cls.mutator(response)
            handler.send_response(status)
            handler.send_header("content-type","application/json")
            handler.end_headers()
            handler.wfile.write(json.dumps(response).encode())
        tls.Handler.do_POST = post

    @classmethod
    def metadata(cls, row):
        body = row["request"]
        encoded = body["text"].encode()
        return {"turn_id":body["turn_id"],"model_id":body["model_id"],"source_request_ids":body["source_request_ids"],
                "text_sha256":hashlib.sha256(encoded).hexdigest(),"bytes":len(encoded),"expires_at":row["expires"]}

    @classmethod
    def tearDownClass(cls):
        tls.Handler.do_POST = cls.primary
        fixtures.ModelTransportTest.tearDownClass()

    def setUp(self):
        self.case = fixtures.ModelTransportTest()
        self.case.setUp()
        type(self).bound, type(self).final_records, type(self).final_calls = {}, {}, []
        type(self).states, type(self).mutator, type(self).record_loss = ["ready"], None, False
        self.runner = self.Runner(self.Config())
        self.store = self.runner.session_store
        self.source = self.Source(platform=self.Platform.LOCAL,chat_id="fixed-final-chat")
        self.entry = self.store.get_or_create_session(self.source)
        self.case.context.session_id = self.entry.session_id
        self.case.context.session_key = self.entry.session_key
        self.case.context.run_generation = self.runner._begin_session_run_generation(self.entry.session_key)
        self.case.context._brokerrouter_native_turn_owner = self.store.mark_turn_active(self.entry.session_key)
        self.case.context._brokerrouter_run_owner_key = self.entry.session_key
        self.db = self.store._db_for_session_id(self.entry.session_id)
        self.notices, self.sends = [], []
        self.sender = None
        owner = self

        class Sink(self.Base):
            name = "real-native-final-local-sink"
            async def connect(self): return True
            async def disconnect(self): return None
            async def get_chat_info(self, _chat): return {}
            async def send(self, chat_id, content, **kwargs):
                owner.sends.append(content)
                if owner.sender is not None:
                    return await owner.sender(chat_id,content,kwargs)
                return owner.SendResult(success=True,message_id="actual-local-ack")
        self.adapter = Sink(self.PlatformConfig(),self.Platform.LOCAL)
        self.adapter._session_store = self.store
        self.event = SimpleNamespace(text="zero fee final",message_id=self.case.context.inbound_message_id,
            ledger_message_id=None,source=self.source,metadata={"brokerrouter_turn":dict(self.case.context._brokerrouter_turn_binding)})

    def tearDown(self):
        self.store.close_all_db_handles()
        self.case.tearDown()

    def agent(self):
        agent = self.case.agent(bind=False)
        agent.session_id = self.entry.session_id
        agent._session_db = self.db
        self.case.bind(agent,self.case.context,self.runner)
        self.case.module.bind_model_origin(agent,self.case.context)
        self.module.prepare(agent,self.case.context,self.store,self.runner)
        agent._brokerrouter_approval_callback = self.notices.append
        clock = [0.0]
        gate = agent._brokerrouter_governance_gate
        gate._monotonic = lambda: clock[0]
        gate._sleep = lambda seconds: clock.__setitem__(0,clock[0]+151)
        return agent

    def conversation(self, agent, history=None, *, streaming=False):
        agent._disable_streaming = not streaming
        result = agent.run_conversation("zero fee dedicated final",conversation_history=history,task_id="native-final")
        self.module.note_return(agent,result)
        result.update(history_offset=len(history or []),_brokerrouter_final_agent=agent)
        self.assertEqual(self.db.get_messages(self.entry.session_id),history or [])
        return result

    def finish(self, result, text=None):
        return asyncio.run(self.module.finalize_for_gateway(self.runner,self.event,self.entry.session_key,
            self.entry.session_key,self.case.context.run_generation,result,
            result["final_response"] if text is None else text))

    def send(self, text):
        return asyncio.run(self.adapter.send_final_ledgered(self.event,self.entry.session_key,text,{},reply_to=None))[0]

    def test_actual_stream_and_native_history_are_held_until_ready_then_sender_rechecks(self):
        agent = self.agent()
        captured = []
        agent.stream_delta_callback = captured.append
        result = self.conversation(agent,streaming=True)
        self.assertEqual(captured,[])
        text,projected = self.finish(result,result["final_response"]+"\n\nexact runtime footer")
        self.assertEqual(self.db.get_messages(self.entry.session_id)[-1]["content"],text)
        self.assertNotIn("last_reasoning",projected[-1])
        self.assertTrue(self.send(text).success)
        self.assertTrue(self.send(text).success)
        self.assertEqual(self.sends,[text])
        self.assertEqual(len(self.case.posts),1)
        type(self).states = ["waiting_approval"]
        with self.assertRaises(self.Halt): self.send(text)
        self.assertEqual(self.sends,[text])
        self.assertEqual(len(self.notices),1)

    def test_pending_and_rejected_body_never_reaches_ordinary_db_or_next_cached_turn(self):
        agent = self.agent()
        result = self.conversation(agent)
        type(self).states = ["waiting_approval"]
        with self.assertRaises(self.Halt): self.finish(result)
        self.assertEqual(self.db.get_messages(self.entry.session_id),[])
        self.assertEqual(self.sends,[])
        type(self).states = ["rejected"]
        with self.assertRaises(self.Halt): self.module.release(agent)
        from gateway.run_turn_runner import TurnRunner
        context = SimpleNamespace(history=[],channel_prompt=None,user_config={},session_id=self.entry.session_id,
                                  session_key=self.entry.session_key)
        actual_runner = TurnRunner.__new__(TurnRunner)
        actual_runner._ctx = context
        history,*_ = actual_runner._load_turn_history(agent,True)
        self.assertEqual(history,[])
        self.assertIsNone(agent._session_messages)
        self.case.context._brokerrouter_turn_binding = {"turn_id":str(uuid4()),"run_id":"fresh-run"}
        self.case.context.inbound_message_id = "fresh-message"
        self.case.context.run_generation = self.runner._begin_session_run_generation(self.entry.session_key)
        self.case.context._brokerrouter_native_turn_owner = self.store.mark_turn_active(self.entry.session_key)
        self.case.bind(agent,self.case.context,self.runner)
        self.case.module.bind_model_origin(agent,self.case.context)
        self.module.prepare(agent,self.case.context,self.store,self.runner)
        agent._disable_streaming = True
        new = agent.run_conversation("fresh after rejection",conversation_history=history,task_id="new-native-final")
        self.assertTrue(new["completed"])
        wire = self.case.posts[-1][2]["messages"]
        self.assertFalse(any(item.get("role")=="assistant" and result["final_response"] in str(item.get("content")) for item in wire))
        self.assertEqual(self.db.get_messages(self.entry.session_id),[])

    def test_two_real_native_turns_append_only_new_interval(self):
        agent = self.agent()
        first = self.conversation(agent)
        self.finish(first)
        history = self.db.get_messages(self.entry.session_id)
        self.assertEqual(len(history),2)
        self.case.context._brokerrouter_turn_binding = {"turn_id":str(uuid4()),"run_id":"second-run"}
        self.case.context.inbound_message_id = "second-message"
        self.case.context.run_generation = self.runner._begin_session_run_generation(self.entry.session_key)
        self.case.context._brokerrouter_native_turn_owner = self.store.mark_turn_active(self.entry.session_key)
        agent = self.agent()
        # Provider payload uses native ordinary rows; record timestamp fields are
        # projected by the real builder and are irrelevant to final identity.
        from gateway.run_turn_runner import TurnRunner
        runner = TurnRunner.__new__(TurnRunner)
        runner._ctx = SimpleNamespace(history=history,channel_prompt=None,user_config={},session_id=self.entry.session_id,
                                     session_key=self.entry.session_key)
        provider_history,*_ = runner._load_turn_history(agent,True)
        result = agent.run_conversation("second zero fee",conversation_history=provider_history,task_id="native-second")
        self.module.note_return(agent,result)
        result.update(history_offset=len(provider_history),_brokerrouter_final_agent=agent)
        self.finish(result)
        rows = self.db.get_messages(self.entry.session_id)
        self.assertEqual([row["role"] for row in rows],["user","assistant","user","assistant"])
        self.assertEqual(sum(row.get("content")=="zero fee dedicated final" for row in rows),1)

    def test_frozen_resume_uses_current_release_and_no_model_execution(self):
        agent = self.agent()
        result = self.conversation(agent)
        type(self).record_loss = True
        with self.assertRaises(self.Halt): self.finish(result)
        count = len(self.case.posts)
        restored_agent = self.case.agent(bind=False)
        restored_agent.session_id = self.entry.session_id
        self.case.bind(restored_agent,self.case.context,self.runner)
        self.case.module.bind_model_origin(restored_agent,self.case.context)
        restored = self.module.prepare(restored_agent,self.case.context,self.store,self.runner)
        self.assertEqual(restored.text,result["final_response"])
        restored_result = {"_brokerrouter_final_agent":restored_agent,"_brokerrouter_final_restored":True}
        text,_ = self.finish(restored_result,restored.text)
        self.assertTrue(self.send(text).success)
        self.assertEqual(len(self.case.posts),count)
        self.assertEqual(len(self.db.get_messages(self.entry.session_id)),2)

    def test_actual_native_marker_prevents_late_prepare_from_overwriting_new_owner(self):
        old = self.agent()
        old_origin = old._brokerrouter_model_origin
        old_token = self.case.context._brokerrouter_native_turn_owner
        release_old, waiting = threading.Event(), threading.Event()
        outcome = []

        def late_worker():
            waiting.set()
            if not release_old.wait(5):
                outcome.append("timed_out")
                return
            try:
                self.module.bind_resume_owner(self.store,self.entry.session_key,old_origin,old_token,lambda:True)
            except self.Halt:
                outcome.append("rejected")
            else:
                outcome.append("overwrote")

        worker = threading.Thread(target=late_worker)
        worker.start()
        self.assertTrue(waiting.wait(5))
        next_token = self.store.mark_turn_active(self.entry.session_key)
        self.case.context._brokerrouter_turn_binding = {"turn_id":str(uuid4()),"run_id":"next-owner"}
        self.case.context.inbound_message_id = "next-owner-message"
        self.case.context.run_generation = self.runner._begin_session_run_generation(self.entry.session_key)
        self.case.context._brokerrouter_native_turn_owner = next_token
        new = self.agent()
        before = dict(self.entry.metadata)
        release_old.set()
        worker.join(5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(outcome,["rejected"])
        self.assertEqual(self.entry.metadata,before)
        self.assertEqual(self.entry.active_turn_token,next_token)
        self.assertNotEqual(old_origin,new._brokerrouter_model_origin)

    def test_history_missing_or_mutated_rows_never_count_as_durable_ack(self):
        agent = self.agent()
        result = self.conversation(agent)
        self.finish(result)
        released = agent._brokerrouter_released_final
        self.assertTrue(self.module.history_is_ready(self.store,self.entry.session_key,released))
        self.db._execute_write(lambda conn:conn.execute("UPDATE messages SET content='changed local row' WHERE role='assistant'"))
        self.assertFalse(self.module.history_is_ready(self.store,self.entry.session_key,released))
        with self.assertRaises(self.Halt): self.module.append_ready(agent,self.store,self.entry.session_key,released)
        with self.assertRaises(self.Halt): self.send(released.text)
        self.assertEqual(self.sends,[])

    def test_plain_constructed_sendresult_cannot_clear_resume_without_platform_ack(self):
        agent = self.agent()
        result = self.conversation(agent)
        self.finish(result)
        self.store.mark_resume_pending(self.entry.session_key,"still-held")
        released = agent._brokerrouter_released_final
        self.module.observe_send(self.adapter,self.event,self.entry.session_key,released,
                                 self.SendResult(success=True,message_id="forged-local-ack"),{},None)
        self.assertTrue(self.entry.resume_pending)
        self.assertTrue(self.send(released.text).success)
        self.assertFalse(self.entry.resume_pending)

    def test_metadata_http200_drift_and_oversized_wire_fail_closed(self):
        agent = self.agent()
        result = self.conversation(agent)
        type(self).mutator = lambda response:response.update(bytes=True)
        with self.assertRaises(self.Halt): self.finish(result)
        self.assertEqual(self.db.get_messages(self.entry.session_id),[])
        type(self).mutator = None
        with self.assertRaises(self.Halt):
            self.module.freeze(agent,"x"*(512*1024),result["messages"])
        self.assertEqual(self.sends,[])

    def test_known_model_effect_without_frozen_final_cannot_restart_dispatch(self):
        agent = self.agent()
        self.conversation(agent)
        restored = self.case.agent(bind=False)
        restored.session_id = self.entry.session_id
        self.case.bind(restored,self.case.context,self.runner)
        self.case.module.bind_model_origin(restored,self.case.context)
        count = len(self.case.posts)
        with self.assertRaises(self.Halt): self.module.prepare(restored,self.case.context,self.store,self.runner)
        self.assertEqual(len(self.case.posts),count)
        self.assertEqual(self.db.get_messages(self.entry.session_id),[])

    def test_actual_sender_rejects_route_message_run_and_generation_drift(self):
        agent = self.agent()
        result = self.conversation(agent)
        text,_ = self.finish(result)
        original = self.event.source
        for change in ("chat_id","thread_id","profile","scope_id"):
            with self.subTest(route=change):
                values = {name:getattr(original,name,None) for name in
                          ("platform","chat_id","thread_id","profile","scope_id")}
                values[change] = "different-native-routing-owner"
                self.event.source = SimpleNamespace(**values)
                with self.assertRaises(self.Halt): self.send(text)
        self.event.source = original
        self.event.message_id = "new-inbound-message"
        with self.assertRaises(self.Halt): self.send(text)
        self.event.message_id = self.case.context.inbound_message_id
        self.event.metadata["brokerrouter_turn"]["run_id"] = "new-run"
        with self.assertRaises(self.Halt): self.send(text)
        self.event.metadata["brokerrouter_turn"]["run_id"] = agent._brokerrouter_model_origin.run_id
        self.runner._begin_session_run_generation(self.entry.session_key)
        with self.assertRaises(self.Halt): self.send(text)
        self.assertEqual(self.sends,[])
        self.assertEqual(len(self.case.posts),1)

    def test_real_release_identity_matrix_never_accepts_http200_as_grant(self):
        agent = self.agent()
        result = self.conversation(agent)
        type(self).record_loss = True
        with self.assertRaises(self.Halt): self.finish(result)
        for field,value in (("publication_id",str(uuid4())),("output_operation_id",str(uuid4())),
                            ("id",str(uuid4())),("payload_hash","0"*64),("text_sha256","0"*64),
                            ("turn_id",str(uuid4())),("model_id","another-model"),
                            ("source_request_ids",[str(uuid4())]),("bytes",True),
                            ("text","modified final"),("expires_at","2000-01-01T00:00:00Z")):
            with self.subTest(field=field):
                def mutate(response, field=field, value=value):
                    if response.get("state") != "recorded": response[field] = value
                type(self).mutator = mutate
                with self.assertRaises(self.Halt): self.module.release(agent)
                self.assertEqual(self.db.get_messages(self.entry.session_id),[])
                self.assertEqual(self.sends,[])
        type(self).mutator = None
        self.assertEqual(len(self.case.posts),1)

    def test_native_cursor_and_private_final_are_immutable_and_expiry_erases_only_bytes(self):
        agent = self.agent()
        result = self.conversation(agent)
        self.finish(result)
        released = agent._brokerrouter_released_final
        with self.module._transaction() as connection:
            row = self.module._row(connection,released.origin)
            for sql in ("UPDATE brokerrouter_final_candidates SET text_sha256='changed'",
                        "UPDATE brokerrouter_final_candidates SET result_ciphertext=X'0000'",
                        "UPDATE brokerrouter_final_candidates SET work_ciphertext=X'0000'",
                        "UPDATE brokerrouter_final_candidates SET model='changed'",
                        "DELETE FROM brokerrouter_final_candidates",
                        "INSERT OR REPLACE INTO brokerrouter_final_candidates SELECT * FROM brokerrouter_final_candidates"):
                with self.subTest(sql=sql):
                    with self.assertRaises(sqlite3.IntegrityError): connection.execute(sql)
        for sql in ("UPDATE brokerrouter_final_history SET projection_sha256='changed'",
                    "DELETE FROM brokerrouter_final_history",
                    "INSERT OR REPLACE INTO brokerrouter_final_history SELECT * FROM brokerrouter_final_history"):
            with self.subTest(sql=sql):
                with self.assertRaises(sqlite3.IntegrityError):
                    self.db._execute_write(lambda connection,sql=sql:connection.execute(sql))
        with patch.object(self.module.time,"time_ns",return_value=row["expires_at_ns"]+1):
            with self.assertRaises(self.Halt): self.module.release(agent)
            with self.module._transaction() as connection:
                expired = self.module._row(connection,released.origin)
        self.assertIsNone(expired["result_ciphertext"])
        self.assertIsNone(expired["work_ciphertext"])
        self.assertEqual(expired["text_sha256"],released.text_sha256)
        self.assertEqual(expired["publication_id"],released.publication_id)
        self.assertEqual(expired["sources"],row["sources"])
        self.assertEqual(len(self.case.posts),1)

    def test_actual_gateway_outer_handler_formats_freezes_and_sends_after_release(self):
        from gateway.platforms.event import MessageEvent
        # This is the real GatewayRunner/TurnRunner path with a configured
        # native /model runtime, not a fake runner or model factory.
        self.runner._session_state(self.entry.session_key).conversation.model_override = {
            "model":"contract-model","provider":"openai","requested_provider":"openai",
            "api_key":self.case.key,"base_url":self.case.url+"/v1","api_mode":"chat_completions",
        }
        self.runner.adapters[self.Platform.LOCAL] = self.adapter
        os.environ.update(self.case.required)
        event = MessageEvent(text="zero fee actual outer handler",source=self.source,
            message_id=self.case.context.inbound_message_id,metadata={"brokerrouter_turn":dict(self.case.context._brokerrouter_turn_binding)})
        response = asyncio.run(self.runner._handle_message_with_agent(
            event,self.source,self.entry.session_key,self.case.context.run_generation))
        self.assertIn("local no-fee model fixture",response)
        self.assertEqual(len(self.case.posts),1)
        self.assertTrue(type(self).final_records)
        self.assertEqual(self.db.get_messages(self.entry.session_id)[-1]["content"],response)
        result,_ = asyncio.run(self.adapter.send_final_ledgered(event,self.entry.session_key,response,{},reply_to=None))
        self.assertTrue(result.success)
        self.assertEqual(self.sends,[response])

    def test_actual_builtin_status_control_reply_sends_without_final_source_or_model(self):
        from gateway.platforms.event import MessageEvent
        from gateway.platforms.base import EphemeralReply
        os.environ.update(self.case.required)
        event = MessageEvent(text="/status",source=self.source,message_id="native-status-control")
        handled,response = asyncio.run(self.runner._hm_dispatch_idle_commands(event,self.source,self.entry.session_key))
        self.assertTrue(handled)
        self.assertIsInstance(response,EphemeralReply)
        content,_ = self.adapter._unwrap_ephemeral(response)
        self.assertIn(self.entry.session_id,content)
        result,_ = asyncio.run(self.adapter.send_final_ledgered(event,self.entry.session_key,content,{},
            reply_to=None,is_ephemeral_response=True))
        self.assertTrue(result.success)
        self.assertEqual(self.sends,[content])
        self.assertEqual(self.case.posts,[])
        self.assertEqual(type(self).final_calls,[])

    def test_unknown_command_extensions_do_not_gain_control_authority_and_rewrites_remain_model_turns(self):
        from gateway.platforms.event import MessageEvent
        os.environ.update(self.case.required)
        invoked = []
        with patch("hermes_cli.plugins.fire_pre_command_hook",side_effect=lambda **kw:invoked.append("pre")):
            result = asyncio.run(self.runner._hm_command_hooks(
                MessageEvent(text="/status",source=self.source),self.source,self.entry.session_key,"status","status"))
        self.assertEqual(result,(False,None,None))
        self.assertEqual(invoked,[])
        self.runner.config.quick_commands = {"private-exec":{"type":"exec","command":"echo ungoverned"}}
        with patch("asyncio.create_subprocess_shell",side_effect=AssertionError("unexpected exec")):
            with self.assertRaises(self.Halt):
                asyncio.run(self.runner._hm_dispatch_quick_and_plugin_commands(
                    MessageEvent(text="/private-exec",source=self.source),self.source,"private-exec"))
        with patch("hermes_cli.plugins.get_plugin_command_handler",return_value=lambda _args:invoked.append("plugin")):
            with self.assertRaises(self.Halt):
                asyncio.run(self.runner._hm_dispatch_quick_and_plugin_commands(
                    MessageEvent(text="/private-plugin",source=self.source),self.source,"private-plugin"))
        for command in ("steer","queue"):
            with self.subTest(command=command):
                event = MessageEvent(text="/"+command+" /model-looking-user-payload",source=self.source)
                handled,response = asyncio.run(self.runner._hm_dispatch_idle_commands(event,self.source,self.entry.session_key))
                self.assertFalse(handled)
                self.assertIsNone(response)
                self.assertEqual(event.text,"/model-looking-user-payload")
        self.assertEqual(invoked,[])
        self.assertEqual(self.case.posts,[])
        self.assertEqual(self.sends,[])

    def test_off_builtin_control_reply_retains_original_plain_string_behavior(self):
        from gateway.platforms.event import MessageEvent
        os.environ["BROKERROUTER_HERMES_GOVERNANCE"] = "off"
        event = MessageEvent(text="/status",source=self.source,message_id="off-native-control")
        handled,response = asyncio.run(self.runner._hm_dispatch_idle_commands(event,self.source,self.entry.session_key))
        self.assertTrue(handled)
        self.assertIs(type(response),str)
        result,_ = asyncio.run(self.adapter.send_final_ledgered(event,self.entry.session_key,response,{},reply_to=None))
        self.assertTrue(result.success)
        self.assertEqual(self.sends,[response])

    def test_required_refine_does_not_export_private_cached_pending_messages(self):
        from gateway.platforms.event import MessageEvent
        agent = self.agent()
        result = self.conversation(agent)
        self.runner._agent_cache[self.entry.session_key] = (agent, None)
        self.assertTrue(agent._session_messages)
        self.assertEqual(self.db.get_messages(self.entry.session_id), [])
        exports = []
        for state in ("waiting_approval", "rejected"):
            with self.subTest(state=state):
                type(self).states = [state]
                with self.assertRaises(self.Halt):
                    self.finish(result)
                with patch.object(agent, "_spawn_background_review", side_effect=lambda **kw: exports.append(
                        any(row.get("content") == result["final_response"] for row in kw["messages_snapshot"]))):
                    handled, reply = asyncio.run(self.runner._hm_dispatch_idle_commands(
                        MessageEvent(text="/refine", source=self.source), self.source, self.entry.session_key))
                self.assertTrue(handled)
        self.assertEqual(exports, [])
        self.assertEqual(self.db.get_messages(self.entry.session_id), [])
        self.assertEqual(self.sends, [])

    def test_required_save_never_creates_export_file_before_binary_rejection(self):
        import tempfile
        from gateway.platforms.event import MessageEvent
        self.agent()
        self.db.append_message(self.entry.session_id, "user", "synthetic stored export fixture")
        os.environ.update(self.case.required)
        self.runner.adapters[self.Platform.LOCAL] = self.adapter
        with patch("tempfile.mkdtemp", wraps=tempfile.mkdtemp) as directories:
            handled, reply = asyncio.run(self.runner._hm_dispatch_idle_commands(
                MessageEvent(text="/save md", source=self.source), self.source, self.entry.session_key))
        self.assertTrue(handled)
        directories.assert_not_called()
        self.assertEqual(self.sends, [])

    def test_required_unsupported_native_lanes_block_before_idle_and_busy_handlers(self):
        from gateway.platforms.event import MessageEvent
        from gateway.platforms.base import EphemeralReply
        from hermes_cli.commands import resolve_command
        os.environ.update(self.case.required)
        commands = ("review", "refine", "bg", "btw", "compress", "retry", "goal", "subgoal",
                    "heartbeat", "loop", "moa", "save", "debug", "diff", "memory", "skills",
                    "compact", "hb")
        for command in commands:
            with self.subTest(command=command):
                canonical = resolve_command(command).name
                name = ("_hm_cmd_moa" if canonical == "moa" else self.runner._COMMAND_HANDLER_ALIASES.get(
                    canonical, "_handle_" + canonical + "_command"))
                event = MessageEvent(text="/" + command + " private-argument-must-not-echo", source=self.source)
                with patch.object(self.runner, name, side_effect=AssertionError("unexpected native handler")) as handler, \
                     patch.object(self.runner, "_cached_agent_for", side_effect=AssertionError("unexpected cache read")) as cache, \
                     patch.object(self.runner, "_dispatch_busy_slash_command", side_effect=AssertionError("unexpected busy dispatch")) as busy:
                    handled, reply = asyncio.run(self.runner._hm_dispatch_idle_commands(event, self.source, self.entry.session_key))
                    self.assertTrue(handled)
                    self.assertIsInstance(reply, EphemeralReply)
                    text, _ = self.adapter._unwrap_ephemeral(reply)
                    self.assertNotIn("private-argument", text)
                    self.assertIn("unavailable", text)
                    handled, reply = asyncio.run(self.runner._hm_busy_slash_or_photo(event, self.source, self.entry.session_key))
                    self.assertTrue(handled)
                    self.assertEqual(reply, text)
                    handler.assert_not_called()
                    cache.assert_not_called()
                    busy.assert_not_called()
        self.assertEqual(self.case.posts, [])
        self.assertEqual(type(self).final_calls, [])
        self.assertEqual(self.sends, [])

    def test_off_native_refine_and_file_export_keep_actual_handler_behavior(self):
        import tempfile
        from gateway.platforms.event import MessageEvent
        from hermes_cli import session_export
        agent = self.agent()
        result = self.conversation(agent)
        self.runner._agent_cache[self.entry.session_key] = (agent, None)
        self.db.append_message(self.entry.session_id, "user", "synthetic ordinary export fixture")
        self.runner.adapters[self.Platform.LOCAL] = self.adapter
        os.environ["BROKERROUTER_HERMES_GOVERNANCE"] = "off"
        with patch.object(agent, "_spawn_background_review") as review:
            handled, reply = asyncio.run(self.runner._hm_dispatch_idle_commands(
                MessageEvent(text="/refine", source=self.source), self.source, self.entry.session_key))
        self.assertTrue(handled)
        self.assertIs(type(reply), str)
        review.assert_called_once()
        self.assertTrue(any(row.get("content") == result["final_response"] for row in review.call_args.kwargs["messages_snapshot"]))
        exported = []
        original_render = session_export.render_session_for_save
        def render(*args, **kwargs):
            rendered = original_render(*args, **kwargs)
            exported.append("synthetic ordinary export fixture" in rendered)
            return rendered
        with patch("tempfile.mkdtemp", wraps=tempfile.mkdtemp) as directories, \
             patch.object(session_export, "render_session_for_save", side_effect=render) as renderer:
            handled, reply = asyncio.run(self.runner._hm_dispatch_idle_commands(
                MessageEvent(text="/save md", source=self.source), self.source, self.entry.session_key))
        self.assertTrue(handled)
        # This fixed upstream handler writes the export, then calls the absent
        # GatewayRunner.get_adapter. Preserve that off-mode behavior rather than
        # invent a fake native method to claim a successful document delivery.
        self.assertEqual(reply, "Error exporting session: 'GatewayRunner' object has no attribute 'get_adapter'")
        directories.assert_called_once()
        renderer.assert_called_once()
        self.assertEqual(exported, [True])

    def test_required_status_context_help_never_project_private_cached_text(self):
        from gateway.platforms.event import MessageEvent
        from gateway.platforms.base import EphemeralReply
        agent = self.agent()
        result = self.conversation(agent)
        self.runner._agent_cache[self.entry.session_key] = (agent, None)
        pending = result["final_response"]
        for command in ("status", "context", "help"):
            with self.subTest(command=command):
                event = MessageEvent(text="/" + command, source=self.source, message_id="control-" + command)
                handled, reply = asyncio.run(self.runner._hm_dispatch_idle_commands(event, self.source, self.entry.session_key))
                self.assertTrue(handled)
                self.assertIsInstance(reply, EphemeralReply)
                text, _ = self.adapter._unwrap_ephemeral(reply)
                self.assertNotIn(pending, text)
                acknowledgement, _ = asyncio.run(self.adapter.send_final_ledgered(event, self.entry.session_key,
                    text, {}, reply_to=None, is_ephemeral_response=True))
                self.assertTrue(acknowledgement.success)
        self.assertEqual(len(self.sends), 3)
        self.assertEqual(len(self.case.posts), 1)
        self.assertEqual(type(self).final_calls, [])
        self.assertEqual(self.db.get_messages(self.entry.session_id), [])


if __name__ == "__main__":
    unittest.main()
