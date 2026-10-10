"""Contracts executed from the complete pinned tool_executor source.

Dependencies at the source boundary are controlled fixtures. These tests verify
the real executor functions and overlay placement; they are not a full native
Hermes import, tool matrix, deployment or publication-dedup certification.
"""

from __future__ import annotations

import ast
import contextlib
from dataclasses import dataclass
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import threading
import time
from types import ModuleType, SimpleNamespace
from typing import Any, Callable, Optional
import unittest
from unittest.mock import patch
from uuid import uuid4

from brokerrouter_hermes.gate import GovernanceHalt
from brokerrouter_hermes import native


SPEC = importlib.util.spec_from_file_location(
    "native_source_installer", Path(__file__).resolve().parents[1] / "install.py"
)
assert SPEC and SPEC.loader
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


class NativeSourceContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        source_root = os.environ.get("BROKERROUTER_HERMES_SOURCE")
        if not source_root:
            raise unittest.SkipTest("requires complete pinned original tool_executor source")
        path = Path(source_root) / "agent/tool_executor.py"
        source = path.read_bytes()
        if hashlib.sha256(source).hexdigest() != installer.TOOL_EXECUTOR_SHA256:
            raise AssertionError("native source contract received drifted original Hermes source")
        patched = installer.patched_bytes(source)
        from brokerrouter_hermes.integrity import HOST_FILES

        if hashlib.sha256(patched).hexdigest() != HOST_FILES["agent/tool_executor.py"]["patched_sha256"]:
            raise AssertionError("native source contract patched hash differs from integrity manifest")
        tree = ast.parse(patched, str(path))
        names = {
            "_ToolCallRef", "_ManagedToolResult", "_ToolOutcome", "_ToolTimeoutResult",
            "_ToolCancelledResult", "_ConcurrentBatch", "_dispatch_authorized_once",
            "_run_agent_tool_execution_middleware", "_blocked_tool_result",
            "_commit_tool_result", "_emit_tool_complete_and_risk",
            "_publish_sequential_result", "_run_sequential_call",
        }
        nodes = [n for n in tree.body if isinstance(n, (ast.ClassDef, ast.FunctionDef)) and n.name in names]
        if {n.name for n in nodes} != names:
            raise AssertionError("fixed host source contract functions are missing")
        cls.code = compile(ast.fix_missing_locations(ast.Module(
            body=[ast.ImportFrom(module="__future__", names=[ast.alias(name="annotations")], level=0), *nodes],
            type_ignores=[],
        )), str(path), "exec")

    def setUp(self):
        self.events = []
        self.released = False
        self.ref_arguments = {"original": True}
        self.dispatches = 0
        self.recovered = None
        self.hold = False
        self.observer_failure = False
        self.observed = []
        self.checked = []
        self.starts = []
        self.ns = {
            "__name__": "agent.tool_executor", "dataclass": dataclass, "Any": Any,
            "Callable": Callable, "Optional": Optional, "threading": threading,
            "time": time, "contextlib": contextlib, "json": json,
            "logging": SimpleNamespace(debug=lambda *args: self.sink("debug-log", str(args))),
            "_pre_tool_block": lambda _a, _ref: (None, {"final": True}),
            "_begin_tool_execution": lambda *_: self.events.append("begin"),
            "_run_with_activity_heartbeat": lambda _a, _n, callback: callback(),
            "tool_hook_ids": lambda *_: {}, "_detect_tool_failure": lambda *_: (False, None),
            "_safe_callback": lambda callback, _label, *args, **kwargs: callback(*args, **kwargs) if callback else None,
            "_redact_tool_args_for_display": lambda _name, args: args,
            "_is_multimodal_tool_result": lambda _: False,
            "_record_persisted_path_for_stub": lambda *_: None,
            "get_active_env": lambda _: None,
            "_multimodal_text_summary": str,
            "_tool_progress_enabled": lambda _: False,
        }
        host = ModuleType("agent.tool_executor")
        host.__dict__.update(self.ns)
        self.modules = patch.dict(sys.modules, {"agent.tool_executor": host})
        self.modules.start()
        exec(self.code, host.__dict__)
        self.ns = host.__dict__
        agent_package = ModuleType("agent")
        agent_package.tool_executor = host
        guardrails = ModuleType("agent.tool_guardrails")
        guardrails.ToolGuardrailDecision = lambda **fields: SimpleNamespace(**fields)
        relay = ModuleType("agent.relay_tools")
        relay.execute = self.untrusted_relay
        agent_package.relay_tools = relay
        middleware = ModuleType("hermes_cli.middleware")
        middleware.apply_tool_request_middleware = lambda _name, args, **_: SimpleNamespace(payload=args, trace=[])
        middleware.run_tool_execution_middleware = lambda _name, args, dispatch, **_: dispatch(args)
        model_tools = ModuleType("model_tools")
        model_tools.suppress_post_tool_call_hook = contextlib.nullcontext
        self.extras = patch.dict(sys.modules, {
            "agent": agent_package, "agent.tool_guardrails": guardrails,
            "agent.relay_tools": relay, "hermes_cli": ModuleType("hermes_cli"),
            "hermes_cli.middleware": middleware,
            "model_tools": model_tools,
        })
        self.extras.start()
        self.agent = SimpleNamespace(
            _brokerrouter_turn_id=str(uuid4()), session_id="fixture", model="model",
            _tool_guardrails=SimpleNamespace(before_call=lambda *_: SimpleNamespace(allows_execution=True)),
            _tool_guardrail_halt_decision=None, _emit_status=lambda _: None,
            _guardrail_block_result=lambda decision: json.dumps({"error": decision.code}),
            _append_guardrail_observation=self.observe,
            _subdirectory_hints=SimpleNamespace(check_tool_call=self.hints),
            _record_file_mutation_result=lambda *_: self.events.append("file-observer"),
            _touch_activity=lambda *_: None, verbose_logging=True,
            _tool_result_content_for_active_model=lambda _name, result: result,
            tool_progress_callback=lambda *args, **_: self.events.append(args[0]),
            tool_complete_callback=lambda *_: self.events.append("complete-callback"),
        )
        self.ns["logger"] = SimpleNamespace(
            info=lambda *_: self.events.append("info-log"),
            warning=lambda *_: self.events.append("warning-log"),
            error=lambda *_args, **_kwargs: self.events.append("error-log"),
        )
        self.ns["_emit_terminal_post_tool_call"] = lambda _agent, **kw: self.sink("post-hook", kw["result"])
        self.ns["maybe_persist_tool_result"] = lambda **kw: self.sink("spill", kw["content"])
        self.ns["make_tool_result_message"] = lambda _name, result, _id, **_: {"content": self.sink("history-append", result)}
        self.ns["_flush_session_db_after_tool_progress"] = lambda *_args, **_kwargs: self.events.append("history-flush") or True
        outer = self

        class Authority:
            def native_open(self, context, **_):
                outer.events.append("open")
                outer.assertEqual(context.arguments, outer.ref_arguments)
                if outer.recovered is not None:
                    outer.released = True
                return outer.recovered

            def run_execution(self, context, dispatch, **_):
                outer.events.append("input")
                outer.assertEqual(context.arguments, {"final": True})
                return dispatch()

            def finish_native(self, context, text, **_):
                outer.events.append("freeze")
                outer.checked.append(text)
                if outer.hold:
                    raise GovernanceHalt("approval_wait_timeout")
                outer.released = True
                outer.events.append("release")
                return text

        authority = Authority()
        self.native_gate = patch.object(native, "gate_for_agent", return_value=authority)
        self.native_gate.start()
        import brokerrouter_hermes

        self.input_gate = patch.object(brokerrouter_hermes, "gate_for_agent", return_value=authority)
        self.input_gate.start()

    def tearDown(self):
        self.input_gate.stop()
        self.native_gate.stop()
        self.extras.stop()
        self.modules.stop()

    def sink(self, name, text):
        if isinstance(text, str) and "RAW" in text:
            self.assertTrue(self.released, f"{name} received protected text before release")
        self.events.append(name)
        return text

    def observe(self, _name, _args, text, **_):
        self.observed.append(text)
        self.events.append("observe")
        if self.observer_failure:
            raise RuntimeError("RAW private exception")
        return text + "|observation"

    def hints(self, *_):
        self.events.append("hint")
        return "|hint"

    def untrusted_relay(self, _name, args, dispatch, **_):
        self.events.append("untrusted-relay")
        return "RISK_AFTER_RAW_GATE:" + dispatch(args), args

    def execute(self, _args):
        self.dispatches += 1
        self.events.append("dispatch")
        return "RAW"

    def ref(self):
        return self.ns["_ToolCallRef"]("fixture", self.ref_arguments, "task", "call", [])

    def run_outer(self, scope_block=None):
        return self.ns["_run_agent_tool_execution_middleware"](
            self.agent, **self.ref().middleware_kwargs(), execute=self.execute,
            begin_execution=lambda callback: self.starts.append(True) or (callback() if callback else None),
            scope_block=scope_block,
        )

    def publish(self, managed):
        messages = []
        self.assertTrue(self.ns["_publish_sequential_result"](
            self.agent, messages, self.ref(), managed, tool_duration=1, index=1, budget=None,
        ))
        return messages

    def test_final_observation_and_hint_are_frozen_before_sequential_publication(self):
        managed = self.run_outer()
        self.assertEqual(managed.result, "RAW|observation|hint")
        self.assertEqual(self.checked, [managed.result])
        self.assertNotIn("untrusted-relay", self.events)
        self.assertEqual(self.dispatches, 1)
        self.assertEqual(self.starts, [True])
        self.publish(managed)
        self.assertEqual(self.observed, ["RAW"])
        self.assertEqual(self.events.count("hint"), 1)
        for sink in ("post-hook", "spill", "history-append", "history-flush", "tool.completed"):
            self.assertGreater(self.events.index(sink), self.events.index("release"))

    def test_modifier_and_begin_callback_aliases_cannot_change_checked_dispatch_arguments(self):
        retained = {"final": True}
        checked = []
        dispatched = []
        self.ns["_pre_tool_block"] = lambda _agent, _ref: (None, retained)

        def begin(_agent, ref, _index):
            retained["final"] = "old modifier alias changed"
            ref.args["final"] = "display callback changed"

        self.ns["_begin_tool_execution"] = begin
        state = self.ns["_ManagedToolResult"](None, {}, [], False, True)
        import brokerrouter_hermes

        class Authority:
            def run_execution(_self, context, dispatch, **_callbacks):
                checked.append(context.arguments)
                return dispatch()

        def execute(arguments):
            dispatched.append(dict(arguments))
            arguments["final"] = "executor mutation"
            return "RAW"

        with patch.object(brokerrouter_hermes, "gate_for_agent", return_value=Authority()):
            self.ns["_dispatch_authorized_once"](
                self.agent, state, self.ref(), execute=execute, scope_block=None,
                display_index=1, begin_execution=None, authorization_gate=None,
            )
        self.assertEqual(checked, [{"final": True}])
        self.assertEqual(dispatched, [{"final": True}])
        self.assertEqual(state.args, {"final": True})

    def test_parallel_worker_logs_and_history_follow_final_release(self):
        self.agent._invoke_tool = lambda *_args, **_kwargs: self.execute({"final": True})
        batch = self.ns["_ConcurrentBatch"].__new__(self.ns["_ConcurrentBatch"])
        batch.agent, batch.messages, batch.authorization_gate = self.agent, [], None
        outcome = batch._dispatch_worker(0, self.ref(), None, SimpleNamespace(advance=lambda callback=None: callback() if callback else None))
        self.assertEqual(outcome.result, "RAW|observation|hint")
        self.assertGreater(self.events.index("info-log"), self.events.index("release"))
        self.ns["_commit_tool_result"](
            self.agent, [], outcome.ref, outcome.result, budget=None, tool_duration=1,
            is_error=False, blocked=False, effect_disposition=None, observed=True,
        )
        self.assertEqual(self.observed, ["RAW"])

    def test_sequential_spinner_receives_only_the_final_released_text(self):
        self.ns["_run_sequential_tool_execution_middleware"] = self.ns["_run_agent_tool_execution_middleware"]
        self.ns["_finish_quiet_tool_spinner"] = lambda _agent, _spinner, _name, _args, _duration, result: self.sink("spinner", result)
        dispatch = SimpleNamespace(
            execute=self.execute, middleware_trace_arg=[], handles_keyboard_interrupt=False,
            error_result=None, is_delegate=False, finish_spinner=True, finish_in_finally=True,
            spinner=object(),
        )
        managed, _ = self.ns["_run_sequential_call"](
            self.agent, dispatch, self.ref(), scope_block=None, messages=[], remaining_calls=[],
            display_index=1, tool_start_time=time.time(),
        )
        self.assertEqual(managed.result, "RAW|observation|hint")
        self.assertGreater(self.events.index("spinner"), self.events.index("release"))

    def test_waiting_approval_projects_only_sanitized_halt_metadata(self):
        self.hold = True
        managed = self.run_outer()
        self.assertTrue(managed.blocked)
        self.assertFalse(self.released)
        self.assertNotIn("RAW", managed.result)
        self.assertEqual(self.checked, ["RAW|observation|hint"])
        self.assertEqual(self.events.count("post-hook"), 1)
        self.assertNotIn("spill", self.events)

    def test_gate_configuration_halt_uses_structured_stop_without_dispatch(self):
        self.native_gate.stop()
        with patch.object(native, "gate_for_agent", side_effect=GovernanceHalt("adapter_configuration_invalid")):
            managed = self.run_outer()
        self.assertTrue(managed.blocked)
        self.assertIn("adapter_configuration_invalid", managed.result)
        self.assertEqual(self.dispatches, 0)
        self.assertEqual(self.starts, [True])

    def test_observer_exception_cannot_reach_host_error_logger_with_raw_text(self):
        self.observer_failure = True
        managed = self.run_outer()
        self.assertTrue(managed.blocked)
        self.assertNotIn("RAW", managed.result)
        self.assertIn("native_finalization_failed", managed.result)
        self.assertEqual(self.checked, [])
        self.assertNotIn("error-log", self.events)

    def test_recovery_skips_dispatch_observation_hints_and_ordinary_side_effect_callbacks(self):
        self.recovered = "RAW|frozen"
        managed = self.run_outer()
        self.assertEqual(managed.result, self.recovered)
        self.assertTrue(managed.brokerrouter_recovered)
        self.assertEqual(self.dispatches, 0)
        self.assertEqual(self.starts, [True])
        messages = self.publish(managed)
        self.assertEqual(messages[0]["content"], self.recovered)
        for unwanted in ("untrusted-relay", "observe", "hint", "post-hook", "file-observer", "tool.completed", "complete-callback"):
            self.assertNotIn(unwanted, self.events)

    def test_empty_frozen_text_is_recovery_and_never_dispatches(self):
        self.recovered = ""
        managed = self.run_outer()
        self.assertTrue(managed.brokerrouter_recovered)
        self.assertEqual(managed.result, "")
        self.assertEqual(self.dispatches, 0)

    def test_recovery_never_overrides_current_host_scope_denial(self):
        self.recovered = "RAW|frozen"
        managed = self.run_outer(scope_block="Tool is no longer enabled for this session")
        self.assertTrue(managed.blocked)
        self.assertNotIn("RAW", managed.result)
        self.assertEqual(self.dispatches, 0)
        self.assertEqual(self.starts, [True])
        self.assertNotIn("open", self.events)

    def test_binary_result_fails_closed_without_freeze_or_publication(self):
        self.execute = lambda _args: {"_multimodal": True, "image": "private-binary-reference"}
        managed = self.run_outer()
        self.assertTrue(managed.blocked)
        self.assertIn("native_text_result_required", managed.result)
        self.assertFalse(self.released)
        self.assertEqual(self.checked, [])
        self.assertEqual(self.observed, [])

    def test_off_mode_retains_upstream_optional_pipeline(self):
        self.native_gate.stop()
        self.input_gate.stop()
        import brokerrouter_hermes

        with patch.object(native, "gate_for_agent", return_value=None), patch.object(brokerrouter_hermes, "gate_for_agent", return_value=None):
            managed = self.run_outer()
        self.assertEqual(managed.result, "RISK_AFTER_RAW_GATE:RAW")
        self.assertIn("untrusted-relay", self.events)
        self.assertFalse(managed.brokerrouter_finalized)


if __name__ == "__main__":
    unittest.main()
