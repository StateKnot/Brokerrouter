// Zero-cost model fixture. No request body, authorization value or final text is logged.
const http = require('node:http');
let calls = 0;
const response = {
  id: 'provider-id-is-not-the-gateway-request-id',
  object: 'chat.completion', created: 1, model: 'local-upstream',
  choices: [{ index: 0, message: { role: 'assistant', content: 'raw synthetic provider answer' }, finish_reason: 'stop' }],
  usage: { prompt_tokens: 3, completion_tokens: 4, total_tokens: 7 },
};
function json(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body) });
  res.end(body);
}
http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/stats') return json(res, 200, { calls });
  if (req.method !== 'POST' || req.url !== '/v1/chat/completions') return json(res, 404, {});
  if (req.headers.authorization !== 'Bearer ' + process.env.FIXTURE_MODEL_KEY) return json(res, 401, {});
  let body = '', oversized = false;
  req.on('data', chunk => {
    body += chunk;
    if (Buffer.byteLength(body) > 65536) { oversized = true; req.destroy(); }
  });
  req.on('end', () => {
    if (oversized) return;
    try {
      const input = JSON.parse(body);
      if (input.stream || input.model !== 'local-upstream' || !Array.isArray(input.messages)) return json(res, 400, {});
      calls += 1;
      json(res, 200, response);
    } catch { json(res, 400, {}); }
  });
}).listen(8099, '0.0.0.0');
