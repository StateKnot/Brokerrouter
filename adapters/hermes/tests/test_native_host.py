"""Run the full fixed Hermes agent, registry, executor and SQLite history.

Only zero-fee tool handlers, plugin listeners and governance responses are local
fixtures. This does not certify an external tool, model or messaging platform.
Run with BROKERROUTER_HERMES_NATIVE_CONTRACT=1 in an installed full host image.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
from pathlib import Path
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from uuid import uuid4

from brokerrouter_hermes.config import Config
from brokerrouter_hermes.gate import GovernanceGate
from test_output_gate import Clock, ResultServer


class NativeAuthority:
    """Separate atomic operation identities for actual concurrent host calls."""

    def __init__(self, turn_id, states):
        self.turn_id, self.states = turn_id, states
        self.calls, self.operations, self.results = {}, {}, {}
        self.requests, self.released = [], set()
        self.lock = threading.Lock()

    def request(self, method, path, body=None):
        with self.lock:
            self.requests.append((method, path, body))
            if path == "/v1/governance/checks":
                key = body["operation_key"]
                if key not in self.calls:
                    server = ResultServer(self.turn_id)
                    server.states = list(self.states)
                    self.calls[key] = server
                    self.operations[server.input_id] = server
                server = self.calls[key]
            elif path == "/v1/governance/tool-results":
                server = self.operations[body["input_operation_id"]]
                self.results[server.result_id] = server
            elif path.endswith("/release"):
                server = self.results[path.split("/")[-2]]
            else:
                server = self.operations[path.split("/")[-2]]
            response = server.request(method, path, body)
            if path.endswith("/release") and response["state"] == "ready":
                self.released.add(response["text"])
            return response


@unittest.skipUnless(
    os.environ.get("BROKERROUTER_HERMES_NATIVE_CONTRACT") == "1",
    "requires the complete fixed Hermes host and dependencies",
)
class NativeHostTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from run_agent import AIAgent
        from agent import tool_executor
        from hermes_state import SessionDB
        from hermes_cli import lifecycle
        from tools.registry import registry
        from brokerrouter_hermes.integrity import verify_root

        # A flag and a successful import cannot substitute for a verified overlay.
        import brokerrouter_hermes
        verify_root(Path(brokerrouter_hermes.__file__).resolve().parents[1])
        cls.Agent, cls.Executor, cls.Database = AIAgent, tool_executor, SessionDB
        cls.Lifecycle, cls.Registry = lifecycle, registry

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        self.environment = patch.dict(os.environ, {
            "HERMES_HOME": str(self.home), "BROKERROUTER_HERMES_GOVERNANCE": "off",
        })
        self.environment.start()
        (self.home / "config.yaml").write_text(
            "model:\n  default: contract-model\n  provider: openai\n  context_length: 32768\n"
            "compression:\n  enabled: false\n"
        )
        self.config = Config(
            base_url="https://fixture.invalid", approvals_url="https://fixture.invalid",
            virtual_key="sk_br_" + "1" * 64, state_db=self.home / "gate.db", state_key=b"\x17" * 32,
            request_timeout_seconds=1, approval_poll_seconds=0.25, approval_wait_seconds=1,
        )
        self.tool_name, self.toolset = "native_fixture_" + uuid4().hex, "native_fixture_" + uuid4().hex
        self.body = "PRIVATE_NATIVE_TOOL_RESULT_FIXTURE"
        self.dispatches, self.hooks, self.logs = [], [], []
        self.agents, self.databases = [], []
        self.authority = NativeAuthority(str(uuid4()), ["ready"])
        owner = self

        class Capture(logging.Handler):
            def emit(_self, record):
                owner.logs.append(logging.Formatter().format(record))

        self.capture = Capture()
        logging.getLogger().addHandler(self.capture)
        self.listeners = [
            patch.object(self.Lifecycle, "has_hook", side_effect=lambda name: name in {"post_tool_call", "transform_tool_result"}),
            patch.object(self.Lifecycle, "invoke_hook", side_effect=self.listener),
        ]
        for listener in self.listeners:
            listener.start()

    def tearDown(self):
        for agent in self.agents:
            agent.close()
        for database in self.databases:
            database.close()
        for listener in reversed(self.listeners):
            listener.stop()
        logging.getLogger().removeHandler(self.capture)
        self.Registry.deregister(self.tool_name)
        self.environment.stop()
        self.temp.cleanup()

    def listener(self, name, **payload):
        result = payload.get("result")
        if name in {"post_tool_call", "transform_tool_result"}:
            self.hooks.append((name, result))
            if self.body in str(result):
                self.assertIn(result, self.authority.released, "ordinary listener received unapproved bytes")
        return ["UNTRUSTED_TRANSFORM:" + str(result)] if name == "transform_tool_result" else []

    def register(self, handler=None, *, asynchronous=False):
        def execute(arguments, **_context):
            self.dispatches.append(dict(arguments))
            return self.body + ":" + str(arguments["number"])

        if handler is None and asynchronous:
            async def handler(arguments, **context):
                await asyncio.sleep(0)
                return execute(arguments, **context)
        self.Registry.register(
            name=self.tool_name, toolset=self.toolset,
            schema={"name": self.tool_name, "description": "deterministic zero-fee local contract tool",
                "parameters": {"type": "object", "properties": {"number": {"type": "integer"}}}},
            handler=handler or execute, is_async=asynchronous,
        )

    def agent(self, *, governed=True):
        database = self.Database(self.home / (uuid4().hex + ".db"))
        self.databases.append(database)
        agent = self.Agent(
            model="contract-model", provider="openai", api_mode="chat_completions",
            base_url="http://127.0.0.1:9/v1", api_key="invalid-contract-key",
            enabled_toolsets=[self.toolset], skip_context_files=True, skip_memory=True,
            skip_background_review=True, quiet_mode=True, session_id="native-" + uuid4().hex,
            session_db=database,
        )
        self.agents.append(agent)
        if governed:
            self.bind(agent)
        return agent, database

    def bind(self, agent):
        clock = Clock()
        agent._brokerrouter_turn_id = self.authority.turn_id
        agent._brokerrouter_governance_gate = GovernanceGate(
            self.config, transport=self.authority, sleeper=clock.sleep, monotonic=lambda: clock.now,
        )

    def run_calls(self, agent, mode, numbers=(1,)):
        calls = [SimpleNamespace(id="native-call-" + str(number), function=SimpleNamespace(
            name=self.tool_name, arguments=json.dumps({"number": number}))) for number in numbers]
        messages = [{"role": "user", "content": "Run the deterministic contract tools"}]
        execute = (self.Executor.execute_tool_calls_sequential if mode == "sequential"
                   else self.Executor.execute_tool_calls_concurrent)
        execute(agent, SimpleNamespace(tool_calls=calls), messages, "native-task", finalize=False)
        return messages

    def test_real_serial_and_parallel_hold_body_before_hooks_and_durable_history(self):
        self.register()
        self.authority.states = ["waiting_approval"]
        for mode in ("sequential", "parallel"):
            with self.subTest(mode=mode):
                agent, database = self.agent()
                messages = self.run_calls(agent, mode)
                self.assertNotIn(self.body, repr(messages))
                durable = database.get_messages(agent.session_id)
                self.assertTrue(durable, "the real host history flush did not execute")
                self.assertNotIn(self.body, repr(durable))
                self.assertTrue(agent._tool_guardrail_halt_decision.should_halt)
        self.assertEqual(self.dispatches, [{"number": 1}, {"number": 1}])
        self.assertNotIn(self.body, repr(self.hooks) + repr(self.logs))
        self.assertNotIn("transform_tool_result", [name for name, _ in self.hooks])

    def test_real_parallel_workers_publish_each_final_result_once_after_release(self):
        barrier = threading.Barrier(2)

        def handler(arguments, **_context):
            self.dispatches.append(dict(arguments))
            barrier.wait(timeout=5)
            return self.body + ":" + str(arguments["number"])

        self.register(handler)
        agent, database = self.agent()
        messages = self.run_calls(agent, "parallel", (1, 2))
        results = [message["content"] for message in messages if message["role"] == "tool"]
        self.assertEqual(set(results), self.authority.released)
        self.assertEqual(len(self.dispatches), 2)
        posts = [result for name, result in self.hooks if name == "post_tool_call" and self.body in str(result)]
        self.assertCountEqual(posts, results)
        self.assertNotIn("transform_tool_result", [name for name, _ in self.hooks])
        self.assertIn(self.body, repr(database.get_messages(agent.session_id)))

    def test_real_native_restart_rechecks_frozen_text_without_dispatch_or_ordinary_callbacks(self):
        self.register()
        agent, _ = self.agent()
        initial = self.run_calls(agent, "sequential")
        text = next(message["content"] for message in initial if message["role"] == "tool")
        self.assertEqual(self.dispatches, [{"number": 1}])
        posts_before = len(self.hooks)
        self.bind(agent)  # Fresh gate reads durable checkpoint rather than in-memory invocation state.
        self.authority.released.clear()
        for server in self.authority.calls.values():
            server.states = ["waiting_approval"]
        held = self.run_calls(agent, "sequential")
        self.assertNotIn(self.body, repr(held))
        self.bind(agent)
        for server in self.authority.calls.values():
            server.states = ["ready"]
        released = self.run_calls(agent, "sequential")
        self.assertEqual(next(message["content"] for message in released if message["role"] == "tool"), text)
        self.assertEqual(self.dispatches, [{"number": 1}])
        self.assertEqual(len([result for _, result in self.hooks[posts_before:] if self.body in str(result)]), 0)
        self.assertEqual(sum(path.endswith("/claim") for _, path, _ in self.authority.requests), 1)

    def test_real_async_registry_bridge_preserves_the_output_quarantine(self):
        self.register(asynchronous=True)
        self.authority.states = ["waiting_approval"]
        agent, database = self.agent()
        messages = self.run_calls(agent, "parallel")
        self.assertEqual(self.dispatches, [{"number": 1}])
        self.assertNotIn(self.body, repr(messages) + repr(self.hooks) + repr(self.logs))
        self.assertNotIn(self.body, repr(database.get_messages(agent.session_id)))

    def test_real_registry_exception_is_opaque_before_host_error_logs(self):
        def handler(arguments, **_context):
            self.dispatches.append(dict(arguments))
            raise RuntimeError(self.body + " private exception")

        self.register(handler)
        for mode in ("sequential", "parallel"):
            with self.subTest(mode=mode):
                agent, database = self.agent()
                messages = self.run_calls(agent, mode)
                self.assertNotIn(self.body, repr(messages) + repr(self.hooks) + repr(self.logs))
                self.assertNotIn(self.body, repr(database.get_messages(agent.session_id)))
                self.assertTrue(agent._tool_guardrail_halt_decision.should_halt)
        self.assertEqual(len(self.dispatches), 2)
        self.assertEqual(self.authority.results, {})

    def test_off_mode_retains_the_real_upstream_result_transformer(self):
        self.register()
        agent, _ = self.agent(governed=False)
        # Off-mode listeners may see the original upstream bytes by design.
        with patch.object(self.Lifecycle, "invoke_hook", side_effect=lambda name, **payload:
                          ["UNTRUSTED_TRANSFORM:" + str(payload.get("result"))]
                          if name == "transform_tool_result" else []):
            messages = self.run_calls(agent, "sequential")
        self.assertTrue(any("UNTRUSTED_TRANSFORM:" in str(message.get("content")) for message in messages))
        self.assertEqual(self.authority.requests, [])


if __name__ == "__main__":
    unittest.main()
