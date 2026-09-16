// Test-only HTTPS static server. There are no simulated BFF routes in the build.
import { createServer } from "node:https";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
const directory = mkdtempSync(join(tmpdir(), "br-ui-tls-"));
execFileSync(
  "openssl",
  [
    "req",
    "-x509",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-days",
    "1",
    "-subj",
    "/CN=localhost",
    "-keyout",
    join(directory, "key.pem"),
    "-out",
    join(directory, "cert.pem"),
  ],
  { stdio: "ignore" },
);
const root = resolve("dist");
const types = {
  html: "text/html",
  js: "text/javascript",
  css: "text/css",
  woff2: "font/woff2",
};
const server = createServer(
  {
    key: readFileSync(join(directory, "key.pem")),
    cert: readFileSync(join(directory, "cert.pem")),
  },
  (req, res) => {
    const path = new URL(req.url, "https://localhost").pathname;
    const file =
      path === "/"
        ? "index.html"
        : /^\/assets\/[a-zA-Z0-9_.-]+$/.test(path)
          ? path.slice(1)
          : null;
    res.setHeader("cache-control", "no-store");
    res.setHeader(
      "content-security-policy",
      "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self'; frame-src https://127.0.0.1:4174; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
    );
    res.setHeader("x-content-type-options", "nosniff");
    res.setHeader("referrer-policy", "no-referrer");
    try {
      if (!file) throw new Error("not found");
      const content = readFileSync(join(root, file));
      res.setHeader(
        "content-type",
        types[file.split(".").at(-1)] ?? "application/octet-stream",
      );
      res.end(content);
    } catch {
      res.writeHead(404);
      res.end();
    }
  },
);
server.listen(4173, "127.0.0.1");
// Separate host/origin, opaque iframe sandbox, and production-equivalent CSP.
const preview = createServer(
  {
    key: readFileSync(join(directory, "key.pem")),
    cert: readFileSync(join(directory, "cert.pem")),
  },
  (req, res) => {
    res.setHeader("cache-control", "no-store");
    res.setHeader("x-content-type-options", "nosniff");
    res.setHeader("referrer-policy", "no-referrer");
    res.setHeader(
      "content-security-policy",
      "default-src 'none'; script-src 'self'; style-src 'self'; media-src blob:; connect-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors https://localhost:4173; sandbox allow-scripts",
    );
    if (
      !["/review.html", "/review.css", "/review.js"].includes(req.url) ||
      req.method !== "GET"
    ) {
      res.writeHead(404);
      res.end();
      return;
    }
    const file = req.url.slice(1);
    let data = readFileSync(resolve("../preview", file));
    if (file === "review.html")
      data = Buffer.from(
        data
          .toString()
          .replace("https://console.example.invalid", "https://localhost:4173"),
      );
    res.setHeader("content-type", types[file.split(".").at(-1)]);
    res.end(data);
  },
);
preview.listen(4174, "127.0.0.1");
const cleanup = () => {
  server.close();
  preview.close();
  rmSync(directory, { recursive: true, force: true });
};
process.on("SIGTERM", () => {
  cleanup();
  process.exit(0);
});
process.on("SIGINT", () => {
  cleanup();
  process.exit(0);
});
process.on("exit", cleanup);
