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
  const pageServer = httpServer((req, res) => { res.writeHead(200, {'Content-Type': 'text/html'}); if (req.url === '/login') return res.end('<!doctype html><title>Sign in</title><form><input name=user autocomplete=username><input name=pw type=password></form>'); res.end('<!doctype html><title>Later tab</title><h1>Later</h1><p id=c></p><div id=drop-left>Left target</div><div id=drop-right>Right target</div><script>let n=0;(function f(){document.getElementById("c").textContent=n++;requestAnimationFrame(f)})(); for(const id of ["drop-left","drop-right"]) { const e=document.getElementById(id); Object.assign(e.style,{position:"fixed",top:"0",left:id==="drop-left"?"0":"100px",width:"100px",height:"100px"}); e.addEventListener("drop",event=>window.__lastDrop={id:event.currentTarget.id,x:event.clientX,y:event.clientY}); }</script>'); });
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
  await message({type: 'shareFiles', enabled: true});
  // This scenario checks explicit follow-focus behavior; the product default is off.
  await message({type: 'follow', enabled: true});
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
  // + opens and switches to a real blank tab before its address is entered.
  await view.evaluate(`document.getElementById('tabAddBtn').click()`);
  const blank = await until(async () => (await message({type: 'state'})).tabs.find(t => t.url === 'about:blank'));
  await until(async () => (await view.evaluate(`new URLSearchParams(location.search).get('page') === ${JSON.stringify(String(blank.id))} && document.activeElement?.id === 'urlInput'`)));
  assert.equal(await view.evaluate(`document.querySelector('.tab-strip-item.pending') === null`), true);
  await view.evaluate(`(() => { document.getElementById('urlInput').value = ${JSON.stringify(`${pageURL}plus`)}; document.getElementById('urlBar').requestSubmit(); })()`);
  const plus = await until(async () => (await message({type: 'state'})).tabs.find(t => t.id === blank.id && t.url === `${pageURL}plus`));
  await until(async () => (await view.evaluate(`new URLSearchParams(location.search).get('page') === ${JSON.stringify(String(plus.id))}`)));
  // File transfer dispatches the drop to the element under the pointer and
  // preserves that point so pages can distinguish nearby drop zones.
  await view.evaluate(`drv.cdp.call('Page.startFiles')`);
  const fileTarget = await view.evaluate(`drv.cdp.call('Page.fileTarget', {x: 150, y: 50})`);
  assert.ok(fileTarget.target);
  const page = await CDP.open((await targets()).find(t => t.url === pageURL).webSocketDebuggerUrl);
  const hit = await page.evaluate(`document.elementFromPoint(150, 50)?.id`);
  assert.equal(hit, 'drop-right');
  const upload = await view.evaluate(`drv.cdp.call('Page.beginFile', {target: ${JSON.stringify(fileTarget.target)}, name: 'point.txt', type: 'text/plain', size: 0, lastModified: 0})`);
  assert.ok(upload.id);
  const applied = await view.evaluate(`drv.cdp.call('Page.applyFiles', {target: ${JSON.stringify(fileTarget.target)}, files: [${JSON.stringify(upload.id)}]})`);
  assert.equal(applied.count, 1);
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
  // Vault logins: the live view asks, the secret agent (played here) fills, the
  // extension puts the values into the tab's form.
  const agent = (path, init = {}) => fetch(`${base}/api${path}`, {...init, headers: {'X-Browser-Cluster-Secret': secret, 'Content-Type': 'application/json', ...init.headers}});
  await message({type: 'share', tabs: [typed.id], urls: {[typed.id]: `${pageURL}typed`}, control: true});
  await fetch(`${base}/api/extension/nodes/${state.registration.id}/command`, {method: 'POST', headers: {'X-Browser-Cluster-Secret': secret, 'Content-Type': 'application/json'}, body: JSON.stringify({tab: typed.id, action: 'navigate', params: {url: `${pageURL}login`}})});
  await until(async () => (await message({type: 'state'})).tabs.find(t => t.id === typed.id && t.url === `${pageURL}login` && t.shared));
  await until(async () => (await (await fetch(`${base}/api/extension/nodes`, {headers: {'X-Browser-Cluster-Secret': secret}})).json()).nodes.some(n => n.tabs?.some(t => t.id === typed.id && t.url === `${pageURL}login`)));
  const taken = agent('/secret-agent/next').then(r => r.json());
  await sleep(300);
  const asked = await (await agent(`/extension/nodes/${state.registration.id}/tabs/${typed.id}/autofill`, {method: 'POST', body: JSON.stringify({page_id: String(typed.id), entry: 'entry-1', fields: ['username', 'password']})})).json();
  assert.ok(asked.request, `vault request: ${JSON.stringify(asked)}`);
  const job = await taken;
  assert.equal(job.kind, 'fill');
  const loginOrigin = new URL(pageURL).origin;
  const filled = await (await agent(`/pages/${job.page_id}/autofill`, {method: 'POST', body: JSON.stringify({request: job.id, host: job.rp_id, uris: [{uri: loginOrigin, match_type: null}], username: 'alice', password: 's3cret'})})).json();
  assert.deepEqual(filled.filled, ['username', 'password'], JSON.stringify(filled));
  const loginTab = (await targets()).find(t => t.url === `${pageURL}login`);
  const login = await CDP.open(loginTab.webSocketDebuggerUrl);
  assert.equal(await login.evaluate(`[document.querySelector('[name=user]').value, document.querySelector('[name=pw]').value].join()`), 'alice,s3cret');
  // A second fill for the same approval is refused.
  const again = await agent(`/pages/${job.page_id}/autofill`, {method: 'POST', body: JSON.stringify({request: job.id, host: job.rp_id, uris: [{uri: loginOrigin, match_type: null}], username: 'x'})});
  assert.equal(again.status, 403);
  // The simplified rbw prompt presents accounts as keyboard-selectable cards.
  // Picking one must pass its exact id to the agent.
  await until(() => view.evaluate(`vaultReady() && pageId === ${JSON.stringify(String(typed.id))}`));
  await view.call('Page.bringToFront');
  await view.evaluate(`window.__savedPasskeyAgent = passkeyAgent; window.__passkeyEntry = null; passkeyAgent = async (_rp, find, _note, entry) => { if (find) return [{id:'personal',name:'Example',user:'alice'}, {id:'work',name:'Example Work',user:'alice-work'}]; window.__passkeyEntry = entry; return {}; }; void askPasskey({rpId:'example.org',challenge:'AQID',allow:[]}); true`);
  await until(() => view.evaluate(`document.querySelectorAll('#passkeyEntries input[type=radio]').length === 2`));
  assert.equal(await view.evaluate(`document.querySelector('#passkeyEntries input:checked').value`), 'personal');
  await view.evaluate(`document.querySelector('#passkeyEntries input[value=work]').click(); document.querySelector('#passkeyDialog button[value=vault]').click(); true`);
  assert.equal(await view.evaluate(`document.getElementById('passkeyDialog').returnValue`), 'vault');
  await until(() => view.evaluate(`window.__passkeyEntry === 'work'`));
  await view.evaluate(`passkeyAgent = async () => ({locked:true}); void askPasskey({rpId:'locked.example',challenge:'AQID',allow:[]}); true`);
  await until(() => view.evaluate(`document.querySelector('#passkeyDialog button[value=vault]').textContent === 'Unlock and continue'`));
  assert.equal(await view.evaluate(`document.querySelectorAll('#passkeyEntries input').length`), 0);
  await view.evaluate(`document.getElementById('passkeyDialog').close('cancel'); passkeyAgent = window.__savedPasskeyAgent; true`);
  // Passkeys: the extension hooks the tab; with the view watching, the page's request
  // reaches the view, and the vault signs for the tab's own origin.
  await until(() => login.evaluate(`typeof globalThis.__bdPasskeyRepair === "function" && Date.now() - (globalThis.__bdPasskeyAlive || 0) < 8000`));
  // A provider injected later (as rbw does) must remain the fallback, while the
  // heartbeat restores interception for the controlling view.
  await login.evaluate(`window.__providerCalls = 0; window.__provider = async () => { window.__providerCalls++; return null; }; Object.defineProperty(navigator.credentials, 'get', {configurable: true, value: window.__provider}), true`);
  await until(() => login.evaluate(`navigator.credentials.get !== window.__provider`));
  // A conditional request (the passkey in a login field's suggestions) is offered by
  // the view, and withdrawn when the page aborts it.
  await until(() => login.evaluate(`globalThis.__bdPasskeyOffer === true`));
  await login.evaluate(`window.__abort = new AbortController(); void navigator.credentials.get({mediation: 'conditional', signal: window.__abort.signal, publicKey: {challenge: new Uint8Array([5, 6, 7, 8]), rpId: location.hostname}}).catch(e => { window.__aborted = e.name; }), true`);
  await until(() => view.evaluate(`!document.getElementById('pkOffer').hidden && passkeyOffer?.req.conditional === true`));
  await login.evaluate(`window.__abort.abort(), true`);
  await until(() => view.evaluate(`document.getElementById('pkOffer').hidden`));
  assert.equal(await login.evaluate(`window.__aborted`), 'AbortError');
  await view.evaluate(`window.__seen = []; passkeyRequested = (d, p) => window.__seen.push(JSON.parse(p))`);
  await login.evaluate(`void navigator.credentials.get({publicKey: {challenge: new Uint8Array([1, 2, 3, 4]), rpId: location.hostname}}).catch(() => {}), true`);
  const seenReq = await until(async () => (await view.evaluate(`window.__seen[0] || null`)));
  assert.equal(seenReq.rpId, new URL(pageURL).hostname);
  assert.equal(await login.evaluate(`window.__providerCalls`), 0, 'the provider must not open its local prompt');
  const signTaken = agent('/secret-agent/next').then(r => r.json());
  await sleep(300);
  const signAsked = await (await agent(`/extension/nodes/${state.registration.id}/tabs/${typed.id}/passkey`, {method: 'POST', body: JSON.stringify({page_id: String(typed.id), rp_id: seenReq.rpId, find: false, challenge: seenReq.challenge, allow: []})})).json();
  assert.ok(signAsked.request, JSON.stringify(signAsked));
  const signJob = await signTaken;
  assert.deepEqual([signJob.kind, signJob.origin, signJob.challenge], ['assert', loginOrigin, seenReq.challenge]);
  const wrongSite = await agent(`/extension/nodes/${state.registration.id}/tabs/${typed.id}/passkey`, {method: 'POST', body: JSON.stringify({page_id: '1', rp_id: 'example.org', find: false, challenge: 'AQID'})});
  assert.equal(wrongSite.status, 403);
  // The page's own dialogs show as the view's, never as the browser's confirm().
  // (The view is a tab of the shared browser here, behind the tab it follows: bring it up.)
  await view.call('Page.bringToFront');
  await login.evaluate(`setTimeout(() => { window.__said = confirm('Sure?'); }, 10), true`);
  await until(() => view.evaluate(`document.getElementById('askDialog').open && document.getElementById('askText').textContent === 'Sure?'`));
  await view.evaluate(`document.getElementById('askDialog').close('ok')`);
  await until(() => login.evaluate(`window.__said === true`));
  // A tab closed here reopens from the menu, at its address (after the closing is confirmed).
  const dashTab = (await message({type: 'state'})).tabs.find(t => t.url === `${pageURL}dash`);
  await message({type: 'share', tabs: [typed.id, dashTab.id], urls: {[typed.id]: `${pageURL}login`, [dashTab.id]: `${pageURL}dash`}, control: true});
  await until(() => view.evaluate(`!!sharedBrowser?.tabs.some(t => t.id === ${dashTab.id})`));
  await view.evaluate(`void closeTab(${JSON.stringify(String(dashTab.id))}), true`);
  await until(() => view.evaluate(`document.getElementById('askDialog').open`));
  await view.evaluate(`document.getElementById('askDialog').close('ok')`);
  await until(() => view.evaluate(`!document.getElementById('smReopen').hidden && closedTabs.at(-1)?.url === ${JSON.stringify(`${pageURL}dash`)}`));
  await until(async () => !(await message({type: 'state'})).tabs.some(t => t.url === `${pageURL}dash`));
  await view.evaluate(`document.getElementById('smReopen').click()`);
  await until(async () => (await message({type: 'state'})).tabs.some(t => t.url === `${pageURL}dash` && t.shared));
  console.log(`Handoff: a shared browser without tabs shows its first shared tab by itself; shared/unshared tabs appear in ${shown} ms / leave in ${hidden} ms; the address bar opens the first tab; + and the quality slider; screencast frames; zoom; the browser follows the view; the dashboard opens tabs in it; vault logins; passkeys; the view's dialogs; reopening closed tabs passed`);
} finally {
  for (const c of children) c.kill('SIGTERM');
  await sleep(1000);
  await rm(directory, {recursive: true, force: true});
}
// Open protocol sockets would keep Node waiting; reached only on success.
process.exit(0);
