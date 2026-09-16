// Real Nginx boundary for the separate static viewer; synthetic TLS only.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { request } from "node:https";
const image = process.env.BROKERROUTER_TEST_NGINX_IMAGE;
if (!/^nginx@sha256:[a-f0-9]{64}$/.test(image ?? ""))
  throw new Error("verified Nginx digest required");
const directory = mkdtempSync(join(tmpdir(), "br-preview-edge-"));
const docker = (...a) =>
  execFileSync("docker", a, { encoding: "utf8", timeout: 30000 }).trim();
let container;
try {
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
      "/CN=review.example-media.invalid",
      "-keyout",
      join(directory, "privkey.pem"),
      "-out",
      join(directory, "fullchain.pem"),
    ],
    { stdio: "ignore" },
  );
  chmodSync(directory, 0o755);
  chmodSync(join(directory, "privkey.pem"), 0o644);
  const mounts = [
    "-v",
    `${resolve("../deploy/preview/nginx.conf")}:/etc/nginx/nginx.conf:ro`,
    "-v",
    `${directory}:/etc/brokerrouter/tls:ro`,
    "-v",
    `${resolve("../preview")}:/srv/brokerrouter/preview:ro`,
  ];
  docker("run", "--rm", ...mounts, image, "nginx", "-t");
  container = docker(
    "run",
    "-d",
    "--read-only",
    "--user",
    "101:101",
    "--cap-drop=ALL",
    "--security-opt=no-new-privileges",
    "--tmpfs",
    "/tmp:mode=1777",
    "--tmpfs",
    "/var/cache/nginx:mode=1777",
    "--tmpfs",
    "/var/run:mode=1777",
    "-p",
    "127.0.0.1::8443",
    ...mounts,
    "--entrypoint",
    "nginx",
    image,
    "-g",
    "daemon off;",
  );
  const port = Number(docker("port", container, "8443/tcp").split(":").at(-1));
  const get = (path, method = "GET", host = "review.example-media.invalid") =>
    new Promise((resolve, reject) => {
      const req = request(
        {
          hostname: "127.0.0.1",
          port,
          servername: "review.example-media.invalid",
          rejectUnauthorized: false,
          path,
          method,
          headers: { host },
          timeout: 5000,
        },
        (res) => {
          const chunks = [];
          res.on("data", (v) => chunks.push(v));
          res.on("end", () =>
            resolve({
              status: res.statusCode,
              headers: res.headers,
              body: Buffer.concat(chunks).toString(),
            }),
          );
        },
      );
      req.on("error", reject);
      req.on("timeout", () => req.destroy(new Error("timeout")));
      req.end();
    });
  let page;
  for (let i = 0; i < 20; i++) {
    try {
      page = await get("/review.html");
      break;
    } catch {
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  assert.equal(page.status, 200);
  assert.equal(page.headers["cache-control"], "no-store");
  assert.equal(page.headers["x-content-type-options"], "nosniff");
  assert.equal(page.headers["referrer-policy"], "no-referrer");
  assert.equal(page.headers["set-cookie"], undefined);
  const csp = page.headers["content-security-policy"];
  assert.match(csp, /sandbox allow-scripts/);
  assert.match(csp, /connect-src 'none'/);
  assert.match(csp, /media-src blob:/);
  assert.match(csp, /frame-ancestors https:\/\/console.example.invalid/);
  assert.doesNotMatch(
    csp,
    /unsafe-inline|unsafe-eval|allow-same-origin|allow-downloads|allow-top-navigation/,
  );
  assert.match(
    page.body,
    /br-parent-origin" content="https:\/\/console.example.invalid"/,
  );
  for (const name of ["review.js", "review.css"]) {
    const r = await get(`/${name}`);
    assert.equal(r.status, 200);
    assert.equal(r.body, readFileSync(resolve("../preview", name), "utf8"));
  }
  for (const path of [
    "/",
    "/.env",
    "/bff/v1/session",
    "/video.mp4",
    "/unknown",
  ]) {
    assert.equal((await get(path)).status, 404);
  }
  assert.equal((await get("/review.html?secret=test")).status, 400);
  assert.equal((await get("/review.html", "POST")).status, 405);
  assert.equal(
    (await get("/review.html", "GET", "console.example.invalid")).status,
    421,
  );
  console.log(
    "isolated preview Nginx TLS/static/CSP/no-store/no-proxy boundary passed",
  );
} finally {
  if (container) docker("rm", "-f", container);
  rmSync(directory, { recursive: true, force: true });
}
