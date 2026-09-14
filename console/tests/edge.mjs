// Isolated real Nginx TLS/static/proxy contract, not a real IdP certification.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  readFileSync,
  writeFileSync,
  rmSync,
  chmodSync,
} from "node:fs";
import { request } from "node:https";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
const image = process.env.BROKERROUTER_TEST_NGINX_IMAGE;
if (!/^nginx@sha256:[a-f0-9]{64}$/.test(image ?? ""))
  throw new Error(
    "Set BROKERROUTER_TEST_NGINX_IMAGE to the verified official image digest.",
  );
const directory = mkdtempSync(join(tmpdir(), "br-edge-contract-"));
const docker = (...args) =>
  execFileSync("docker", args, { encoding: "utf8", timeout: 30_000 }).trim();
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
      "/CN=console.example.invalid",
      "-keyout",
      join(directory, "privkey.pem"),
      "-out",
      join(directory, "fullchain.pem"),
    ],
    { stdio: "ignore" },
  );
  chmodSync(directory, 0o755);
  chmodSync(join(directory, "privkey.pem"), 0o644); // Ephemeral test certificate ONLY.
  const production = readFileSync("../deploy/console/nginx.conf", "utf8");
  // A local test-only upstream in the same network namespace, no credentials/data.
  const fixture = `server { listen 127.0.0.1:8087; location / { default_type application/json; return 200 '{"csrf":"$http_x_br_csrf","origin":"$http_origin","authorization":"$http_authorization"}'; } }`;
  writeFileSync(
    join(directory, "nginx.conf"),
    production.replace(/}\s*$/, `${fixture}\n}`),
  );
  const mounts = [
    "-v",
    `${directory}/nginx.conf:/etc/nginx/nginx.conf:ro`,
    "-v",
    `${directory}:/etc/brokerrouter/tls:ro`,
    "-v",
    `${resolve("dist")}:/srv/brokerrouter/console:ro`,
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
  const fetch = (path, headers = {}, method = "GET") =>
    new Promise((resolve, reject) => {
      const req = request(
        {
          hostname: "127.0.0.1",
          port,
          servername: "console.example.invalid",
          rejectUnauthorized: false,
          path,
          method,
          headers: { host: "console.example.invalid", ...headers },
          timeout: 5000,
        },
        (res) => {
          const chunks = [];
          res.on("data", (chunk) => chunks.push(chunk));
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
  let root;
  for (let i = 0; i < 20; i++) {
    try {
      root = await fetch("/");
      break;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  assert.equal(root?.status, 200);
  assert.equal(root.headers["cache-control"], "no-store");
  assert.equal(root.headers["referrer-policy"], "no-referrer");
  assert.match(root.headers["content-security-policy"], /script-src 'self'/);
  assert.doesNotMatch(
    root.headers["content-security-policy"],
    /unsafe-inline|unsafe-eval/,
  );
  const asset = root.body.match(/src="([^"]+\.js)"/)[1];
  assert.equal((await fetch(asset)).status, 200);
  for (const path of [
    "/.env",
    "/src/App.tsx",
    "/unknown",
    "/assets/missing.js",
  ]) {
    const result = await fetch(path);
    assert.equal(result.status, 404);
    assert.equal(result.headers["cache-control"], "no-store");
  }
  const response = await fetch("/bff/v1/session", {
    origin: "https://console.example.invalid",
    "x-br-csrf": "test-csrf",
    authorization: "test-rejected-by-bff",
  });
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(response.body), {
    csrf: "test-csrf",
    origin: "https://console.example.invalid",
    authorization: "test-rejected-by-bff",
  });
  const querySecret = "test-only-do-not-log";
  await fetch(`/bff/v1/callback?code=${querySecret}`);
  assert.ok(!docker("logs", container).includes(querySecret));
  assert.equal((await fetch("/", { host: "unexpected.example" })).status, 421);
  console.log(
    "Nginx TLS/static/CSP/no-store/proxy/no-query-log/host contracts passed.",
  );
} catch (error) {
  if (container) console.error(docker("logs", container));
  throw error;
} finally {
  if (container) docker("rm", "-f", container);
  rmSync(directory, { recursive: true, force: true });
}
