from __future__ import annotations

import json
import os
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from uuid import uuid4


@unittest.skipUnless(
    os.environ.get("BROKERROUTER_HERMES_CONTRACT") == "1",
    "requires an installed fixed-revision Hermes overlay",
)
class HermesHostContractTest(unittest.TestCase):
    def setUp(self):
        from agent import tool_executor

        self.module = tool_executor
        self.agent = SimpleNamespace(
            _brokerrouter_turn_id=str(uuid4()),
            session_id="session-fixture",
            model="logical-model",
            _turns_since_memory=4,
            _iters_since_skill=4,
            _tool_guardrail_halt_decision=None,
            _tool_guardrails=SimpleNamespace(
                before_call=lambda _name, _args: SimpleNamespace(allows_execution=True)
            ),
            _emit_status=lambda _message: None,
            _guardrail_block_result=lambda decision: json.dumps(
                {"error": decision.code}, separators=(",", ":")
            ),
        )

    def managed(self):
        return self.module._ManagedToolResult(
            result=None, args={"original": True}, middleware_trace=[], blocked=False, dispatched=True
        )

    def ref(self):
        return self.module._ToolCallRef(
            "send_message", {"original": True}, "task-1", "call-1", []
        )

    def test_gate_sees_final_modified_args_before_the_only_dispatch(self):
        import brokerrouter_hermes

        observed = []
        began = []
        dispatched = []

        class Gate:
            def run_tool(_self, context, dispatch, status=None, approval=None):
                observed.append(context.arguments)
                self.assertIs(
                    approval,
                    getattr(self.agent, "_brokerrouter_approval_callback", None),
                )
                if status:
                    status("checking")
                return dispatch()

        state = self.managed()
        ref = self.ref()
        with (
            patch.object(self.module, "_pre_tool_block", return_value=(None, {"modified": "final"})),
            patch.object(self.module, "_begin_tool_execution", side_effect=lambda *_: began.append(True)),
            patch.object(
                self.module,
                "_run_with_activity_heartbeat",
                side_effect=lambda _agent, _name, callback: callback(),
            ),
            patch.object(brokerrouter_hermes, "gate_for_agent", return_value=Gate()),
        ):
            result = self.module._dispatch_authorized_once(
                self.agent,
                state,
                ref,
                execute=lambda arguments: dispatched.append(arguments) or "ok",
                scope_block=None,
                display_index=1,
                begin_execution=None,
                authorization_gate=None,
            )
        self.assertEqual(result, "ok")
        self.assertEqual(observed, [{"modified": "final"}])
        self.assertEqual(dispatched, [{"modified": "final"}])
        self.assertEqual(began, [True])
        self.assertEqual(state.args, {"modified": "final"})

    def test_governance_halt_uses_hermes_structured_hard_stop_without_dispatch(self):
        import brokerrouter_hermes
        from brokerrouter_hermes import GovernanceHalt

        class Gate:
            def run_tool(self, _context, _dispatch, status=None, approval=None):
                raise GovernanceHalt("claim_response_unknown", str(uuid4()))

        state = self.managed()
        dispatched = []
        with (
            patch.object(self.module, "_pre_tool_block", return_value=(None, {"modified": "final"})),
            patch.object(self.module, "_emit_terminal_post_tool_call", return_value=None),
            patch.object(brokerrouter_hermes, "gate_for_agent", return_value=Gate()),
        ):
            result = self.module._dispatch_authorized_once(
                self.agent,
                state,
                self.ref(),
                execute=lambda arguments: dispatched.append(arguments),
                scope_block=None,
                display_index=1,
                begin_execution=None,
                authorization_gate=None,
            )
        self.assertEqual(dispatched, [])
        self.assertTrue(state.blocked)
        self.assertEqual(
            self.agent._tool_guardrail_halt_decision.code,
            "brokerrouter_claim_response_unknown",
        )
        self.assertEqual(json.loads(result)["error"], "brokerrouter_claim_response_unknown")


if __name__ == "__main__":
    unittest.main()
