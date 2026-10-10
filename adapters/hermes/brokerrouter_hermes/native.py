"""Final text boundary for the one manifest-verified Hermes executor.

Required mode has an empty allowlist for optional Relay and execution middleware:
those Python continuations may publish their own raw results and are not a sandbox.
Core input permission checks and pre-tool argument modifiers remain in the host.
"""

from __future__ import annotations

from contextlib import contextmanager
from contextvars import ContextVar
from typing import Any

from .gate import GovernanceHalt, context_for_agent, gate_for_agent
from .config import Config


_output_quarantined: ContextVar[bool] = ContextVar("brokerrouter_native_output", default=False)


def output_is_quarantined() -> bool:
    return _output_quarantined.get()


def binary_delivery_disabled() -> bool:
    # Text approval metadata is never an artifact grant. Until the trusted byte
    # pipeline exists, required deployments expose no binary output capability.
    return Config.from_env() is not None


def restrict_binary_capabilities(adapter: Any) -> None:
    """Close fixed platform attachment/voice APIs before any provider or send.

    This operates on the ordinary BasePlatformAdapter interface, including
    subclass overrides. Arbitrary tool/plugin SDK calls are trusted-code I/O,
    outside this interface and outside any Python sandbox claim.
    """
    if not binary_delivery_disabled():
        return

    async def unsupported(*_args, **_kwargs):
        from gateway.platforms.base import SendResult

        return SendResult(success=False, error="brokerrouter_binary_artifact_release_unsupported")

    async def unsupported_stream(*_args, **_kwargs):
        # These protocol methods return handles/None, not SendResult. Failing
        # before opening/writing prevents a fake metadata/handle from playing PCM.
        raise RuntimeError("brokerrouter_binary_artifact_release_unsupported")

    async def no_synthesis(*_args, **_kwargs):
        return [], None

    for name in (
        "send_multiple_images", "send_image", "send_animation", "send_voice",
        "send_video", "send_document", "send_image_file", "send_audio", "send_file",
        "send_media", "send_sticker", "play_tts", "play_in_voice_channel",
    ):
        if callable(getattr(adapter, name, None)):
            setattr(adapter, name, unsupported)
    for name in ("begin_streaming_tts", "write_streaming_tts", "finish_streaming_tts"):
        if callable(getattr(adapter, name, None)):
            setattr(adapter, name, unsupported_stream)
    adapter.supports_streaming_tts = lambda *_args, **_kwargs: False
    adapter._should_auto_tts_for_chat = lambda *_args, **_kwargs: False
    adapter._synthesize_auto_tts = no_synthesis


@contextmanager
def quarantine_tool_output():
    """Keep ordinary registry hooks and error logs behind final policy release.

    This is a host pipeline boundary. Tool code itself is trusted and retains its
    configured filesystem/network capabilities; Python is not a sandbox.
    """
    from model_tools import suppress_post_tool_call_hook

    token = _output_quarantined.set(True)
    try:
        with suppress_post_tool_call_hook():
            yield
    finally:
        _output_quarantined.reset(token)


class NativeInvocation:
    def __init__(self, agent: Any, ref: Any, gate: Any):
        self.agent = agent
        self.gate = gate
        self.origin = context_for_agent(agent, ref)

    @classmethod
    def for_call(cls, agent: Any, ref: Any) -> NativeInvocation | None:
        gate = gate_for_agent(agent)
        return None if gate is None else cls(agent, ref, gate)

    def _callbacks(self) -> dict:
        return {
            "status": lambda message: self.agent._emit_status(message),
            "approval": getattr(self.agent, "_brokerrouter_approval_callback", None),
        }

    def open(self) -> str | None:
        # The gate binds original args/model before any host modifier. A frozen
        # result goes through current output policy without reentering the tool.
        return self.gate.native_open(self.origin, **self._callbacks())

    def finish(self, ref: Any, result: Any) -> str:
        try:
            if type(result) is not str:
                raise GovernanceHalt("native_text_result_required")
            from agent.tool_executor import _detect_tool_failure

            failed, _ = _detect_tool_failure(ref.name, result)
            final_text = self.agent._append_guardrail_observation(
                ref.name, ref.args, result, failed=failed, tool_call_id=ref.call_id,
            )
            if type(final_text) is not str:
                raise GovernanceHalt("native_text_result_required")
            hints = self.agent._subdirectory_hints.check_tool_call(ref.name, ref.args)
            if hints:
                final_text += hints
            # Freeze after the host's last content transformation. The gate owns
            # immutable execution evidence, bounded ciphertext and policy release.
            return self.gate.finish_native(
                context_for_agent(self.agent, ref), final_text, **self._callbacks(),
            )
        except GovernanceHalt:
            raise
        except Exception as exc:
            # An observer failure can include raw text in its exception. Never let
            # the host's ordinary error logger or error-result builder receive it.
            raise GovernanceHalt("native_finalization_failed") from exc
