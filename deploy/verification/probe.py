#!/usr/bin/env python3
"""Exercise the production Compose deployment with synthetic, zero-cost calls."""

import argparse
import datetime
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]


class Probe:
    def __init__(self, args):
        self.args = args
        self.compose = ["docker", "compose", "--project-name", args.project,
                        "--file", str(ROOT / "compose.production.yaml"),
                        "--file", str(ROOT / "compose.verification.yaml")]
        self.report = {"started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                       "project": args.project, "port": args.port, "samples": [],
                       "waves": args.waves, "tcp_limit": args.tcp_limit,
                       "rss_budget_bytes": args.rss_mib * 1024 * 1024}
        self.key = None
        self.report["max_settlement_wait_seconds"] = 0

    def run(self, *args):
        completed = subprocess.run([*self.compose, *args], cwd=ROOT, capture_output=True,
                                   text=True, timeout=180)
        if completed.returncode:
            # A command can return a one-time key; never echo its output on failure.
            raise RuntimeError(f"Compose command failed: {args[0]}")
        return completed.stdout.strip()

    def operator(self, *args):
        return self.run("--profile", "ops", "run", "--rm", "--no-deps", "operator", *args)

    def sql(self, query):
        return self.run("exec", "--no-TTY", "postgres", "psql", "--no-psqlrc", "-U",
                        "brokerrouter_migration", "-d", "brokerrouter", "-At", "-c", query)

    def provider_calls(self):
        value = self.run("exec", "--no-TTY", "verification-provider", "python", "-c",
                         "import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:8099/stats').read().decode())")
        return json.loads(value)["calls"]

    def request(self, method, path, body=None, idem=None, unread=False):
        connection = http.client.HTTPConnection("127.0.0.1", self.args.port, timeout=10)
        connection.connect()
        if unread:
            connection.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
        headers = {"Content-Type": "application/json"}
        if self.key:
            headers["Authorization"] = "Bearer " + self.key
        if idem:
            headers["Idempotency-Key"] = idem
        try:
            connection.request(method, path, json.dumps(body) if body is not None else None, headers)
            response = connection.getresponse()
        except Exception:
            connection.close()
            raise
        if unread and response.status == 200:
            return connection, response
        result = response.status, dict(response.getheaders()), response.read()
        connection.close()
        return result

    def chat(self, scenario, stream=True):
        return {"model": "local/verification", "max_tokens": 32, "stream": stream,
                "messages": [{"role": "user", "content": json.dumps(scenario)}]}

    def sample(self, label):
        command = """for p in /proc/[0-9]*; do
read -r name < "$p/comm" || continue
if [ "$name" = brokerrouter ]; then
while read -r field value rest; do
case "$field" in VmRSS:) echo "rss $value";; VmHWM:) echo "hwm $value";; esac
done < "$p/status"
fi
done
echo cgroup
cat /sys/fs/cgroup/memory.current
echo tcp
cat /proc/net/tcp
cat /proc/net/tcp6
"""
        lines = self.run("exec", "--no-TTY", "gateway", "sh", "-c", command).splitlines()
        sample = {"label": label, "rss_bytes": 0, "hwm_bytes": 0, "established": 0}
        for index, line in enumerate(lines):
            if line.startswith("rss "):
                sample["rss_bytes"] += int(line.split()[1]) * 1024
            elif line.startswith("hwm "):
                sample["hwm_bytes"] += int(line.split()[1]) * 1024
            elif line == "cgroup":
                sample["cgroup_bytes"] = int(lines[index + 1])
            else:
                fields = line.split()
                if len(fields) > 3 and fields[1].endswith(":1F90") and fields[3] == "01":
                    sample["established"] += 1
        self.report["samples"].append(sample)
        return sample

    def wait_ready(self):
        for _ in range(60):
            try:
                if self.request("GET", "/health/ready")[0] == 204:
                    return
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.5)
        raise RuntimeError("gateway did not become ready")

    def verify_http1(self):
        # No HEADERS/business request: negotiating H2 must never allocate streams
        # outside the data-plane's per-socket transport accounting.
        before = int(self.sql("SELECT count(*) FROM requests"))
        with socket.create_connection(("127.0.0.1", self.args.port), timeout=5) as peer:
            peer.sendall(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" + bytes.fromhex("000000040000000000"))
            try:
                response = peer.recv(64)
            except ConnectionResetError:
                response = b""
            assert not response or response.startswith(b"HTTP/1.1 4"), "data-plane accepted HTTP/2 outside socket accounting"
        assert int(self.sql("SELECT count(*) FROM requests")) == before
        self.report["data_plane_http2_rejected"] = True

    def wait_settled(self, ids):
        if not ids:
            return
        quoted = ",".join("'" + str(uuid.UUID(value)) + "'" for value in ids)
        started = time.monotonic()
        for _ in range(60):
            count = int(self.sql(f"SELECT count(*) FROM requests WHERE id IN ({quoted}) AND status='succeeded'"))
            if count == len(ids):
                self.report["max_settlement_wait_seconds"] = max(
                    self.report["max_settlement_wait_seconds"], time.monotonic() - started)
                return
            time.sleep(0.25)
        raise RuntimeError("a disconnected/slow stream did not settle")

    def verify(self):
        self.wait_ready()
        self.verify_http1()
        actual_limit = int(self.run("exec", "--no-TTY", "gateway", "printenv", "BROKERROUTER_MAX_HTTP_CONNECTIONS"))
        assert actual_limit == self.args.tcp_limit, "probe and deployment TCP limits differ"
        self.report["connection_lifetime_seconds"] = int(self.run(
            "exec", "--no-TTY", "gateway", "printenv", "BROKERROUTER_HTTP_CONNECTION_TIMEOUT_SECONDS"))
        uid = self.run("exec", "--no-TTY", "gateway", "sh", "-c",
                       'for p in /proc/[0-9]*; do read -r name < "$p/comm" || continue; '
                       'if [ "$name" = brokerrouter ]; then while read -r field value rest; do '
                       'case "$field" in Uid:) echo "$value";; esac; done < "$p/status"; fi; done')
        assert uid == "10001", "application process did not drop root privileges"
        self.report["database_tls_connections"] = int(self.sql(
            "SELECT count(*) FROM pg_stat_ssl s JOIN pg_stat_activity a USING(pid) "
            "WHERE a.usename='brokerrouter_runtime_login' AND s.ssl"))
        assert self.report["database_tls_connections"] >= 2
        assert self.request("GET", "/v1/models")[0] == 401
        initial_calls = self.provider_calls()
        self.tenant = str(uuid.UUID(self.operator("tenant-create", "--name", "local-verification", "--budget-cny", "10")))
        self.operator("model-put", "--tenant", self.tenant, "--id", "local/verification", "--name", "Local fixture")
        created = json.loads(self.operator("key-create", "--tenant", self.tenant, "--name", "Local verification", "--budget-cny", "10", "--model", "local/verification"))
        self.key = created["key"]
        self.operator("fx-set", "--tenant", self.tenant, "--usd-cny", "7", "--source", "local-fixture")
        self.operator("endpoint-add", "--tenant", self.tenant, "--model", "local/verification",
                      "--name", "Zero-cost provider", "--upstream-model", "local-upstream",
                      "--base-url", "http://verification-provider:8099/v1",
                      "--credential-env", "BROKERROUTER_LOCAL_PROVIDER_KEY", "--allow-private",
                      "--input-per-million", "1", "--output-per-million", "1",
                      "--context-tokens", "10000", "--max-output-tokens", "512", "--supports-streaming")
        # Credentials are loaded from active endpoint references at process start.
        self.run("restart", "gateway", "mcp-judge")
        self.wait_ready()
        self.sample("baseline")
        body = self.chat({"bytes": 64}, stream=False)
        before = self.provider_calls()
        for _ in range(2):
            status, headers, data = self.request("POST", "/v1/chat/completions", body, "nonstream-replay")
            assert status == 200, f"nonstream HTTP {status}: {json.loads(data).get('error', {}).get('code')}"
            assert json.loads(data)["choices"][0]["message"]["content"] == "x" * 64
        assert self.provider_calls() == before + 1
        self.run("restart", "gateway", "mcp-judge")
        self.wait_ready()
        assert self.request("POST", "/v1/chat/completions", body, "nonstream-replay")[0] == 200
        assert self.provider_calls() == before + 1
        before = self.provider_calls()
        stream_body = self.chat({"bytes": 3 * 1024 * 1024})
        for _ in range(2):
            status, headers, data = self.request("POST", "/v1/chat/completions", stream_body, "large-replay")
            assert status == 200 and data.endswith(b"data: [DONE]\n\n")
        assert self.provider_calls() == before + 1
        connection, response = self.request("POST", "/v1/chat/completions",
                                             self.chat({"bytes": 128, "delay_seconds": 1}), "disconnect", unread=True)
        disconnected = response.getheader("x-brokerrouter-request-id")
        connection.close()
        response.close()
        self.wait_settled([disconnected])
        print("Normal, large SSE, replay and restart checks passed", flush=True)
        held = []
        rejected = 0
        try:
            for wave in range(self.args.waves):
                ids = []
                for ordinal in range(4):
                    try:
                        result = self.request("POST", "/v1/chat/completions",
                                              self.chat({"bytes": 3 * 1024 * 1024, "delay_seconds": 1}),
                                              f"unread-{wave}-{ordinal}", unread=True)
                    except (OSError, http.client.HTTPException):
                        rejected += 1
                        continue
                    if len(result) == 2:
                        held.append(result)
                        ids.append(result[1].getheader("x-brokerrouter-request-id"))
                    else:
                        assert result[0] == 503 and json.loads(result[2])["error"]["code"] == "gateway_busy"
                        rejected += 1
                self.wait_settled(ids)
                time.sleep(0.3)
                sample = self.sample(f"unread-wave-{wave}")
                print(f"wave {wave + 1}: held={len(held)}, active TCP={sample['established']}, RSS={sample['rss_bytes'] // 1024 // 1024} MiB", flush=True)
            self.report["slow_connections_opened"] = len(held)
            self.report["rejected_connections"] = rejected
        finally:
            for connection, response in held:
                response.close()
                connection.close()
        time.sleep(1)
        self.sample("after-client-close")
        ledger = json.loads(self.sql(
            "SELECT json_build_object('requests',count(*),'succeeded',count(*) FILTER(WHERE status='succeeded'),"
            "'final_micros',coalesce(sum(final_micros),0),'invalid_finalizations',"
            f"(SELECT count(*) FROM (SELECT r.id FROM requests r LEFT JOIN ledger_entries l ON l.request_id=r.id AND l.kind='finalize' WHERE r.tenant_id='{self.tenant}' GROUP BY r.id HAVING count(l.id)<>1) x)) "
            f"FROM requests WHERE tenant_id='{self.tenant}'"))
        self.report["ledger"] = ledger
        assert ledger["requests"] == ledger["succeeded"] and ledger["invalid_finalizations"] == 0
        assert ledger["requests"] == self.provider_calls() - initial_calls
        # Fixture reports 3 input + 4 output tokens, each at 1 CNY/M tokens.
        assert ledger["final_micros"] == ledger["requests"] * 7, "settlement differs from fixed fixture pricing"
        assert int(self.sql(f"SELECT reserved_micros FROM tenants WHERE id='{self.tenant}'")) == 0
        assert int(self.sql(f"SELECT spent_micros FROM tenants WHERE id='{self.tenant}'")) == ledger["final_micros"]
        self.report["peak_rss_bytes"] = max(s["hwm_bytes"] for s in self.report["samples"])
        self.report["peak_cgroup_bytes"] = max(s["cgroup_bytes"] for s in self.report["samples"])
        self.report["peak_established_tcp"] = max(s["established"] for s in self.report["samples"])
        assert self.report["peak_rss_bytes"] <= self.report["rss_budget_bytes"], "RSS exceeded the fixed budget"
        assert self.report["peak_established_tcp"] <= self.args.tcp_limit, "transport connections are not bounded"

    def verify_deadline(self):
        self.wait_ready()
        self.verify_http1()
        lifetime = int(self.run("exec", "--no-TTY", "gateway", "printenv",
                                "BROKERROUTER_HTTP_CONNECTION_TIMEOUT_SECONDS"))
        assert 1 <= lifetime <= 900
        self.report["connection_lifetime_seconds"] = lifetime
        before = int(self.sql("SELECT count(*) FROM requests"))
        sockets = []
        started = time.monotonic()
        try:
            for partial in (False, True):
                peer = socket.create_connection(("127.0.0.1", self.args.port), timeout=10)
                sockets.append(peer)
                peer.settimeout(lifetime + 5)
                if partial:
                    peer.sendall(b"GET /health/ready HTTP/1.1\r\nHost: localhost\r\n")
            print(f"Checking idle and partial-header sockets at the deployed {lifetime}s deadline", flush=True)
            for peer in sockets:
                try:
                    assert peer.recv(1) == b"", "partial/idle socket returned unexpected bytes"
                except ConnectionResetError:
                    pass
            elapsed = time.monotonic() - started
            assert lifetime - 1 <= elapsed <= lifetime + 5, "socket lifetime was not enforced"
            self.report["deadline_elapsed_seconds"] = elapsed
        finally:
            for peer in sockets:
                peer.close()
        self.wait_ready()
        self.sample("after-deadline")
        assert int(self.sql("SELECT count(*) FROM requests")) == before, "idle sockets caused charging"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", default="brokerrouter-local-acceptance")
    parser.add_argument("--port", type=int, default=28080)
    parser.add_argument("--waves", type=int, default=20)
    parser.add_argument("--tcp-limit", type=int, default=64)
    parser.add_argument("--rss-mib", type=int, default=384)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--deadline-only", action="store_true")
    args = parser.parse_args()
    if not args.project.startswith("brokerrouter-local-") or not 1 <= args.waves <= 64:
        parser.error("a dedicated local project and 1–64 waves are required")
    os.umask(0o077)
    probe = Probe(args)
    try:
        if args.deadline_only:
            probe.verify_deadline()
        else:
            probe.verify()
        probe.report["result"] = "passed"
    except Exception as error:
        probe.report["result"] = "failed"
        probe.report["error"] = str(error)
        raise
    finally:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(probe.report, indent=2) + "\n")


if __name__ == "__main__":
    main()
