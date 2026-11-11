// human_input=1 on /chromium: Playwright's clicks, typing and wheel still land, but
// arrive reshaped (pointer path, one insertion per character, wheel in steps); a
// plain connection is unchanged. Real Chromium and daemon, isolated profile.
// Needs playwright-core: PLAYWRIGHT_DIR is a directory where it is installed
// (npm install --prefix DIR playwright-core); CHROMIUM and XVFB name the binaries.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdtemp, rm, writeFile} from 'node:fs/promises';
import {createServer as tcpServer} from 'node:net';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {randomBytes} from 'node:crypto';
import {createRequire} from 'node:module';

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(fn) {
  const deadline = Date.now() + 45000;
  while (Date.now() < deadline) { try { const value = await fn(); if (value) return value; } catch {} await sleep(100); }
  throw new Error('Human input check timed out');
}
async function freePort() {
  const s = tcpServer(); await new Promise(resolve => s.listen(0, '127.0.0.1', resolve));
  const port = s.address().port; await new Promise(resolve => s.close(resolve)); return port;
}
const {chromium} = createRequire(join(resolve(process.env.PLAYWRIGHT_DIR || '.'), 'noop.js'))('playwright-core');

const PAGE = `<!doctype html><title>t</title><body style="margin:0;height:3000px">
<button id="btn" style="position:absolute;left:500px;top:300px;width:120px;height:40px" onclick="log.clicks++">Go</button>
<input id="field" style="position:absolute;left:100px;top:100px">
<script>
var log = window.log = {moves: 0, clicks: 0, inputs: 0, keys: [], wheels: [], trusted: true};
addEventListener('mousemove', e => { log.moves++; log.trusted = log.trusted && e.isTrusted; });
addEventListener('wheel', e => log.wheels.push(e.deltaY));
var f = document.getElementById('field');
f.addEventListener('input', () => log.inputs++);
f.addEventListener('keydown', () => log.keys.push(performance.now()));
</script>`;

const directory = await mkdtemp(join(tmpdir(), 'browser-node-human-'));
const children = [];
try {
  const daemonPort = await freePort();
  const secret = randomBytes(32).toString('hex');
  const secretFile = join(directory, 'test-secret');
  await writeFile(secretFile, secret, {mode: 0o600});
  const base = `http://127.0.0.1:${daemonPort}`;
  const bin = process.env.BROWSER_DAEMON || resolve('target/debug/browser-daemon');
  children.push(spawn(bin, ['run', '--bind', `127.0.0.1:${daemonPort}`, '--public-url', base, '--profile-dir', join(directory, 'profiles'), '--recordings-dir', join(directory, 'recordings'), '--cluster-secret-file', secretFile, '--record-sessions', 'false', '--display-mode', 'per-session', '--chromium', process.env.CHROMIUM || 'chromium', '--xvfb', process.env.XVFB || 'Xvfb'], {stdio: process.env.SMOKE_DEBUG ? 'inherit' : 'ignore'}));
  await until(async () => (await fetch(`${base}/health`)).ok);

  async function run(query) {
    const browser = await chromium.connectOverCDP(`ws://127.0.0.1:${daemonPort}/chromium${query}`, {headers: {'X-Browser-Cluster-Secret': secret}});
    try {
      const context = browser.contexts()[0] || await browser.newContext();
      const page = await context.newPage();
      await page.goto('data:text/html,' + encodeURIComponent(PAGE));
      const t0 = Date.now();
      await page.click('#btn');
      const clickMs = Date.now() - t0;
      const t1 = Date.now();
      await page.fill('#field', 'hello world');
      const fillMs = Date.now() - t1;
      const filled = await page.inputValue('#field');
      await page.fill('#field', '');
      await page.locator('#field').pressSequentially('abc def');
      const typed = await page.inputValue('#field');
      await page.mouse.wheel(0, 800);
      await sleep(300);
      const log = await page.evaluate('window.log');
      // Still ordered: a command right after the delayed ones sees their effect.
      const after = await page.evaluate('document.getElementById("field").value');
      return {log, filled, typed, after, clickMs, fillMs};
    } finally {
      await browser.close();
    }
  }

  const human = await run('?human_input=1');
  console.log('human', JSON.stringify(human));
  assert.equal(human.log.clicks, 1, 'the click lands once');
  assert.ok(human.log.moves >= 15, `pointer path (${human.log.moves} moves)`);
  assert.ok(human.log.trusted);
  assert.equal(human.filled, 'hello world');
  assert.equal(human.typed, 'abc def');
  assert.equal(human.after, 'abc def');
  assert.ok(human.log.inputs >= 11 + 1, `one insertion per character (${human.log.inputs})`);
  assert.ok(human.fillMs >= 11 * 30 - 30, `typing takes time (${human.fillMs} ms)`);
  assert.ok(human.log.wheels.length >= 2, `wheel in steps (${human.log.wheels.length})`);
  assert.equal(human.log.wheels.reduce((a, b) => a + b, 0), 800);
  const gaps = human.log.keys.slice(1).map((t, i) => t - human.log.keys[i]);
  assert.ok(gaps.every(g => g > 5), 'key presses have gaps');

  const plain = await run('');
  console.log('plain', JSON.stringify(plain));
  assert.equal(plain.log.clicks, 1);
  assert.ok(plain.log.moves <= 3, `no path (${plain.log.moves} moves)`);
  assert.equal(plain.filled, 'hello world');
  assert.equal(plain.typed, 'abc def');
  assert.equal(plain.log.wheels.length, 1);
  assert.ok(plain.fillMs < human.fillMs);
  console.log('human input smoke test passed');
} finally {
  for (const child of children) child.kill('SIGTERM');
  await sleep(500);
  await rm(directory, {recursive: true, force: true});
}
