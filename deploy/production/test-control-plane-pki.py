#!/usr/bin/env python3
"""Real OpenSSL preflight and default Python 3.13 TLS, with task-only PKI.

Run in the fixed Python 3.13 verification container as root so the production
PostgreSQL Secret generator can apply its actual UID 999 ownership.
"""
import importlib.util
import os
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import threading
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("preflight", HERE / "verify-control-plane.py")
preflight = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preflight)

CA_PROFILE = ("basicConstraints=critical,CA:TRUE", "keyUsage=critical,keyCertSign,cRLSign",
              "subjectKeyIdentifier=hash", "authorityKeyIdentifier=keyid:always")
LEAF_PROFILE = ("basicConstraints=critical,CA:FALSE", "keyUsage=critical,digitalSignature,keyEncipherment",
                "extendedKeyUsage=serverAuth", "subjectKeyIdentifier=hash", "authorityKeyIdentifier=keyid,issuer")


def command(*arguments):
    result = subprocess.run(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if result.returncode:
        raise AssertionError("task-only PKI command failed")
    return result.stdout


class PkiTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="brokerrouter-pki-")
        self.root = Path(self.temporary.name).resolve()
        self.root.chmod(0o700)
        self.ca = self.root / "ca.crt"
        self.ca_key = self.root / "ca.key"

    def tearDown(self):
        self.temporary.cleanup()

    def root_ca(self, missing_usage=False, prefix="ca"):
        key, certificate = self.root / (prefix + ".key"), self.root / (prefix + ".crt")
        extensions = [value for value in CA_PROFILE if not (missing_usage and value.startswith("keyUsage="))]
        arguments = ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-nodes", "-days", "14",
                     "-subj", "/CN=task-only-" + prefix, "-keyout", str(key), "-out", str(certificate)]
        for extension in extensions:
            arguments.extend(("-addext", extension))
        command(*arguments)
        return certificate, key

    def signed(self, ca, ca_key, prefix="server", hostname="console.brokerrouter.internal", profile=LEAF_PROFILE, bits=2048):
        key, csr = self.root / (prefix + ".key"), self.root / (prefix + ".csr")
        certificate, extensions = self.root / (prefix + ".crt"), self.root / (prefix + ".ext")
        command("openssl", "req", "-new", "-newkey", "rsa:" + str(bits), "-sha256", "-nodes", "-subj", "/CN=" + prefix,
                "-keyout", str(key), "-out", str(csr))
        extensions.write_text("\n".join((*profile, "subjectAltName=DNS:" + hostname)) + "\n")
        command("openssl", "x509", "-req", "-sha256", "-days", "14", "-in", str(csr), "-CA", str(ca), "-CAkey", str(ca_key),
                "-CAcreateserial", "-extfile", str(extensions), "-out", str(certificate))
        return certificate, key

    def default_tls(self, ca, certificate, key, hostname="console.brokerrouter.internal", accepted=True):
        context = ssl.create_default_context(cafile=str(ca))
        self.assertTrue(context.verify_flags & ssl.VERIFY_X509_STRICT, "requires actual default Python 3.13 strict TLS")
        self.assertTrue(context.check_hostname)
        self.assertEqual(context.verify_mode, ssl.CERT_REQUIRED)
        server = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        server.load_cert_chain(str(certificate), str(key))
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(5)
        address = listener.getsockname()

        def serve():
            try:
                connection, _ = listener.accept()
                with connection:
                    with server.wrap_socket(connection, server_side=True):
                        pass
            except (OSError, ssl.SSLError):
                pass
            finally:
                listener.close()

        worker = threading.Thread(target=serve)
        worker.start()
        error = None
        try:
            with socket.create_connection(address, timeout=5) as connection:
                with context.wrap_socket(connection, server_hostname=hostname):
                    pass
        except ssl.SSLCertVerificationError as failure:
            error = failure.verify_code
        finally:
            worker.join(6)
        self.assertFalse(worker.is_alive())
        self.assertEqual(error is None, accepted)
        return error

    def test_standard_chain_matches_default_strict_tls(self):
        ca, key = self.root_ca()
        certificate, leaf_key = self.signed(ca, key)
        preflight.verify_tls_server(ca, certificate, "console.brokerrouter.internal")
        self.default_tls(ca, certificate, leaf_key)

    def test_old_ca_missing_usage_is_rejected(self):
        ca, key = self.root_ca(missing_usage=True)
        certificate, leaf_key = self.signed(ca, key)
        # Preserve the original false-pass boundary as a regression fixture.
        command("openssl", "verify", "-CAfile", str(ca), "-verify_hostname", "console.brokerrouter.internal", str(certificate))
        with self.assertRaises(ValueError):
            preflight.verify_tls_server(ca, certificate, "console.brokerrouter.internal")
        self.assertEqual(self.default_tls(ca, certificate, leaf_key, accepted=False), 92)

    def test_client_only_certificate_is_rejected(self):
        ca, key = self.root_ca()
        profile = tuple(value.replace("serverAuth", "clientAuth") for value in LEAF_PROFILE)
        certificate, leaf_key = self.signed(ca, key, profile=profile)
        with self.assertRaises(ValueError):
            preflight.verify_tls_server(ca, certificate, "console.brokerrouter.internal")
        self.default_tls(ca, certificate, leaf_key, accepted=False)

    def test_hostname_drift_is_rejected(self):
        ca, key = self.root_ca()
        certificate, leaf_key = self.signed(ca, key)
        with self.assertRaises(ValueError):
            preflight.verify_tls_server(ca, certificate, "wrong.brokerrouter.internal")
        self.default_tls(ca, certificate, leaf_key, hostname="wrong.brokerrouter.internal", accepted=False)

    def test_untrusted_ca_is_rejected(self):
        ca, key = self.root_ca()
        certificate, leaf_key = self.signed(ca, key)
        other, _ = self.root_ca(prefix="other")
        with self.assertRaises(ValueError):
            preflight.verify_tls_server(other, certificate, "console.brokerrouter.internal")
        self.default_tls(other, certificate, leaf_key, accepted=False)

    def test_intermediate_fullchain_preserves_explicit_root_trust(self):
        ca, key = self.root_ca()
        intermediate, intermediate_key = self.signed(ca, key, prefix="intermediate", profile=CA_PROFILE)
        certificate, leaf_key = self.signed(intermediate, intermediate_key)
        fullchain = self.root / "fullchain.pem"
        fullchain.write_bytes(certificate.read_bytes() + intermediate.read_bytes())
        preflight.verify_tls_server(ca, fullchain, "console.brokerrouter.internal")
        self.default_tls(ca, fullchain, leaf_key)
        other, _ = self.root_ca(prefix="other")
        with self.assertRaises(ValueError):
            preflight.verify_tls_server(other, fullchain, "console.brokerrouter.internal")

    def test_weak_leaf_key_is_rejected(self):
        ca, key = self.root_ca()
        certificate, _ = self.signed(ca, key, bits=1024)
        with self.assertRaises(ValueError):
            preflight.verify_tls_server(ca, certificate, "console.brokerrouter.internal")
        # The same default Python server context rejects this key before listening.
        with self.assertRaises(ssl.SSLError):
            ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER).load_cert_chain(str(certificate), str(self.root / "server.key"))

    def test_production_generator_actual_pg_chain_and_secret_ownership(self):
        self.assertEqual(os.geteuid(), 0, "production generator requires root for UID 999 Secret ownership")
        secrets = self.root / "generated"
        command("sh", str(HERE / "generate-secrets.sh"), str(secrets))
        ca, certificate, key = secrets / "pg-ca.crt", secrets / "pg-server.crt", secrets / "pg-server.key"
        preflight.verify_tls_server(ca, certificate, "postgres")
        self.default_tls(ca, certificate, key, hostname="postgres")
        self.assertEqual(key.stat().st_uid, 999)
        self.assertEqual(key.stat().st_mode & 0o777, 0o400)
        self.assertEqual(ca.stat().st_mode & 0o777, 0o444)
        self.assertFalse((secrets / "pg-ca.key").exists())
        self.assertFalse((secrets / "pg-server.csr").exists())
        self.assertFalse((secrets / "pg-server.ext").exists())
        original = key.read_bytes()
        result = subprocess.run(["sh", str(HERE / "generate-secrets.sh"), str(secrets)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(key.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
