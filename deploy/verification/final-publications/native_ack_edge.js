// Test-only fixed runtime HTTPS edge: lose exactly one committed record ACK.
// It never logs request paths, headers, bodies, credentials or model text.
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const tls = require('node:tls');
const key = fs.readFileSync('/run/secrets/tls_private_key');
const cert = fs.readFileSync('/run/secrets/tls_fullchain');
const context = tls.createSecureContext({ key, cert, minVersion: 'TLSv1.2' });
process.setgid(10001);
process.setuid(10001);
let lost = false;
const server = https.createServer({ key, cert, minVersion: 'TLSv1.2',
  SNICallback(name, callback) {
    callback(name === 'runtime.brokerrouter.internal' ? null : Error('SNI refused'), context);
  },
}, (request, response) => {
  if (request.headers.host !== 'runtime.brokerrouter.internal:8443') {
    response.writeHead(421); response.end(); return;
  }
  const record = request.method === 'POST' && request.url === '/v1/governance/final-publications';
  // No destination, URI rewrite, redirect, or retry comes from caller input.
  const upstream = http.request({ hostname: 'gateway', port: 8080,
    method: request.method, path: request.url,
    headers: { ...request.headers, connection: 'close' }, timeout: 30000,
  }, reply => {
    if (record && !lost && reply.statusCode === 200) {
      let bytes = 0, chunks = [];
      reply.on('data', chunk => {
        bytes += chunk.length;
        if (bytes > 65536) { reply.destroy(); request.destroy(); return; }
        chunks.push(chunk);
      });
      reply.on('end', () => {
        let metadata;
        try { metadata = JSON.parse(Buffer.concat(chunks)); }
        catch { response.destroy(); return; }
        if (metadata.state !== 'recorded' || typeof metadata.id !== 'string' || 'text' in metadata) {
          request.destroy(); return;
        }
        lost = true;
        fs.writeFileSync('/tmp/committed-record-ack-loss.json', JSON.stringify({
          dropped: 1, status: reply.statusCode, publication_id: metadata.id,
          text_sha256: metadata.text_sha256, text_bytes: metadata.bytes,
          ack_body_forwarded: false,
        }), { mode: 0o600 });
        // The core response is fully received before the real client socket dies.
        response.destroy();
      });
    } else {
      response.writeHead(reply.statusCode, reply.headers);
      reply.pipe(response);
    }
    reply.on('error', () => response.destroy());
  });
  upstream.on('timeout', () => upstream.destroy());
  upstream.on('error', () => response.destroy());
  let bytes = 0, requestChunks = [];
  const chat = request.method === 'POST' && request.url === '/v1/chat/completions';
  request.on('data', chunk => {
    bytes += chunk.length;
    if (bytes > 16 * 1024 * 1024) { upstream.destroy(); return; }
    if (chat) requestChunks.push(chunk);
  });
  request.on('end', () => {
    if (!chat || bytes > 16 * 1024 * 1024) return;
    let body;
    try { body = JSON.parse(Buffer.concat(requestChunks)); }
    catch { requestChunks = []; return; }
    requestChunks = [];
    if (!body || typeof body !== 'object' || Array.isArray(body)) return;
    const keys = Object.keys(body).sort();
    if (keys.length > 64 || !keys.every(name => /^[A-Za-z_][A-Za-z0-9_]{0,80}$/.test(name))) return;
    const options = body.stream_options;
    fs.writeFileSync('/tmp/native-wire-field-profile.json', JSON.stringify({
      top_level_field_names: keys,
      top_level_field_types: Object.fromEntries(keys.map(name => [name, Array.isArray(body[name]) ? 'array' : typeof body[name]])),
      stream: body.stream === true,
      stream_options_types: options && typeof options === 'object' ? Object.fromEntries(Object.keys(options).map(name => [name, typeof options[name]])) : {},
      stream_options_include_usage: options && typeof options.include_usage === 'boolean' ? options.include_usage : null,
    }), { mode: 0o600 });
  });
  request.on('error', () => upstream.destroy());
  request.pipe(upstream);
});
server.requestTimeout = 45000;
server.headersTimeout = 10000;
server.keepAliveTimeout = 15000;
server.maxRequestsPerSocket = 100;
server.listen(8443, '0.0.0.0');
