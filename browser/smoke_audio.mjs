// Stereo media audio reaches a view-only handoff and stops on owner revocation.
// Real Chromium, extension and daemon, with isolated profiles and fake media.
// Node 22+ provides fetch and WebSocket; no npm dependencies are needed.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {cp, mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
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


const directory = await mkdtemp(join(tmpdir(), 'browser-node-audio-'));
const children = [];
try {
  const daemonPort = await freePort(); const chromePort = await freePort();
  const secret = randomBytes(32).toString('hex');
  const secretFile = join(directory, 'test-secret');
  await writeFile(secretFile, secret, {mode: 0o600});
  const base = `http://127.0.0.1:${daemonPort}`;
  const launch = (program, args) => { const child = spawn(program, args, {stdio: 'ignore'}); children.push(child); return child; };
  launch(process.env.BROWSER_DAEMON || resolve('target/debug/browser-daemon'), ['run', '--bind', `127.0.0.1:${daemonPort}`, '--public-url', base, '--profile-dir', join(directory, 'daemon/profiles'), '--recordings-dir', join(directory, 'daemon/recordings'), '--cluster-secret-file', secretFile, '--record-sessions', 'false']);
  await until(async () => (await fetch(`${base}/health`)).ok);
  const wave = Buffer.alloc(44 + 48000 * 4);
  wave.write('RIFF'); wave.writeUInt32LE(wave.length - 8, 4); wave.write('WAVEfmt ', 8);
  wave.writeUInt32LE(16, 16); wave.writeUInt16LE(1, 20); wave.writeUInt16LE(2, 22);
  wave.writeUInt32LE(48000, 24); wave.writeUInt32LE(192000, 28);
  wave.writeUInt16LE(4, 32); wave.writeUInt16LE(16, 34); wave.write('data', 36); wave.writeUInt32LE(wave.length - 44, 40);
  for (let i = 0; i < 48000; i++) {
    wave.writeInt16LE(Math.round(Math.sin(i * 2 * Math.PI * 440 / 48000) * 12000), 44 + i * 4);
    wave.writeInt16LE(Math.round(Math.sin(i * 2 * Math.PI * 880 / 48000) * 12000), 46 + i * 4);
  }
  const pageServer = httpServer((req, res) => {
    if (req.url === '/tone.wav') { res.writeHead(200, {'Content-Type': 'audio/wav'}); res.end(wave); return; }
    res.writeHead(200, {'Content-Type': 'text/html'});
    res.end('<!doctype html><title>Stereo fixture</title><audio loop src="/tone.wav"></audio>');
  });
  await new Promise(r => pageServer.listen(0, '127.0.0.2', r));
  const pageURL = `http://127.0.0.2:${pageServer.address().port}/`;
  const extension = join(directory, 'extension');
  await cp(resolve('browser/dist/chromium'), extension, {recursive: true});
  const manifest = JSON.parse(await readFile(join(extension, 'manifest.json'), 'utf8'));
  // Headless fixture: preapprove site access, bypassing only the permissions dialog.
  manifest.host_permissions = ['http://*/*', 'https://*/*'];
  await writeFile(join(extension, 'manifest.json'), JSON.stringify(manifest));
  launch('chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check', `--user-data-dir=${join(directory, 'chromium')}`, `--remote-debugging-port=${chromePort}`, '--remote-debugging-address=127.0.0.1', `--load-extension=${extension}`, pageURL]);
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
  const source = await CDP.open((await targets()).find(t => t.url === pageURL).webSocketDebuggerUrl);
  const tab = state.tabs.find(t => t.url === pageURL);
  await message({type: 'share', tabs: [tab.id], urls: {[tab.id]: pageURL}, control: false});
  await message({type: 'shareAudio', enabled: true});
  // Simulate the owner's site-access approval without an interactive permissions dialog.
  await view.call('Page.navigate', {url: `${base}/handoff/?browser=${state.registration.id}&page=${tab.id}`});
  await until(() => view.evaluate(`typeof audio !== 'undefined' && !!drv`));
  await view.evaluate(`
    globalThis.audioPeaks = [0, 0]; globalThis.audioStarted = 0;
    const originalStart = AudioBufferSourceNode.prototype.start;
    AudioBufferSourceNode.prototype.start = function(...args) {
      for (let c = 0; c < 2; c++) for (const v of this.buffer?.getChannelData(c) || []) audioPeaks[c] = Math.max(audioPeaks[c], Math.abs(v));
      audioStarted++; return originalStart.apply(this, args);
    };
  `);
  await until(() => view.evaluate(`!!audio.context`));
  await source.call('Runtime.evaluate', {expression: `document.querySelector('audio').play()`, awaitPromise: true, userGesture: true});
  await view.call('Runtime.evaluate', {expression: `audio.context.resume()`, awaitPromise: true, userGesture: true});
  await until(() => view.evaluate(`audioStarted > 3 && audioPeaks.every(v => v > 0.1)`));
  console.log('Stereo media sound captured, relayed, decoded and scheduled in view-only handoff:', await view.evaluate(`({chunks:audioStarted,peaks:audioPeaks,state:audio.context.state})`));
  await message({type:'shareAudio', enabled:false});
  await until(() => view.evaluate(`!audio.context && E.audioBtn.hidden`));
  console.log('Owner disabling sound stops the viewer.');
  pageServer.close();
} finally {
  for (const c of children) c.kill('SIGTERM');
  await sleep(1000);
  await rm(directory, {recursive: true, force: true});
}
process.exit(0);
