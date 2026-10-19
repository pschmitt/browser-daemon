// Real Chromium extension + daemon round trip in isolated temporary profiles.
// Node 22+ provides fetch and WebSocket; no npm dependencies are needed.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdtemp, rm, writeFile} from 'node:fs/promises';
import {createServer as httpServer} from 'node:http';
import {createServer as tcpServer} from 'node:net';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {randomBytes} from 'node:crypto';

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(fn) {
  const deadline = Date.now() + 45000;
  while (Date.now() < deadline) { try { const value = await fn(); if (value) return value; } catch {} await sleep(100); }
  throw new Error('Browser integration check timed out');
}
async function freePort() {
  const s = tcpServer(); await new Promise(resolve => s.listen(0, '127.0.0.1', resolve));
  const port = s.address().port; await new Promise(resolve => s.close(resolve)); return port;
}
class CDP {
  constructor(socket) {
    this.socket = socket; this.id = 0; this.pending = new Map();
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data); const p = this.pending.get(message.id);
      if (p) { this.pending.delete(message.id); clearTimeout(p.timer); message.error ? p.reject(new Error('CDP rejected an integration operation')) : p.resolve(message.result); }
    });
  }
  static async open(url) {
    const socket = new WebSocket(url);
    await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, {once: true}); socket.addEventListener('error', reject, {once: true}); });
    return new CDP(socket);
  }
  call(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error('CDP check timed out')); }, 15000);
      this.pending.set(id, {resolve, reject, timer}); this.socket.send(JSON.stringify({id, method, params}));
    });
  }
  async evaluate(expression) {
    const value = await this.call('Runtime.evaluate', {expression, awaitPromise: true, returnByValue: true});
    if (value.exceptionDetails) throw new Error('Extension integration script failed');
    return value.result.value;
  }
}

const directory = await mkdtemp(join(tmpdir(), 'browser-node-chromium-'));
const children = [];
const sockets = [];
let pageServer;
try {
  const daemonPort = await freePort(); const chromePort = await freePort();
  const secret = randomBytes(32).toString('hex');
  const secretFile = join(directory, 'test-secret');
  await writeFile(secretFile, secret, {mode: 0o600});
  const base = `http://127.0.0.1:${daemonPort}`;
  const launch = (program, args) => {
    const child = spawn(program, args, {stdio: 'ignore'}); children.push(child); return child;
  };
  launch(process.env.BROWSER_DAEMON || resolve('target/debug/browser-daemon'), ['run', '--bind', `127.0.0.1:${daemonPort}`, '--public-url', base, '--profile-dir', join(directory, 'daemon/profiles'), '--recordings-dir', join(directory, 'daemon/recordings'), '--cluster-secret-file', secretFile, '--record-sessions', 'false']);
  await until(async () => (await fetch(`${base}/health`)).ok);
  async function api(path, method = 'GET', body) {
    const response = await fetch(`${base}${path}`, {method, headers: {'X-Browser-Cluster-Secret': secret, 'Content-Type': 'application/json'}, body: body === undefined ? undefined : JSON.stringify(body)});
    return {status: response.status, value: await response.json()};
  }
  pageServer = httpServer((req, res) => {
    res.writeHead(200, {'Content-Type': 'text/html'});
    res.end('<!doctype html><title>Chromium integration</title><input id="field"><button onclick="window.trusted=event.isTrusted">Click</button>');
  });
  await new Promise(resolve => pageServer.listen(0, '127.0.0.2', resolve));
  const pageURL = `http://127.0.0.2:${pageServer.address().port}/`;
  launch(process.env.CHROMIUM || 'chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check', `--user-data-dir=${join(directory, 'chromium')}`, `--remote-debugging-port=${chromePort}`, '--remote-debugging-address=127.0.0.1', `--load-extension=${resolve('browser/dist/chromium')}`, pageURL]);
  const targets = () => fetch(`http://127.0.0.1:${chromePort}/json/list`).then(r => r.json());
  const worker = await until(async () => (await targets()).find(t => t.type === 'service_worker' && t.url.endsWith('/background.js')));
  const origin = worker.url.replace(/\/background.js$/, '');
  const popupTarget = await fetch(`http://127.0.0.1:${chromePort}/json/new?${encodeURIComponent(`${origin}/popup.html`)}`, {method: 'PUT'}).then(r => r.json());
  const popup = await CDP.open(popupTarget.webSocketDebuggerUrl); sockets.push(popup.socket);
  const message = value => popup.evaluate(`chrome.runtime.sendMessage(${JSON.stringify(value)})`);
  // /json/new advertises the target before its extension page has committed.
  // Wait for that page and its background receiver before sending the code.
  await until(() => popup.evaluate(`location.href === ${JSON.stringify(`${origin}/popup.html`)} && document.readyState === 'complete' && typeof globalThis.chrome?.runtime?.sendMessage === 'function'`));
  await until(async () => Array.isArray((await message({type: 'state'}))?.tabs));
  const pairing = await api('/api/extension/pair', 'POST', {origin});
  assert.equal(pairing.status, 200);
  assert.ok(!(await message({type: 'connect', server: base, code: pairing.value.code, name: 'Chromium integration'})).error);
  const state = await until(async () => { const s = await message({type: 'state'}); return s.registration && s; });
  const tab = state.tabs.find(t => t.url === pageURL); assert.ok(tab);
  await message({type: 'share', tabs: [tab.id], urls: {[tab.id]: pageURL}, control: false});
  const browserId = state.registration.id;
  async function command(action, params = {}, tabId = tab.id) {
    return api(`/api/extension/nodes/${browserId}/command`, 'POST', {tab: tabId, action, params});
  }
  const screenshot = await command('screenshot');
  assert.equal(screenshot.status, 200); assert.ok(screenshot.value.result.data.startsWith('/9j'));
  assert.equal((await command('type', {text: 'refuse'})).status, 403);
  assert.equal((await command('snapshot', {}, 999999)).status, 403);
  await message({type: 'share', tabs: [tab.id], urls: {[tab.id]: pageURL}, control: true});
  assert.equal((await command('cdp', {method: 'Browser.close'})).status, 403);
  let result = await command('cdp', {method: 'Runtime.evaluate', params: {expression: 'document.querySelector("input").focus()'}});
  assert.equal(result.status, 200);
  assert.equal((await command('type', {text: 'hello'})).status, 200);
  result = await command('cdp', {method: 'Runtime.evaluate', params: {expression: 'document.querySelector("input").value', returnByValue: true}});
  assert.equal(result.value.result.result.value, 'hello');
  await command('navigate', {url: `${pageURL}next`});
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs.length === 0);
  assert.equal((await command('screenshot')).status, 403);
  await api(`/api/extension/nodes/${browserId}`, 'DELETE');
  await until(async () => (await message({type: 'state'})).registration === null);
  console.log('Chromium: extension pairing, screenshot, remote typing, view-only enforcement, unshared-tab rejection, protocol limits, navigation revocation and disconnect passed');
} finally {
  for (const socket of sockets) socket.close();
  for (const child of children) child.kill('SIGTERM');
  await Promise.all(children.map(async child => {
    if (child.exitCode !== null) return;
    await Promise.race([new Promise(resolve => child.once('exit', resolve)), sleep(5000)]);
    if (child.exitCode === null) child.kill('SIGKILL');
  }));
  if (pageServer) await new Promise(resolve => pageServer.close(resolve));
  await rm(directory, {recursive: true, force: true});
}
