from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest


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

    def test_atomic_write_replaces_complete_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested" / "file"
            installer.atomic_write(path, b"first", 0o600)
            installer.atomic_write(path, b"second", 0o600)
            self.assertEqual(path.read_bytes(), b"second")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
