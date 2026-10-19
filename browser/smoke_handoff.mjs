// The live view of a shared browser without tabs: it shows the first tab as soon
// as one is shared, without a click. Real Chromium and daemon, isolated profiles.
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
    if (value.exceptionDetails) throw new Error(`Integration script failed: ${value.exceptionDetails.exception?.description || value.exceptionDetails.text}`);
    return value.result.value;
  }
}


const directory = await mkdtemp(join(tmpdir(), 'browser-node-notab-'));
const children = [];
try {
  const daemonPort = await freePort(); const chromePort = await freePort();
  const secret = randomBytes(32).toString('hex');
  const secretFile = join(directory, 'test-secret');
  await writeFile(secretFile, secret, {mode: 0o600});
  const base = `http://127.0.0.1:${daemonPort}`;
  const launch = (program, args) => { const child = spawn(program, args, {stdio: 'ignore'}); children.push(child); return child; };
  launch(resolve('target/debug/browser-daemon'), ['run', '--bind', `127.0.0.1:${daemonPort}`, '--public-url', base, '--profile-dir', join(directory, 'daemon/profiles'), '--recordings-dir', join(directory, 'daemon/recordings'), '--cluster-secret-file', secretFile, '--record-sessions', 'false']);
  await until(async () => (await fetch(`${base}/health`)).ok);
  const pageServer = httpServer((req, res) => { res.writeHead(200, {'Content-Type': 'text/html'}); res.end('<!doctype html><title>Later tab</title><h1>Later</h1><p id=c></p><script>let n=0;(function f(){document.getElementById("c").textContent=n++;requestAnimationFrame(f)})()</script>'); });
  await new Promise(r => pageServer.listen(0, '127.0.0.2', r));
  const pageURL = `http://127.0.0.2:${pageServer.address().port}/`;
  launch('chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check', `--user-data-dir=${join(directory, 'chromium')}`, `--remote-debugging-port=${chromePort}`, '--remote-debugging-address=127.0.0.1', `--load-extension=${resolve('browser/dist/chromium')}`, pageURL]);
  const targets = () => fetch(`http://127.0.0.1:${chromePort}/json/list`).then(r => r.json());
  const worker = await until(async () => (await targets()).find(t => t.type === 'service_worker' && t.url.endsWith('/background.js')));
  const origin = worker.url.replace(/\/background.js$/, '');
  const open = async url => { const t = await fetch(`http://127.0.0.1:${chromePort}/json/new?${encodeURIComponent(url)}`, {method: 'PUT'}).then(r => r.json()); return CDP.open(t.webSocketDebuggerUrl); };
  const popup = await open(`${origin}/popup.html`);
  await until(() => popup.evaluate(`document.readyState === 'complete' && !!globalThis.chrome?.runtime?.sendMessage`));
  const message = v => popup.evaluate(`chrome.runtime.sendMessage(${JSON.stringify(v)})`);
  await until(async () => Array.isArray((await message({type: 'state'}))?.tabs));
  await message({type: 'setDaemonAllowlist', origins: [base]});
  await message({type: 'shareAll', enabled: false});
  const pairing = await (await fetch(`${base}/api/extension/pair`, {method: 'POST', headers: {'X-Browser-Cluster-Secret': secret, 'Content-Type': 'application/json'}, body: JSON.stringify({origin})})).json();
  await message({type: 'connect', server: base, code: pairing.code, name: 'notab'});
  const state = await until(async () => { const s = await message({type: 'state'}); return s.registration && s; });
  const view = await open('about:blank');
  await view.call('Network.enable');
  await view.call('Network.setExtraHTTPHeaders', {headers: {'X-Browser-Cluster-Secret': secret}});
  await view.call('Page.navigate', {url: `${base}/handoff/?browser=${state.registration.id}`});
  await sleep(3000);
  assert.match(await view.evaluate(`document.getElementById('msg').textContent`), /No shared tabs/);
  const tab = state.tabs.find(t => t.url === pageURL);
  await message({type: 'share', tabs: [tab.id], urls: {[tab.id]: pageURL}, control: true});
  await until(async () => (await view.evaluate(`!document.getElementById('overlay').classList.contains('show')`)));
  assert.equal(await view.evaluate(`new URLSearchParams(location.search).get('page')`), String(tab.id));
  // Sharing and unsharing reach the tab strip at once (long poll), not on the next round.
  const second = await fetch(`http://127.0.0.1:${chromePort}/json/new?${encodeURIComponent(`${pageURL}two`)}`, {method: 'PUT'}).then(r => r.json());
  const secondTab = await until(async () => (await message({type: 'state'})).tabs.find(t => t.url === `${pageURL}two`));
  const strip = () => view.evaluate(`document.querySelectorAll('.tab-strip-item').length`);
  let started = Date.now();
  await message({type: 'share', tabs: [tab.id, secondTab.id], urls: {[tab.id]: pageURL, [secondTab.id]: `${pageURL}two`}, control: true});
  await until(async () => (await strip()) === 2);
  const shown = Date.now() - started;
  started = Date.now();
  await message({type: 'share', tabs: [tab.id], urls: {[tab.id]: pageURL}, control: true});
  await until(async () => (await strip()) === 1);
  const hidden = Date.now() - started;
  assert.ok(shown < 1500 && hidden < 1500, `tab strip took ${shown} / ${hidden} ms`);
  await fetch(`http://127.0.0.1:${chromePort}/json/close/${second.id}`, {method: 'PUT'});
  // Nothing shared, but tabs may be opened: the address bar opens the first one.
  await message({type: 'share', tabs: [], urls: {}, control: false});
  await message({type: 'setControl', control: true});
  await until(async () => (await view.evaluate(`!document.getElementById('urlInput').readOnly && document.getElementById('overlay').classList.contains('show')`)));
  await view.evaluate(`(() => { const i = document.getElementById('urlInput'); i.value = ${JSON.stringify(`${pageURL}typed`)}; document.getElementById('urlBar').requestSubmit(); })()`);
  await until(async () => (await view.evaluate(`!document.getElementById('overlay').classList.contains('show') && new URLSearchParams(location.search).get('page')`)));
  const typed = (await message({type: 'state'})).tabs.find(t => t.url === `${pageURL}typed`);
  assert.ok(typed?.shared && typed.control, 'typed address opened a shared, controllable tab');
  assert.equal(await view.evaluate(`new URLSearchParams(location.search).get('page')`), String(typed.id));
  // + works as in a regular session: a "New Tab" placeholder and the focused
  // address bar; the submitted address becomes the real tab.
  await view.evaluate(`document.getElementById('tabAddBtn').click()`);
  assert.equal(await view.evaluate(`[!!document.querySelector('.tab-strip-item.pending'), document.activeElement?.id].join()`), 'true,urlInput');
  await view.evaluate(`(() => { document.getElementById('urlInput').value = ${JSON.stringify(`${pageURL}plus`)}; document.getElementById('urlBar').requestSubmit(); })()`);
  const plus = await until(async () => (await message({type: 'state'})).tabs.find(t => t.url === `${pageURL}plus`));
  await until(async () => (await view.evaluate(`new URLSearchParams(location.search).get('page') === ${JSON.stringify(String(plus.id))} && !document.querySelector('.tab-strip-item.pending')`)));
  // The view streams a screencast (not the screenshot fallback): frames keep coming.
  await until(async () => (await view.evaluate(`!!drv?.castParams`)));
  const seen = await view.evaluate(`drv.lastFrameAt || 0`);
  await sleep(400);
  assert.ok((await view.evaluate(`drv.lastFrameAt || 0`)) > seen, 'screencast frames arrive');
  // The picture quality slider restarts the screencast at the chosen quality.
  await view.evaluate(`(() => { const s = document.getElementById('smQuality'); s.value = '30'; s.dispatchEvent(new Event('input')); s.dispatchEvent(new Event('change')); })()`);
  await until(async () => (await view.evaluate(`drv.castParams.quality`)) === 30);
  assert.equal(await view.evaluate(`localStorage.getItem('bd-jpeg-quality')`), '30');
  // Zoom (pinch, Ctrl+wheel): the point under the fingers stays put, input still maps
  // to the same page point, and frames get sharper for the magnified picture.
  const castWidth = await view.evaluate(`drv.castParams.maxWidth`);
  const mapped = await view.evaluate(`(() => { const r = E.stage.getBoundingClientRect(), c = {clientX: r.left + r.width / 3, clientY: r.top + r.height / 3}; const a = toPage(c); zoomAt(c.clientX, c.clientY, 2); const b = toPage(c); return [a.x, a.y, b.x, b.y, document.getElementById('zoomReset').hidden]; })()`);
  assert.ok(Math.abs(mapped[0] - mapped[2]) < 1 && Math.abs(mapped[1] - mapped[3]) < 1 && mapped[4] === false, `zoom keeps the point: ${mapped}`);
  await until(async () => (await view.evaluate(`drv.castParams.maxWidth`)) > castWidth);
  await view.evaluate(`document.getElementById('zoomReset').click()`);
  assert.equal(await view.evaluate(`E.view.style.transform`), '');
  // The browser follows the view: switching to a tab brings it to the front there.
  await view.evaluate(`switchTab(${JSON.stringify(String(typed.id))})`);
  await until(() => popup.evaluate(`chrome.tabs.get(${typed.id}).then(t => t.active)`));
  // The dashboard's New browser session dialog opens a tab in a connected browser.
  const dash = await open('about:blank');
  await dash.call('Network.enable');
  await dash.call('Network.setExtraHTTPHeaders', {headers: {'X-Browser-Cluster-Secret': secret}});
  await dash.call('Page.navigate', {url: `${base}/`});
  await until(() => dash.evaluate(`typeof extensionBrowserNodes !== 'undefined' && extensionBrowserNodes.length > 0 && !!ov`));
  await dash.evaluate(`openNewSession()`);
  await dash.evaluate(`document.querySelector('#sessionNodes button[data-node^="browser:"]').click()`);
  assert.equal(await dash.evaluate(`[document.getElementById('newSessionForm').classList.contains('to-browser'), document.getElementById('sessionSubmitText').textContent, document.getElementById('sessionSubmit').disabled].join()`), 'true,Open tab,false');
  await dash.evaluate(`(() => { document.getElementById('sessionUrl').value = ${JSON.stringify(`${pageURL}dash`)}; document.getElementById('sessionHandoff').checked = false; document.getElementById('newSessionForm').requestSubmit(); })()`);
  const fromDash = await until(async () => (await message({type: 'state'})).tabs.find(t => t.url === `${pageURL}dash`));
  assert.ok(fromDash.shared && fromDash.control, 'the dashboard opened a shared, controllable tab');
  console.log(`Handoff: a shared browser without tabs shows its first shared tab by itself; shared/unshared tabs appear in ${shown} ms / leave in ${hidden} ms; the address bar opens the first tab; + and the quality slider; screencast frames; zoom; the browser follows the view; the dashboard opens tabs in it passed`);
} finally {
  for (const c of children) c.kill('SIGTERM');
  await sleep(1000);
  await rm(directory, {recursive: true, force: true});
}
// Open protocol sockets would keep Node waiting; reached only on success.
process.exit(0);
