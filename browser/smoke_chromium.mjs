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
    this.socket = socket; this.id = 0; this.pending = new Map(); this.events = [];
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data);
      if (message.method) {
        this.events.push(message);
        // Like the live view: acknowledge frames so the screencast keeps going.
        if (this.autoAck && message.method === 'Page.screencastFrame') this.call('Page.screencastFrameAck', {sessionId: message.params.sessionId}).catch(() => {});
        return;
      }
      const p = this.pending.get(message.id);
      if (p) { this.pending.delete(message.id); clearTimeout(p.timer); message.error ? p.reject(new Error(`CDP rejected ${p.method}: ${message.error.message || message.error}`)) : p.resolve(message.result); }
    });
  }
  static async open(url, options) {
    const socket = new WebSocket(url, options);
    await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, {once: true}); socket.addEventListener('error', reject, {once: true}); });
    return new CDP(socket);
  }
  call(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP check timed out: ${method}`)); }, 15000);
      this.pending.set(id, {resolve, reject, timer, method}); this.socket.send(JSON.stringify({id, method, params}));
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
  const pageURL = `http://fixture.browser.test:${pageServer.address().port}/`;
  launch(process.env.CHROMIUM || 'chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check', `--user-data-dir=${join(directory, 'chromium')}`, `--remote-debugging-port=${chromePort}`, '--remote-debugging-address=127.0.0.1', '--host-resolver-rules=MAP fixture.browser.test 127.0.0.2', '--no-proxy-server', `--load-extension=${resolve('browser/dist/chromium')}`, pageURL]);
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
  assert.ok(!(await message({type: 'setDaemonAllowlist', origins: [base]})).error);
  // Per-tab grants first; sharing everything (the default) is checked at the end.
  assert.ok(!(await message({type: 'shareAll', enabled: false})).error);
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
  // The live view: the regular CDP driver over the daemon's relay socket.
  const liveURL = id => `ws://127.0.0.1:${daemonPort}/api/extension/nodes/${browserId}/tabs/${id}/cdp`;
  const live = await CDP.open(liveURL(tab.id), {headers: {'X-Browser-Cluster-Secret': secret}}); sockets.push(live.socket);
  await live.call('Page.enable');
  await live.call('Page.startScreencast', {format: 'jpeg', quality: 60, maxWidth: 800, maxHeight: 600});
  const frame = await until(() => live.events.find(e => e.method === 'Page.screencastFrame'));
  assert.ok(frame.params.data.startsWith('/9j'));
  await live.call('Page.screencastFrameAck', {sessionId: frame.params.sessionId});
  const box = await live.call('Runtime.evaluate', {expression: 'JSON.stringify(document.querySelector("button").getBoundingClientRect())', returnByValue: true});
  const {x, y, width, height} = JSON.parse(box.result.value);
  for (const type of ['mousePressed', 'mouseReleased']) await live.call('Input.dispatchMouseEvent', {type, x: x + width / 2, y: y + height / 2, button: 'left', clickCount: 1});
  assert.equal((await live.call('Runtime.evaluate', {expression: 'window.trusted', returnByValue: true})).result.value, true);
  await assert.rejects(live.call('Target.createTarget', {url: 'about:blank'}));
  await assert.rejects(live.call('Page.navigate', {url: 'file:///etc/passwd'}));
  // A reload keeps the view's frames coming (for the tab in front: Chromium
  // paints no other; the live view shows screenshots of those).
  await fetch(`http://127.0.0.1:${chromePort}/json/activate/${(await targets()).find(t => t.url === pageURL).id}`);
  live.autoAck = true;
  for (const e of live.events) if (e.method === 'Page.screencastFrame') await live.call('Page.screencastFrameAck', {sessionId: e.params.sessionId}).catch(() => {});
  await live.call('Page.reload');
  await until(() => live.events.some(e => e.method === 'Page.loadEventFired'));
  const afterReload = live.events.length;
  // As the live view does after every load (an extension's debugger stops the screencast).
  await live.call('Page.stopScreencast');
  await live.call('Page.startScreencast', {format: 'jpeg', quality: 60, maxWidth: 800, maxHeight: 600});
  await until(() => live.events.slice(afterReload).some(e => e.method === 'Page.screencastFrame'));
  // Recording a shared browser: frames through the daemon's own relay channel.
  assert.equal((await api(`/api/sessions/${browserId}/record`, 'POST')).value.status, 'recording_started');
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.recording === true);
  await sleep(2500);
  assert.equal((await api(`/api/sessions/${browserId}/stop-record`, 'POST')).value.status, 'recording_stopped');
  const recorded = await until(async () => {
    const list = (await api('/api/recordings')).value;
    const r = (list.recordings || list).find(x => x.session_id === browserId && x.ended_at_secs);
    return r?.frame_count > 0 && r;
  });
  assert.equal(recorded.initial_url, pageURL);
  // A controller opens a tab: next to this one, shared with control.
  const openedTab = await command('open', {url: `${pageURL}opened`});
  assert.equal(openedTab.status, 200);
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs.some(t => t.id === openedTab.value.result.tab && t.control && t.url === `${pageURL}opened`));
  assert.equal((await command('open', {url: 'file:///etc/passwd'})).status, 403);
  assert.equal((await command('close', {}, openedTab.value.result.tab)).status, 200);
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs.length === 1);
  await fetch(`http://127.0.0.1:${chromePort}/json/activate/${(await targets()).find(t => t.url === pageURL).id}`);
  // Control follows the tab across navigations; the view keeps working.
  await live.call('Page.navigate', {url: `${pageURL}next`});
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs[0]?.url === `${pageURL}next`);
  assert.equal((await live.call('Runtime.evaluate', {expression: 'location.pathname', returnByValue: true})).result.value, '/next');
  assert.equal((await command('screenshot')).status, 200);
  // A daemon page pauses sharing: the tab is unlisted and its views close.
  assert.ok((await command('navigate', {url: `${base}/`})).value.error);
  const ownerTab = (await targets()).find(t => t.type === 'page' && t.url === `${pageURL}next`);
  const owner = await CDP.open(ownerTab.webSocketDebuggerUrl); sockets.push(owner.socket);
  await owner.call('Page.navigate', {url: `${base}/`});
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs.length === 0);
  await until(() => live.socket.readyState === WebSocket.CLOSED);
  assert.equal((await command('screenshot')).status, 403);
  // Back on a shareable page (navigated locally), sharing resumes by itself.
  const paused = (await targets()).find(t => t.type === 'page' && t.url.startsWith(base));
  const local = await CDP.open(paused.webSocketDebuggerUrl); sockets.push(local.socket);
  await local.call('Page.navigate', {url: pageURL});
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs[0]?.url === pageURL);
  // View only: frames, never input.
  await message({type: 'share', tabs: [tab.id], urls: {[tab.id]: pageURL}, control: false});
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs[0]?.control === false);
  const viewer = await CDP.open(liveURL(tab.id), {headers: {'X-Browser-Cluster-Secret': secret}}); sockets.push(viewer.socket);
  await viewer.call('Page.enable');
  await assert.rejects(viewer.call('Input.insertText', {text: 'refuse'}));
  await assert.rejects(viewer.call('Runtime.evaluate', {expression: '1'}));
  // New tabs join automatically with the last control choice.
  const opened = await fetch(`http://127.0.0.1:${chromePort}/json/new?${encodeURIComponent(`${pageURL}new`)}`, {method: 'PUT'}).then(r => r.json());
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs.some(t => t.url === `${pageURL}new` && t.control === false));
  await fetch(`http://127.0.0.1:${chromePort}/json/close/${opened.id}`, {method: 'PUT'});
  // Sharing everything: every shareable tab is listed; with control, a controller
  // opens the browser's first tab without any tab (0).
  await message({type: 'shareAll', enabled: true});
  await message({type: 'setControl', control: true});
  await until(async () => { const n = (await api('/api/extension/nodes')).value.nodes[0]; return n?.can_open && n.tabs.length >= 1 && n.tabs.every(t => t.control); });
  const first = await command('open', {url: `${pageURL}first`}, 0);
  assert.equal(first.status, 200);
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.tabs.some(t => t.id === first.value.result.tab));
  await message({type: 'setControl', control: false});
  await until(async () => (await api('/api/extension/nodes')).value.nodes[0]?.can_open !== true);
  assert.equal((await command('open', {url: `${pageURL}refused`}, 0)).status, 403);
  await api(`/api/extension/nodes/${browserId}`, 'DELETE');
  await until(async () => (await message({type: 'state'})).registration === null);
  console.log('Chromium: pairing, screenshots, typing, live CDP relay (screencast, trusted clicks, protocol limits, reload), recording, opening tabs, control across navigation, pausing on daemon pages, view-only enforcement, automatic sharing of new tabs, sharing all tabs, opening the first tab and disconnect passed');
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
