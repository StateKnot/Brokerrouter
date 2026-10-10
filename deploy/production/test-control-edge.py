#!/usr/bin/env python3
"""Real TLS/Nginx tests of production static images and fixed private upstreams."""
import http.client
import json
import os
from pathlib import Path
import ssl
import subprocess
import tempfile
import time
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[2]
NGINX = "nginx@sha256:dc5069ad14f19660b141b21236140b91656bf89bbc3e2417c70ae650cd66104c"


def run(*args, **kwargs):
    return subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=30, **kwargs)


def docker(*args):
    result = run("docker", *args)
    if result.returncode:
        raise RuntimeError("task-only Docker fixture failed: " + result.stderr.replace("SYNTHETIC_NEVER_LOG_TOKEN", "[fixture token redacted]"))
    return result.stdout.strip()


class EdgeTest(unittest.TestCase):
    def test_production_images_and_tls_boundaries(self):
        with tempfile.TemporaryDirectory(prefix="brokerrouter-edge-test-") as directory:
            root = Path(directory).resolve()
            os.chmod(root, 0o700)
            def openssl(*arguments):
                self.assertEqual(run("openssl", *arguments).returncode, 0, "synthetic PKI creation failed")
            openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=task-only-ca",
                    "-keyout", str(root / "ca.key"), "-out", str(root / "ca.crt"))
            openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=task-only-wrong-ca",
                    "-keyout", str(root / "wrong-ca.key"), "-out", str(root / "wrong-ca.crt"))
            hosts = [purpose + ".brokerrouter.internal" for purpose in (
                "control", "assets", "media-api", "output-scan", "output-delivery", "console", "catalog", "settings", "approvals", "media-review")]
            for label, names in (("console", ["console.fixture.invalid"]), ("preview", ["review.fixture-media.invalid"]), ("control", hosts)):
                extensions = root / (label + ".ext")
                extensions.write_text("subjectAltName=" + ",".join("DNS:" + host for host in names) + "\n")
                openssl("req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=" + names[0],
                        "-keyout", str(root / (label + ".key")), "-out", str(root / (label + ".csr")))
                openssl("x509", "-req", "-days", "2", "-in", str(root / (label + ".csr")), "-CA", str(root / "ca.crt"),
                        "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-extfile", str(extensions), "-out", str(root / (label + ".crt")))
                os.chmod(root / (label + ".key"), 0o600)
            self.assertEqual(run("python3", str(ROOT / "deploy/production/render-control-edge.py"),
                                 "https://console.fixture.invalid", "https://review.fixture-media.invalid", str(root)).returncode, 0)
            self.assertEqual(run("python3", str(ROOT / "deploy/production/render-control-edge.py"),
                                 "https://console.fixture.invalid", "https://review.fixture-media.invalid", str(root)).returncode, 0)
            print("PASS: atomically rendered console/preview release and identical rerun", flush=True)
            for origins in (("http://console.fixture.invalid", "https://review.fixture-media.invalid"),
                            ("https://console.fixture.invalid", "https://review.fixture.invalid"),
                            ("https://console.fixture.invalid/", "https://review.fixture-media.invalid"),
                            ("https://console.fixture.invalid:443", "https://review.fixture-media.invalid")):
                self.assertNotEqual(run("python3", str(ROOT / "deploy/production/render-control-edge.py"), *origins, str(root)).returncode, 0)
            print("PASS: HTTP/noncanonical/same-apex origins are rejected", flush=True)
            token = "SYNTHETIC_NEVER_LOG_TOKEN"
            upstream = "user nginx; worker_processes 1; pid /tmp/upstream.pid; error_log /dev/null; events {worker_connections 128;} http {access_log off; client_body_temp_path /tmp/body; proxy_temp_path /tmp/proxy; fastcgi_temp_path /tmp/fastcgi; uwsgi_temp_path /tmp/uwsgi; scgi_temp_path /tmp/scgi;"
            for port in range(8081, 8092):
                upstream += "server {listen " + str(port) + "; location / {default_type application/json; return 200 '{\"authorization\":\"$http_authorization\",\"cookie\":\"$http_cookie\",\"csrf\":\"$http_x_br_csrf\",\"origin\":\"$http_origin\"}';}}"
            upstream += "}\n"
            (root / "upstream.conf").write_text(upstream)
            namespace = "brokerrouter-edge-test-" + uuid.uuid4().hex[:12]
            network = namespace + "-network"
            preview_network = namespace + "-preview"
            containers = []
            context = ssl.create_default_context(cafile=str(root / "ca.crt"))
            def start(label, image, conf, aliases=()):
                name = namespace + "-" + label
                args = ["run", "--detach", "--name", name, "--network", preview_network if label == "preview" else network, "--read-only", "--cap-drop", "ALL",
                        "--cap-add", "CHOWN", "--cap-add", "SETUID", "--cap-add", "SETGID", "--security-opt", "no-new-privileges", "--memory", "128m",
                        "--cpus", "0.5", "--pids-limit", "64", "--tmpfs", "/tmp:size=16m,mode=1777",
                        "--mount", "type=bind,src=" + str(conf) + ",dst=/etc/nginx/nginx.conf,readonly"]
                for alias in aliases:
                    args += ["--network-alias", alias]
                if label != "upstream":
                    args += ["--publish", "127.0.0.1::8443",
                             "--mount", "type=bind,src=" + str(root / (label + ".crt")) + ",dst=/run/secrets/tls_fullchain,readonly",
                             "--mount", "type=bind,src=" + str(root / (label + ".key")) + ",dst=/run/secrets/tls_private_key,readonly"]
                args += ["--entrypoint", "nginx", image, "-g", "daemon off;"]
                docker(*args)
                containers.append(name)
                return name
            def request(name, hostname, path="/", method="GET", headers=None, trust=context):
                port = int(docker("port", name, "8443/tcp").rsplit(":", 1)[1])
                connection = http.client.HTTPSConnection("127.0.0.1", port, context=trust, timeout=5)
                import socket
                connection.sock = trust.wrap_socket(socket.create_connection(("127.0.0.1", port), 5), server_hostname=hostname)
                try:
                    selected = {"Host": hostname, **(headers or {})}
                    connection.request(method, path, headers=selected)
                    response = connection.getresponse()
                    return response.status, dict(response.getheaders()), response.read()
                finally:
                    connection.close()
            try:
                # Docker's internal-only bridge does not publish ingress ports.
                # The production overlay pairs separate ingress bridges with its internal backend.
                docker("network", "create", network)
                docker("network", "create", preview_network)
                upstream_name = start("upstream", NGINX, root / "upstream.conf", ["bff", "control", "assets", "media-api", "output-scan", "output-delivery", "console", "catalog", "settings", "approvals", "media-review"])
                console_image = os.environ.get("BROKERROUTER_CONSOLE_TEST_IMAGE", "brokerrouter-console:r1-local-verification")
                preview_image = os.environ.get("BROKERROUTER_PREVIEW_TEST_IMAGE", "brokerrouter-preview:r1-local-verification")
                main = start("console", console_image, root / "control-edge/console-nginx.conf")
                preview = start("preview", preview_image, root / "control-edge/preview-nginx.conf")
                control = start("control", NGINX, ROOT / "deploy/production/control-plane-nginx.conf", hosts)
                # One bounded startup wait, never retries failed assertions.
                for _ in range(30):
                    if all(docker("inspect", "--format", "{{.State.Running}}", name) == "true" for name in containers):
                        break
                    time.sleep(0.1)
                def fetch(*args, **kwargs):
                    # No preliminary TLS handshake with numeric-IP SNI.
                    return request(*args, **kwargs)
                status, headers, body = fetch(main, "console.fixture.invalid")
                self.assertEqual(status, 200)
                self.assertTrue(b"/assets/" in body)
                self.assertIn("frame-src https://review.fixture-media.invalid", headers["Content-Security-Policy"])
                self.assertEqual(headers["Cache-Control"], "no-store")
                print("PASS: built console artifacts served with exact CSP/preview origin", flush=True)
                status, _, body = fetch(main, "console.fixture.invalid", "/bff/v1/callback?code=" + token,
                                        headers={"Authorization": token, "Cookie": token, "X-BR-CSRF": token, "Origin": "https://console.fixture.invalid"})
                self.assertEqual(status, 200)
                received = json.loads(body)
                self.assertTrue(all(received[key] == token for key in ("authorization", "cookie", "csrf")))
                self.assertEqual(received["origin"], "https://console.fixture.invalid")
                print("PASS: BFF proxy preserves authorization/cookie/CSRF/origin without synthesis", flush=True)
                self.assertEqual(fetch(main, "console.fixture.invalid", headers={"Host": "wrong.fixture.invalid"})[0], 421)
                for name, hostname in ((main, "console.fixture.invalid"), (preview, "review.fixture-media.invalid"), (control, hosts[0])):
                    for bad_trust in (ssl.create_default_context(), ssl.create_default_context(cafile=str(root / "wrong-ca.crt"))):
                        with self.assertRaises(ssl.SSLError):
                            fetch(name, hostname, trust=bad_trust)
                    with self.assertRaises(ssl.SSLError):
                        fetch(name, "wrong.fixture.invalid")
                print("PASS: all three edges reject absent/wrong CA and unknown SNI/hostname; wrong Host is 421", flush=True)
                for path in ("/", "/bff/v1/session", "/secret.env", "/review.html.map"):
                    self.assertEqual(fetch(preview, "review.fixture-media.invalid", path)[0], 404)
                status, headers, body = fetch(preview, "review.fixture-media.invalid", "/review.html")
                self.assertEqual(status, 200)
                self.assertTrue(b'content="https://console.fixture.invalid"' in body)
                self.assertIn("sandbox allow-scripts", headers["Content-Security-Policy"])
                self.assertIn("connect-src 'none'", headers["Content-Security-Policy"])
                self.assertIn("frame-ancestors https://console.fixture.invalid", headers["Content-Security-Policy"])
                self.assertNotIn("Set-Cookie", headers)
                self.assertEqual(fetch(preview, "review.fixture-media.invalid", "/review.js?token=" + token)[0], 400)
                self.assertEqual(fetch(preview, "review.fixture-media.invalid", "/review.html", method="POST")[0], 405)
                self.assertEqual(fetch(preview, "review.fixture-media.invalid", headers={"Host": "wrong.fixture.invalid"})[0], 421)
                print("PASS: immutable preview parent origin, sandbox, no BFF/Secret/query/POST/cookie authority", flush=True)
                for host in hosts:
                    self.assertEqual(fetch(control, host, "/health/ready")[0], 200)
                print("PASS: ten fixed private TLS names route to their respective HTTP upstreams", flush=True)
                for name in containers:
                    inspection = json.loads(docker("inspect", name))[0]
                    self.assertTrue(inspection["HostConfig"]["ReadonlyRootfs"])
                    process_ids = docker("exec", name, "sh", "-c", "for p in /proc/[0-9]*/status; do awk '/^Name:.*nginx/{n=1} /^Uid:/{if(n) print $2}' \"$p\"; done").splitlines()
                    self.assertIn("0", process_ids)
                    self.assertIn("101", process_ids)
                    self.assertNotIn(token, docker("logs", name))
                print("PASS: root TLS master/nonprivileged UID101 workers, read-only roots, and no token in logs", flush=True)
                for image in (console_image, preview_image):
                    self.assertEqual(run("docker", "run", "--rm", "--network", "none", "--read-only", "--entrypoint", "sh", image,
                                         "-c", "[ ! -e /src ] && [ ! -e /run/secrets/tls_private_key ]").returncode, 0)
                label = docker("image", "inspect", preview_image, "--format", '{{index .Config.Labels "io.brokerrouter.preview.parent-origin"}}')
                self.assertEqual(label, "https://console.fixture.invalid")
                failure = run("docker", "run", "--rm", "--network", network, "--read-only", "--tmpfs", "/tmp:size=16m,mode=1777",
                              "--mount", "type=bind,src=" + str(root / "control-edge/console-nginx.conf") + ",dst=/etc/nginx/nginx.conf,readonly", console_image, "-t")
                self.assertNotEqual(failure.returncode, 0)
                print("PASS: missing external TLS material prevents startup; images contain no source or TLS Secret", flush=True)
            finally:
                for name in reversed(containers):
                    state = run("docker", "inspect", "--format", "{{.State.Running}}", name)
                    if state.stdout.strip() != "true":
                        logs = run("docker", "logs", name)
                        print("Task-only failed startup: " + name + " " + (logs.stdout + logs.stderr).replace(token, "[fixture token redacted]"), flush=True)
                    run("docker", "rm", "--force", name)
                run("docker", "network", "rm", network)
                run("docker", "network", "rm", preview_network)


if __name__ == "__main__":
    unittest.main()
