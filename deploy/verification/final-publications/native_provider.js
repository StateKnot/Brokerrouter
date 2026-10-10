// Separate real primary-SDK model fixture: complete OpenAI JSON and SSE contracts.
// It stores only aggregate invocation counts; no request/authorization/text logs.
const http = require('node:http');
let calls = 0, streamingCalls = 0;
const common = { id: 'provider-id-is-not-the-gateway-request-id', created: 1, model: 'local-upstream' };
const usage = { prompt_tokens: 3, completion_tokens: 4, total_tokens: 7 };
function json(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body) });
  res.end(body);
}
http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/stats') return json(res, 200, { calls, streaming_calls: streamingCalls });
  if (req.method !== 'POST' || req.url !== '/v1/chat/completions') return json(res, 404, {});
  if (req.headers.authorization !== 'Bearer ' + process.env.FIXTURE_MODEL_KEY) return json(res, 401, {});
  let body = '', oversized = false;
  req.on('data', chunk => {
    body += chunk;
    if (Buffer.byteLength(body) > 1024 * 1024) { oversized = true; req.destroy(); }
  });
  req.on('end', () => {
    if (oversized) return;
    let input;
    try { input = JSON.parse(body); } catch { return json(res, 400, {}); }
    if (input.model !== 'local-upstream' || !Array.isArray(input.messages)) return json(res, 400, {});
    calls += 1;
    if (!input.stream) return json(res, 200, { ...common, object: 'chat.completion',
      choices: [{ index: 0, message: { role: 'assistant', content: 'raw synthetic provider answer' }, finish_reason: 'stop' }], usage });
    streamingCalls += 1;
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
    const emit = value => res.write('data: ' + JSON.stringify({ ...common, object: 'chat.completion.chunk', ...value }) + '\n\n');
    emit({ choices: [{ index: 0, delta: { role: 'assistant', content: '' }, finish_reason: null }] });
    emit({ choices: [{ index: 0, delta: { content: 'raw synthetic provider answer' }, finish_reason: null }] });
    emit({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] });
    emit({ choices: [], usage });
    res.end('data: [DONE]\n\n');
  });
}).listen(8099, '0.0.0.0');
