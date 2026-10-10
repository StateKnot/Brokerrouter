"""Local TLS fixture using only Python's standard library; no real credentials."""
import http.server
import json
import ssl
import sys
import threading

hits = {"proxy": 0, "leak": 0}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/redirect":
            self.send_response(307)
            self.send_header("Location", f"https://localhost:{self.server.server_port}/leak")
            self.end_headers()
            return
        if self.path == "/leak":
            hits["leak"] += 1
        body = json.dumps(hits if self.path == "/stats" else {"ok": True}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


class Proxy(Handler):
    def do_CONNECT(self):
        hits["proxy"] += 1
        self.send_error(502)

    def do_GET(self):
        hits["proxy"] += 1
        self.send_error(502)


tls = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[1], sys.argv[2])
tls.socket = context.wrap_socket(tls.socket, server_side=True)
proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
threading.Thread(target=proxy.serve_forever, daemon=True).start()
print(json.dumps([tls.server_port, proxy.server_port]), flush=True)
tls.serve_forever()
