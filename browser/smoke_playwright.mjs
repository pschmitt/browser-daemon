// Playwright against a shared browser: connectOverCDP to the daemon's browser-level
// endpoint for a browser node, then drive its shared tabs. Real Chromium, daemon and
// extension, isolated profiles. Needs playwright-core (no browsers): PLAYWRIGHT_DIR
// is a directory where it is installed (npm install --prefix DIR playwright-core).
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdtemp, rm, writeFile} from 'node:fs/promises';
import {createServer as httpServer} from 'node:http';
import {createServer as tcpServer} from 'node:net';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {randomBytes} from 'node:crypto';
import {createRequire} from 'node:module';

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
    if (value.exceptionDetails) throw new Error(`Integration script failed: ${value.exceptionDetails.exception?.description || value.exceptionDetails.text}`);
    return value.result.value;
  }
}


const {chromium} = createRequire(join(resolve(process.env.PLAYWRIGHT_DIR || '.'), 'noop.js'))('playwright-core');
const directory = await mkdtemp(join(tmpdir(), 'browser-node-playwright-'));
const children = [];
try {
  const daemonPort = await freePort(); const chromePort = await freePort();
  const secret = randomBytes(32).toString('hex');
  const secretFile = join(directory, 'test-secret');
  await writeFile(secretFile, secret, {mode: 0o600});
  const base = `http://127.0.0.1:${daemonPort}`;
  // SMOKE_LOG=FILE keeps the daemon's log.
  const logFd = process.env.SMOKE_LOG ? (await import('node:fs')).openSync(process.env.SMOKE_LOG, 'w') : 'ignore';
  const launch = (program, args) => { const child = spawn(program, args, {stdio: ['ignore', program.endsWith('browser-daemon') ? logFd : 'ignore', program.endsWith('browser-daemon') ? logFd : 'ignore']}); children.push(child); return child; };
  launch(resolve('target/debug/browser-daemon'), ['run', '--bind', `127.0.0.1:${daemonPort}`, '--public-url', base, '--profile-dir', join(directory, 'daemon/profiles'), '--recordings-dir', join(directory, 'daemon/recordings'), '--cluster-secret-file', secretFile, '--record-sessions', 'false']);
  await until(async () => (await fetch(`${base}/health`)).ok);
  const pageServer = httpServer((req, res) => {
    res.writeHead(200, {'Content-Type': 'text/html'});
    if (req.url === '/form') return res.end('<!doctype html><title>Form</title><input id=name><button id=go onclick="document.title = document.getElementById(\'name\').value">Go</button>');
    res.end('<!doctype html><title>Start</title><h1>Start</h1>');
  });
  await new Promise(r => pageServer.listen(0, '127.0.0.2', r));
  const pageURL = `http://127.0.0.2:${pageServer.address().port}/`;
  launch(process.env.CHROMIUM || 'chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check', `--user-data-dir=${join(directory, 'chromium')}`, `--remote-debugging-port=${chromePort}`, '--remote-debugging-address=127.0.0.1', `--load-extension=${resolve('browser/dist/chromium')}`, pageURL]);
  const targets = () => fetch(`http://127.0.0.1:${chromePort}/json/list`).then(r => r.json());
  const worker = await until(async () => (await targets()).find(t => t.type === 'service_worker' && t.url.endsWith('/background.js')));
  const origin = worker.url.replace(/\/background.js$/, '');
  const open = async url => { const t = await fetch(`http://127.0.0.1:${chromePort}/json/new?${encodeURIComponent(url)}`, {method: 'PUT'}).then(r => r.json()); return CDP.open(t.webSocketDebuggerUrl); };
  const popup = await open(`${origin}/popup.html`);
  await until(() => popup.evaluate(`document.readyState === 'complete' && !!globalThis.chrome?.runtime?.sendMessage`));
  const message = v => popup.evaluate(`chrome.runtime.sendMessage(${JSON.stringify(v)})`);
  await until(async () => Array.isArray((await message({type: 'state'}))?.tabs));
  await message({type: 'setDaemonAllowlist', origins: [base]});
  await message({type: 'setControl', control: true});
  const pairing = await (await fetch(`${base}/api/extension/pair`, {method: 'POST', headers: {'X-Browser-Cluster-Secret': secret, 'Content-Type': 'application/json'}, body: JSON.stringify({origin})})).json();
  await message({type: 'connect', server: base, code: pairing.code, name: 'playwright'});
  const state = await until(async () => { const s = await message({type: 'state'}); return s.registration && s.tabs.some(t => t.url === pageURL && t.shared && t.control) && s; });
  const headers = {'X-Browser-Cluster-Secret': secret};

  // The browser-level endpoint is found through its /json/version.
  const browser = await chromium.connectOverCDP(`${base}/api/extension/nodes/${state.registration.id}`, {headers, timeout: 30000});
  const [context] = browser.contexts();
  const page = await until(async () => context.pages().find(p => p.url() === pageURL));
  assert.equal(await page.title(), 'Start');
  await page.goto(`${pageURL}form`);
  await page.fill('#name', 'from playwright');
  await page.click('#go');
  await page.waitForFunction(() => document.title === 'from playwright');
  assert.equal(await page.evaluate(() => 6 * 7), 42);
  assert.ok((await page.screenshot()).length > 1000, 'screenshot');
  // Requests can be intercepted while connected...
  await page.route('**/intercepted', route => route.fulfill({contentType: 'text/html', body: '<title>Fulfilled</title>'}));
  await page.goto(`${pageURL}intercepted`);
  assert.equal(await page.title(), 'Fulfilled');
  // ...and a new page is a new shared tab of the owner's browser.
  const fresh = await context.newPage();
  await fresh.goto(`${pageURL}second`);
  assert.equal(await fresh.title(), 'Start');
  assert.ok((await message({type: 'state'})).tabs.some(t => t.url === `${pageURL}second` && t.shared));
  await fresh.close();
  await until(async () => !(await message({type: 'state'})).tabs.some(t => t.url === `${pageURL}second`));
  // Profile-wide and disk access stay closed.
  const session = await context.newCDPSession(page);
  await assert.rejects(session.send('Network.getAllCookies'), /not permitted/);
  await assert.rejects(session.send('DOM.setFileInputFiles', {files: ['/etc/passwd'], nodeId: 1}), /not permitted/);
  await assert.rejects(context.browser().newContext(), /own context/);
  await browser.close();
  // Leaving undoes what the client set: the interception no longer answers.
  const tab = (await message({type: 'state'})).tabs.find(t => t.url === `${pageURL}intercepted`);
  await fetch(`${base}/api/extension/nodes/${state.registration.id}/command`, {method: 'POST', headers: {...headers, 'Content-Type': 'application/json'}, body: JSON.stringify({tab: tab.id, action: 'navigate', params: {url: `${pageURL}intercepted`}})});
  await until(async () => (await message({type: 'state'})).tabs.find(t => t.id === tab.id)?.title === 'Start');
  console.log('Playwright: connectOverCDP to a shared browser, its tabs as pages, goto/fill/click/evaluate/screenshot, request interception, new and closed pages, profile-wide methods refused, cleanup on disconnect passed');
} finally {
  for (const c of children) c.kill('SIGTERM');
  await sleep(1000);
  await rm(directory, {recursive: true, force: true});
}
process.exit(0);
