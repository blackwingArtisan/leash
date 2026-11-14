// Local owner console: serves the dashboard, signs owner actions with the
// owner key from ~/.near-credentials, and streams agent runs to the page.
// Usage: npm start  ->  http://localhost:8787
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { HERE, accountFor, call, loadDeployment, view } from './near.mjs';

const PORT = Number(process.env.PORT || 8787);
const WEB = path.join(HERE, '..', 'web');
const d = loadDeployment();
const owner = accountFor(d.owner);
let running = false;

const ownerActions = {
  approve: ({ id }) => call(owner, d.vault, 'approve', { record_id: Number(id) }),
  reject: ({ id }) => call(owner, d.vault, 'reject', { record_id: Number(id) }),
  freeze: () => call(owner, d.vault, 'freeze', { agent: d.agent, reason: 'frozen by owner' }),
  unfreeze: () => call(owner, d.vault, 'unfreeze', { agent: d.agent }),
  // Puts the demo back to a clean start: unfreeze, clear held requests, reset today's counters.
  reset: async () => {
    for (const r of await view(d.vault, 'get_pending')) await call(owner, d.vault, 'reject', { record_id: r.id });
    const a = await view(d.vault, 'get_agent', { agent: d.agent });
    if (a.state.frozen) await call(owner, d.vault, 'unfreeze', { agent: d.agent });
    await call(owner, d.vault, 'reset_today', { agent: d.agent });
  },
};

function json(res, code, body) {
  res.writeHead(code, { 'content-type': 'application/json' });
  res.end(JSON.stringify(body));
}

function readBody(req) {
  return new Promise((ok) => {
    let s = '';
    req.on('data', (c) => (s += c));
    req.on('end', () => ok(s ? JSON.parse(s) : {}));
  });
}

http
  .createServer(async (req, res) => {
    const url = new URL(req.url, 'http://x');
    try {
      if (url.pathname === '/deployment.json') return json(res, 200, { ...d, ownerConsole: true });
      if (url.pathname === '/api/run') {
        if (running) return json(res, 409, { error: 'agent already running' });
        running = true;
        res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
        const brain = url.searchParams.get('brain') === 'claude' ? 'claude' : 'hijacked';
        const p = spawn(process.execPath, [path.join(HERE, 'agent.mjs'), `--brain=${brain}`], { cwd: HERE });
        const send = (line) => line && res.write(`data: ${JSON.stringify(line)}\n\n`);
        let buf = '';
        const onData = (c) => {
          buf += c;
          const lines = buf.split('\n');
          buf = lines.pop();
          lines.forEach(send);
        };
        p.stdout.on('data', onData);
        p.stderr.on('data', onData);
        p.on('close', () => {
          send(buf);
          res.write('event: done\ndata: {}\n\n');
          res.end();
          running = false;
        });
        return;
      }
      const action = url.pathname.match(/^\/api\/(\w+)$/)?.[1];
      if (req.method === 'POST' && ownerActions[action]) {
        await ownerActions[action](await readBody(req));
        return json(res, 200, { ok: true });
      }
      const file = path.join(WEB, url.pathname === '/' ? 'index.html' : path.normalize(url.pathname));
      if (!file.startsWith(WEB) || !fs.existsSync(file)) return json(res, 404, { error: 'not found' });
      const type = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' }[path.extname(file)];
      res.writeHead(200, { 'content-type': type || 'application/octet-stream' });
      fs.createReadStream(file).pipe(res);
    } catch (e) {
      json(res, 500, { error: String(e.message || e) });
    }
  })
  .listen(PORT, () => console.log(`Leash owner console on http://localhost:${PORT}`));
