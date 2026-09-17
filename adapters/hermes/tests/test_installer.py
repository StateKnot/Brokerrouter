from __future__ import annotations

import importlib.util
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


INSTALLER = Path(__file__).resolve().parents[1] / "install.py"
SPEC = importlib.util.spec_from_file_location("brokerrouter_hermes_installer", INSTALLER)
assert SPEC and SPEC.loader
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


class InstallerTest(unittest.TestCase):
    def test_anchor_is_exact_and_refuses_drift(self):
        source = ("prefix\n" + installer.OLD + "suffix\n").encode()
        patched = installer.patched_bytes(source)
        self.assertNotEqual(source, patched)
        self.assertIn(b"Strong Brokerrouter boundary", patched)
        with self.assertRaises(RuntimeError):
            installer.patched_bytes(source.replace(b"_turns_since_memory", b"changed"))

    def test_gateway_anchors_are_exact_and_refuse_drift(self):
        run_turn = "\n".join(old for old, _ in installer.RUN_TURN_REPLACEMENTS).encode()
        patched_turn = installer.patched_run_turn_bytes(run_turn)
        self.assertIn(b"brokerrouter_turn_binding", patched_turn)
        self.assertIn(b"reject_governed_proxy", patched_turn)
        with self.assertRaises(RuntimeError):
            installer.patched_run_turn_bytes(run_turn.replace(b"message_type", b"changed", 1))

        runner = "\n".join(
            old for old, _ in installer.RUN_TURN_RUNNER_REPLACEMENTS
        ).encode()
        patched_runner = installer.patched_run_turn_runner_bytes(runner)
        self.assertIn(b"bind_gateway_agent", patched_runner)
        with self.assertRaises(RuntimeError):
            installer.patched_run_turn_runner_bytes(runner.replace(b"tools_holder", b"changed"))

    def test_apply_resumes_an_exact_partial_installation(self):
        tool = ("prefix\n" + installer.OLD + "suffix\n").encode()
        turn = "\n".join(old for old, _ in installer.RUN_TURN_REPLACEMENTS).encode()
        runner = "\n".join(
            old for old, _ in installer.RUN_TURN_RUNNER_REPLACEMENTS
        ).encode()
        specs = (
            ("agent/tool_executor.py", hashlib.sha256(tool).hexdigest(), installer.patched_bytes),
            ("gateway/run_turn.py", hashlib.sha256(turn).hexdigest(), installer.patched_run_turn_bytes),
            (
                "gateway/run_turn_runner.py",
                hashlib.sha256(runner).hexdigest(),
                installer.patched_run_turn_runner_bytes,
            ),
        )
        with tempfile.TemporaryDirectory() as directory, patch.object(
            installer, "PATCHES", specs
        ):
            root = Path(directory)
            for relative, source in (
                ("agent/tool_executor.py", tool),
                ("gateway/run_turn.py", turn),
                ("gateway/run_turn_runner.py", runner),
            ):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(source)

            first = specs[0]
            backup = installer._backup(root, first[0])
            backup.write_bytes(tool)
            (root / first[0]).write_bytes(first[2](tool))
            self.assertEqual(installer.inspect(root)[0], "partially_installed")

            manifest = installer.apply(root)
            self.assertEqual(manifest["schema"], 2)
            self.assertEqual(installer.inspect(root)[0], "installed")

            (root / "gateway/run_turn.py").write_bytes(b"drift")
            with self.assertRaises(RuntimeError):
                installer.inspect(root)

    def test_atomic_write_replaces_complete_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested" / "file"
            installer.atomic_write(path, b"first", 0o600)
            installer.atomic_write(path, b"second", 0o600)
            self.assertEqual(path.read_bytes(), b"second")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
