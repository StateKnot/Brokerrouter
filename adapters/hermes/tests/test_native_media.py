"""Use real fixed platform extraction/delivery with a zero-fee local transport.

No external platform sends or provider downloads are performed. A text release
does not authorize the local file or remote URL that the host would attach.
"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


@unittest.skipUnless(
    os.environ.get("BROKERROUTER_HERMES_NATIVE_CONTRACT") == "1",
    "requires the complete fixed Hermes platform host",
)
class NativeMediaTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from gateway.platforms.base import BasePlatformAdapter, Platform, PlatformConfig, SendResult
        from gateway.run_notifications import GatewayNotificationsMixin
        from gateway.run_turn_runner import TurnRunner
        from gateway.run_turn import GatewayTurnMixin
        from gateway.run_voice import GatewayVoiceMixin
        from tools.tts_tool import _media_tag

        cls.Base, cls.Platform, cls.PlatformConfig, cls.SendResult = BasePlatformAdapter, Platform, PlatformConfig, SendResult
        cls.Notifications, cls.TurnRunner, cls.media_tag = GatewayNotificationsMixin, TurnRunner, staticmethod(_media_tag)
        cls.Turn, cls.Voice = GatewayTurnMixin, GatewayVoiceMixin

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        self.environment = patch.dict(os.environ, {
            "HERMES_HOME": str(self.home), "BROKERROUTER_HERMES_GOVERNANCE": "required",
            "BROKERROUTER_BASE_URL": "https://fixture.invalid",
            "BROKERROUTER_HERMES_APPROVALS_URL": "https://fixture.invalid",
            "BROKERROUTER_VIRTUAL_KEY": "sk_br_" + "1" * 64,
            "BROKERROUTER_HERMES_STATE_DB": str(self.home / "gate.db"),
            "BROKERROUTER_HERMES_STATE_KEY": "17" * 32,
        })
        self.environment.start()
        self.path = self.home / "private-artifact.png"
        self.path.write_bytes(b"PRIVATE_BINARY_ARTIFACT_FIXTURE")
        self.transports, self.texts, self.outcomes = [], [], []
        owner = self

        class LocalTransport(self.Base):
            name = "local-zero-fee-transport"

            async def connect(_self, **_):
                return True

            async def disconnect(_self):
                return None

            async def get_chat_info(_self, _chat_id):
                return {"name": "local fixture", "type": "dm"}

            async def send(_self, chat_id, content, **_):
                owner.texts.append(content)
                return owner.SendResult(success=True, message_id="local-text")

            async def attachment(_self, *args, **kwargs):
                # This is the actual platform transport boundary fixture. Production
                # constructors must prevent it from receiving any path/URL in required mode.
                owner.transports.append((args, kwargs))
                return owner.SendResult(success=True, message_id="local-binary")

            send_multiple_images = send_image = send_animation = send_voice = attachment
            send_video = send_document = send_image_file = send_audio = send_file = attachment
            send_media = send_sticker = attachment
            play_tts = play_in_voice_channel = attachment

            def supports_streaming_tts(_self, *_args, **_kwargs):
                return True

            async def begin_streaming_tts(_self, *_args, **_kwargs):
                owner.transports.append(("stream-open", {}))
                return SimpleNamespace(audible=False)

            async def write_streaming_tts(_self, *_args, **_kwargs):
                owner.transports.append(("stream-write", {}))

            async def finish_streaming_tts(_self, *_args, **_kwargs):
                owner.transports.append(("stream-finish", {}))

        self.Adapter = LocalTransport
        self.adapter = self.Adapter(self.PlatformConfig(), self.Platform.TELEGRAM)
        self.event = SimpleNamespace(source=SimpleNamespace(chat_id="fixture-chat", platform=self.Platform.TELEGRAM))

    def tearDown(self):
        self.environment.stop()
        self.temp.cleanup()

    def final_delivery(self, text):
        async def deliver():
            extracted = await self.adapter._extract_response_content(
                text, self.event, "fixture-session", is_ephemeral_response=False,
            )
            await self.adapter._deliver_attachments(
                self.event, extracted, {}, anything_sent=True, record_delivery=self.outcomes.append,
            )
            return extracted
        return asyncio.run(deliver())

    def test_all_public_attachment_capabilities_reject_forged_approval_metadata(self):
        for name in ("send_multiple_images", "send_image", "send_animation", "send_voice", "send_video",
                     "send_document", "send_image_file", "send_audio", "send_file", "send_media", "send_sticker",
                     "play_tts", "play_in_voice_channel"):
            with self.subTest(method=name):
                result = asyncio.run(getattr(self.adapter, name)(
                    "fixture-chat", str(self.path), metadata={"approved": True, "artifact_sha256": "a" * 64},
                ))
                self.assertFalse(result.success)
                self.assertEqual(result.error, "brokerrouter_binary_artifact_release_unsupported")
        self.assertEqual(self.transports, [])
        self.assertEqual(self.texts, [])

    def test_streaming_voice_capabilities_and_auto_tts_are_disabled_before_provider_work(self):
        self.adapter._auto_tts_default = True
        self.assertFalse(self.adapter._should_auto_tts_for_chat("fixture-chat"))
        self.assertFalse(self.adapter.supports_streaming_tts("fixture-chat", SimpleNamespace()))
        for name in ("begin_streaming_tts", "write_streaming_tts", "finish_streaming_tts"):
            with self.subTest(method=name), self.assertRaisesRegex(RuntimeError, "brokerrouter_binary_artifact_release_unsupported"):
                asyncio.run(getattr(self.adapter, name)(SimpleNamespace()))
        self.assertEqual(self.transports, [])

    def test_explicit_gateway_voice_mode_never_calls_the_tts_provider(self):
        voice = self.Voice()
        with patch("tools.tts_tool.text_to_speech_tool", return_value=json.dumps({"success": False})) as provider:
            asyncio.run(voice._send_voice_reply(self.event, "PRIVATE_APPROVED_TEXT_FIXTURE"))
        provider.assert_not_called()
        self.assertEqual(self.transports, [])

    def test_real_final_media_bare_path_and_remote_image_extractors_cannot_send_bytes(self):
        for text in (f"MEDIA:{self.path}", str(self.path), "![private](https://fixture.invalid/private.png)"):
            with self.subTest(text_kind=text.split(":", 1)[0]):
                before = len(self.outcomes)
                extracted = self.final_delivery(text)
                self.assertTrue(extracted.images or extracted.media_files or extracted.local_files,
                                "the actual upstream attachment extractor did not run")
                self.assertTrue(self.outcomes[before:], "the actual final delivery tracker did not see rejection")
                self.assertTrue(all(not result.success for result in self.outcomes[before:]))
        self.assertEqual(self.transports, [])
        self.assertNotIn("PRIVATE_BINARY_ARTIFACT_FIXTURE", repr(self.texts))

    def test_real_tool_media_and_automatic_tag_append_cannot_upgrade_text_to_binary_grant(self):
        tool_text = self.media_tag([str(self.path)], voice_compatible=False)
        runner = self.TurnRunner.__new__(self.TurnRunner)
        response = runner._append_auto_media_tags(
            "Approved textual description", {"messages": [
                {"role": "assistant", "tool_calls": [{"id": "tts-call", "function": {"name": "text_to_speech"}}]},
                {"role": "tool", "tool_call_id": "tts-call", "content": json.dumps({"output": tool_text})},
            ]},
            [], set(),
        )
        self.assertIn("MEDIA:", response, "the actual auto attachment promotion did not run")
        self.final_delivery(response)
        self.assertEqual(self.transports, [])
        self.assertTrue(self.outcomes and all(not result.success for result in self.outcomes))

    def test_actual_gateway_streaming_tts_start_does_not_create_a_provider_consumer(self):
        self.adapter._auto_tts_default = True
        turn = self.Turn()
        turn._adapter_for_source = lambda _source: self.adapter
        holder = [None]
        with patch("gateway.streaming_tts_consumer.StreamingTTSConsumer") as consumer:
            turn._run_agent_start_streaming_tts(self.event.source, "voice", {}, holder)
        consumer.assert_not_called()
        self.assertEqual(holder, [None])
        self.assertEqual(self.transports, [])

    def test_actual_gateway_direct_voice_delivery_does_not_accept_a_local_path_grant(self):
        voice = self.Voice()
        # Required mode must stop before even resolving a platform/voice channel.
        with patch.object(voice, "_adapter_for_source", create=True) as resolve:
            asyncio.run(voice._deliver_voice_reply(self.event, [str(self.path)]))
        resolve.assert_not_called()
        self.assertEqual(self.transports, [])

    def test_real_streamed_media_delivery_cannot_call_the_platform_binary_transport(self):
        notifications = self.Notifications()
        asyncio.run(notifications._deliver_media_from_response(
            f"MEDIA:{self.path}", self.event, self.adapter, thread_metadata={},
        ))
        self.assertEqual(self.transports, [])

    def test_actual_background_worker_cannot_send_local_or_remote_binary_results(self):
        # Execute the complete fixed background delivery function. Only the
        # expensive model call is replaced at its executor boundary; production
        # extraction, routing and the platform constructor are the real host.
        turn = self.Turn()
        turn._adapter_for_source = lambda _source: self.adapter
        turn._thread_metadata_for_source = lambda *_args: {}
        turn._resolve_session_agent_runtime = lambda **_kwargs: ("fixture-model", {"api_key": "not-a-provider-key"})
        turn._resolve_turn_toolsets = lambda *_args: ([], [])
        turn._provider_routing = {}
        turn._resolve_session_reasoning_config = lambda **_kwargs: {}
        turn._resolve_session_service_tier = lambda **_kwargs: None
        turn._resolve_turn_agent_config = lambda _prompt, model, runtime: {"model": model, "runtime": runtime}
        executions = []

        async def completed_model(_callback):
            executions.append("model-boundary")
            return {"final_response": f"![private](https://fixture.invalid/private.png) MEDIA:{self.path}", "messages": []}

        turn._run_in_executor_with_context = completed_model
        with patch("gateway.run._load_gateway_config", return_value={}), patch("gateway.run._current_max_iterations", return_value=4):
            asyncio.run(turn._run_background_task_inner("fixture prompt", self.event.source, "fixture-background"))
        self.assertEqual(executions, ["model-boundary"])
        self.assertEqual(self.transports, [])
        self.assertNotIn("PRIVATE_BINARY_ARTIFACT_FIXTURE", repr(self.texts))

    def test_off_mode_keeps_the_original_platform_attachment_method(self):
        with patch.dict(os.environ, {"BROKERROUTER_HERMES_GOVERNANCE": "off"}):
            adapter = self.Adapter(self.PlatformConfig(), self.Platform.TELEGRAM)
            result = asyncio.run(adapter.send_image_file("fixture-chat", str(self.path)))
        self.assertTrue(result.success)
        self.assertEqual(len(self.transports), 1)


if __name__ == "__main__":
    unittest.main()
