from __future__ import annotations

from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import gzip
import json
from pathlib import Path
import ssl
import tempfile
import threading
import unittest
from unittest.mock import patch

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

from brokerrouter_hermes.client import HttpTransport, TransportFailure, GatewayFailure
from brokerrouter_hermes.config import Config, ConfigurationError


class Handler(BaseHTTPRequestHandler):
    hits = 0

    def do_GET(self):
        type(self).hits += 1
        self.send_response(302 if self.path == "/redirect" else 200)
        if self.path == "/redirect":
            self.send_header("location", f"https://127.0.0.1:{self.server.server_port}/target")
        if self.path == "/compressed":
            self.send_header("content-encoding", "gzip")
        self.send_header("content-type", "text/plain" if self.path == "/wrong-content" else "application/json")
        self.end_headers()
        raw = {
            "/large": b'{"body":"' + b"x" * (1024 * 1024) + b'"}',
            "/invalid-json": b"{",
            "/not-object": b"[]",
            "/bad-utf8": b"\xff",
            "/compressed": gzip.compress(b'{"state":"compressed fixture"}'),
        }.get(self.path, json.dumps({"state": "fixture", "path": self.path}).encode())
        self.wfile.write(raw)

    def log_message(self, *_):
        pass


class TransportTlsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.root = Path(cls.directory.name)
        now = datetime.now(timezone.utc)

        def ca(name):
            key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
            subject = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, name)])
            certificate = (
                x509.CertificateBuilder().subject_name(subject).issuer_name(subject)
                .public_key(key.public_key()).serial_number(x509.random_serial_number())
                .not_valid_before(now - timedelta(minutes=1)).not_valid_after(now + timedelta(days=1))
                .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
                .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
                .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(key.public_key()), critical=False)
                .add_extension(x509.KeyUsage(digital_signature=False, content_commitment=False, key_encipherment=False,
                    data_encipherment=False, key_agreement=False, key_cert_sign=True, crl_sign=True,
                    encipher_only=False, decipher_only=False), critical=True)
                .sign(key, hashes.SHA256())
            )
            return key, certificate

        key, certificate = ca("Hermes gateway fixture CA")
        _, other = ca("Different purpose fixture CA")
        cls.ca = cls.root / "ca.pem"
        cls.ca.write_bytes(certificate.public_bytes(serialization.Encoding.PEM))
        cls.other = cls.root / "other.pem"
        cls.other.write_bytes(other.public_bytes(serialization.Encoding.PEM))
        server_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        server_certificate = (
            x509.CertificateBuilder()
            .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "fixture")]))
            .issuer_name(certificate.subject).public_key(server_key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - timedelta(minutes=1)).not_valid_after(now + timedelta(days=1))
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(server_key.public_key()), critical=False)
            .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(key.public_key()), critical=False)
            .add_extension(x509.KeyUsage(digital_signature=True, content_commitment=False, key_encipherment=True,
                data_encipherment=False, key_agreement=False, key_cert_sign=False, crl_sign=False,
                encipher_only=False, decipher_only=False), critical=True)
            .add_extension(x509.ExtendedKeyUsage([x509.oid.ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
            .add_extension(x509.SubjectAlternativeName([x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]), critical=False)
            .sign(key, hashes.SHA256())
        )
        cert_file, key_file = cls.root / "server.pem", cls.root / "server.key"
        cert_file.write_bytes(server_certificate.public_bytes(serialization.Encoding.PEM))
        key_file.write_bytes(server_key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
        cls.key_bytes = key_file.read_bytes()
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert_file, key_file)
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.server.socket = context.wrap_socket(cls.server.socket, server_side=True)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.url = f"https://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=5)
        cls.directory.cleanup()

    def transport(self, ca=None, url=None):
        transport = HttpTransport(url or self.url, "sk_br_" + "11" * 32, 2, ca)
        self.addCleanup(transport._client.close)
        return transport

    def test_private_ca_handshake_uses_exact_purpose_bundle(self):
        self.assertEqual(self.transport(self.ca).request("GET", "/fixture")["path"], "/fixture")
        for ca in (None, self.other):
            with self.subTest(ca=ca.name if ca else "missing"):
                with self.assertRaises(TransportFailure):
                    self.transport(ca).request("GET", "/fixture")

    def test_additional_roots_preserve_hostname_verification(self):
        with self.assertRaises(TransportFailure):
            self.transport(self.ca, self.url.replace("127.0.0.1", "localhost")).request("GET", "/fixture")

    def test_multi_ca_bundle_is_loaded_once_at_client_construction(self):
        bundle = self.root / "multi.pem"
        bundle.write_bytes(self.other.read_bytes() + self.ca.read_bytes())
        transport = self.transport(bundle)
        bundle.unlink()
        self.assertEqual(transport.request("GET", "/fixture")["state"], "fixture")

    def test_ambient_ca_proxy_and_other_purpose_settings_do_not_change_trust(self):
        with patch.dict("os.environ", {
            "SSL_CERT_FILE": str(self.ca), "SSL_CERT_DIR": str(self.root),
            "HTTPS_PROXY": "http://127.0.0.1:1", "NO_PROXY": "",
            "BROKERROUTER_BFF_CA_FILE": str(self.ca), "BROKERROUTER_SCANNER_CA_FILE": str(self.ca),
        }):
            with self.assertRaises(TransportFailure):
                self.transport().request("GET", "/fixture")
            self.assertEqual(self.transport(self.ca).request("GET", "/fixture")["state"], "fixture")

    def test_redirect_does_not_forward_virtual_key_or_follow_target(self):
        before = Handler.hits
        with self.assertRaises(GatewayFailure) as error:
            self.transport(self.ca).request("GET", "/redirect")
        self.assertEqual(error.exception.status, 302)
        self.assertEqual(Handler.hits, before + 1)

    def test_real_https_response_contract_is_bounded_without_retry(self):
        transport = self.transport(self.ca)
        for path in ("/large", "/invalid-json", "/not-object", "/bad-utf8", "/wrong-content", "/compressed"):
            with self.subTest(path=path):
                before = Handler.hits
                with self.assertRaises(TransportFailure):
                    transport.request("GET", path)
                self.assertEqual(Handler.hits, before + 1)
                self.assertEqual(transport.request("GET", "/fixture")["state"], "fixture")

    def test_invalid_bundle_fails_startup_without_network(self):
        cases = [b"", b" \n", b"x" * (1024 * 1024 + 1), b"\xff", b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
                 self.ca.read_bytes() + b"trailing", self.ca.read_bytes() + self.key_bytes, self.ca.read_bytes() * 129]
        before = Handler.hits
        for ordinal, content in enumerate(cases):
            with self.subTest(ordinal=ordinal):
                path = self.root / f"invalid-{ordinal}.pem"
                path.write_bytes(content)
                with self.assertRaises(ConfigurationError):
                    self.transport(path)
        for path in (self.root, self.root / "absent", Path("relative.pem")):
            with self.subTest(kind=str(path)):
                with self.assertRaises(ConfigurationError):
                    self.transport(path)
        self.assertEqual(Handler.hits, before)

    def test_config_requires_explicit_absolute_gateway_ca_reference(self):
        env = {
            "BROKERROUTER_HERMES_GOVERNANCE": "required", "BROKERROUTER_BASE_URL": "https://gateway.invalid",
            "BROKERROUTER_HERMES_APPROVALS_URL": "https://console.invalid", "BROKERROUTER_VIRTUAL_KEY": "sk_br_" + "11" * 32,
            "BROKERROUTER_HERMES_STATE_DB": str(self.root / "state.db"), "BROKERROUTER_HERMES_STATE_KEY": "11" * 32,
        }
        self.assertIsNone(Config.from_env(env).gateway_ca_file)
        env["BROKERROUTER_HERMES_GATEWAY_CA_FILE"] = str(self.ca)
        self.assertEqual(Config.from_env(env).gateway_ca_file, self.ca)
        for invalid in ("", "relative.pem"):
            env["BROKERROUTER_HERMES_GATEWAY_CA_FILE"] = invalid
            with self.assertRaises(ConfigurationError):
                Config.from_env(env)
