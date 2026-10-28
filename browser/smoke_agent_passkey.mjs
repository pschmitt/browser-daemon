// Agent passkeys (POST /api/sessions/{id}/agent-passkey): an armed session's
// navigator.credentials.get() is signed by the secret agent and answered in the
// page; a site it was not armed for is left to the browser. Real daemon and
// Chromium (per-session Xvfb); a stand-in secret agent answers with a fixed
// assertion. The node's browsers may not reach loopback, so the page is served
// over HTTPS (a throwaway certificate, ignored by a Chromium wrapper) on this
// host's first LAN address. Needs openssl. Node 22+ provides fetch and
// WebSocket; no npm dependencies.
import assert from 'node:assert/strict';
import {execFileSync, spawn} from 'node:child_process';
import {chmod, mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {createServer as httpsServer} from 'node:https';
import {createServer as tcpServer} from 'node:net';
import {networkInterfaces, tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {randomBytes} from 'node:crypto';

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(fn) {
  const deadline = Date.now() + 45000;
  while (Date.now() < deadline) { try { const value = await fn(); if (value) return value; } catch {} await sleep(200); }
  throw new Error('Agent passkey check timed out');
}
async function freePort() {
  const s = tcpServer(); await new Promise(resolve => s.listen(0, '127.0.0.1', resolve));
  const port = s.address().port; await new Promise(resolve => s.close(resolve)); return port;
}
async function evaluate(wsUrl, expression) {
  const socket = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, {once: true}); socket.addEventListener('error', reject, {once: true}); });
  try {
    const answer = new Promise(resolve => socket.addEventListener('message', e => { const m = JSON.parse(e.data); if (m.id === 1) resolve(m); }));
    socket.send(JSON.stringify({id: 1, method: 'Runtime.evaluate', params: {expression, awaitPromise: true, returnByValue: true}}));
    const m = await answer;
    if (m.error || m.result.exceptionDetails) throw new Error(JSON.stringify(m.error || m.result.exceptionDetails));
    return m.result.result.value;
  } finally { socket.close(); }
}

const ASSERTION = {
  id: 'Y3JlZA', rawId: 'Y3JlZA', type: 'public-key', authenticatorAttachment: 'platform',
  response: {clientDataJSON: 'e30', authenticatorData: 'AAAA', signature: 'c2ln', userHandle: 'dXNlcg'},
};

const directory = await mkdtemp(join(tmpdir(), 'browser-agent-passkey-'));
const children = [];
let stopAgent = false;
try {
  const daemonPort = await freePort();
  const secret = randomBytes(32).toString('hex');
  const secretFile = join(directory, 'test-secret');
  await writeFile(secretFile, secret, {mode: 0o600});
  const base = `http://127.0.0.1:${daemonPort}`;
  const lan = process.env.SMOKE_HOST || Object.values(networkInterfaces()).flat().find(i => i.family === 'IPv4' && !i.internal)?.address;
  if (!lan) throw new Error('No LAN address (set SMOKE_HOST)');
  execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-days', '1', '-subj', '/CN=smoke', '-keyout', join(directory, 'key.pem'), '-out', join(directory, 'cert.pem')], {stdio: 'ignore'});
  const chromium = join(directory, 'chromium');
  await writeFile(chromium, `#!/bin/sh\nexec ${JSON.stringify(process.env.CHROMIUM || 'chromium')} --ignore-certificate-errors "$@"\n`);
  await chmod(chromium, 0o755);
  const headers = {'X-Browser-Cluster-Secret': secret, 'Content-Type': 'application/json'};
  children.push(spawn(resolve('target/debug/browser-daemon'), ['run', '--bind', `127.0.0.1:${daemonPort}`, '--public-url', base, '--profile-dir', join(directory, 'profiles'), '--recordings-dir', join(directory, 'recordings'), '--cluster-secret-file', secretFile, '--record-sessions', 'false', '--display-mode', 'per-session', '--chromium', chromium, '--xvfb', process.env.XVFB || 'Xvfb'], {stdio: process.env.SMOKE_DEBUG ? 'inherit' : 'ignore'}));
  await until(async () => (await fetch(`${base}/health`)).ok);

  // The stand-in secret agent: answers every "assert" with ASSERTION.
  const asked = [];
  (async () => {
    while (!stopAgent) {
      const res = await fetch(`${base}/api/secret-agent/next`, {headers}).catch(() => null);
      if (!res || res.status !== 200) { await sleep(200); continue; }
      const req = await res.json();
      asked.push(req);
      await fetch(`${base}/api/secret-agent/${req.id}`, {method: 'POST', headers, body: JSON.stringify(req.kind === 'assert' ? {ok: true, info: ASSERTION} : {ok: false, error: 'not here'})});
    }
  })();
  await until(async () => (await fetch(`${base}/api/sessions/00000000-0000-0000-0000-000000000000/agent-passkey`, {method: 'POST', headers, body: JSON.stringify({sites: ['smoke.test']})})).status === 404);

  const pages = httpsServer({key: await readFile(join(directory, 'key.pem')), cert: await readFile(join(directory, 'cert.pem'))}, (req, res) => { res.writeHead(200, {'Content-Type': 'text/html'}); res.end('<!doctype html><title>Sign in</title><h1>Sign in</h1>'); });
  await new Promise(r => pages.listen(0, lan, r));
  const pageURL = `https://${lan}:${pages.address().port}/login`;
  const created = await (await fetch(`${base}/json/new?${encodeURIComponent(pageURL)}`, {method: 'PUT', headers})).json();
  const page = await until(async () => (await (await fetch(`${base}/json/list`, {headers})).json()).find(t => t.id === created.id && t.url === pageURL && t.sessionId));
  const wsUrl = `ws://127.0.0.1:${daemonPort}/devtools/page/${page.id}?auth=${secret}`;
  // The new tab may still be on about:blank.
  await until(async () => (await evaluate(wsUrl, `location.href === ${JSON.stringify(pageURL)} || (location.href = ${JSON.stringify(pageURL)}, false)`)) && evaluate(wsUrl, 'document.readyState === "complete" && isSecureContext'));
  const signIn = `navigator.credentials.get({publicKey: {challenge: new Uint8Array([1, 2, 3, 4]), rpId: location.hostname, timeout: 2000}}).then(c => c.id + ':' + c.response.signature.byteLength, e => 'native:' + e.name)`;

  // Not armed for this site: the browser's own get() answers (no authenticator).
  const off = await fetch(`${base}/api/sessions/${page.sessionId}/agent-passkey`, {method: 'POST', headers, body: JSON.stringify({sites: ['example.com']})});
  assert.equal(off.status, 200, await off.text());
  await sleep(1000);
  assert.match(await evaluate(wsUrl, signIn), /^native:/);
  assert.equal(asked.length, 0, 'nothing reaches the vault for another site');

  // Armed for it: signed by the agent, answered as a PublicKeyCredential.
  const on = await (await fetch(`${base}/api/sessions/${page.sessionId}/agent-passkey`, {method: 'POST', headers, body: JSON.stringify({sites: [lan], seconds: 60})})).json();
  assert.deepEqual(on, {sites: [lan], seconds: 60});
  await sleep(500);
  assert.equal(await evaluate(wsUrl, signIn), 'Y3JlZA:3');
  assert.equal(asked.length, 1);
  assert.equal(asked[0].kind, 'assert');
  assert.equal(asked[0].rp_id, lan);
  assert.equal(asked[0].origin, new URL(pageURL).origin);
  assert.equal(asked[0].challenge, 'AQIDBA');

  // A page loaded later is hooked too.
  await evaluate(wsUrl, `location.href = ${JSON.stringify(pageURL + '?next')}; true`);
  await sleep(1500);
  assert.equal(await evaluate(wsUrl, signIn), 'Y3JlZA:3');
  assert.equal(asked.length, 2);

  // Disarmed: the browser again.
  await fetch(`${base}/api/sessions/${page.sessionId}/agent-passkey`, {method: 'POST', headers, body: JSON.stringify({sites: []})});
  assert.match(await evaluate(wsUrl, signIn), /^native:/);
  assert.equal(asked.length, 2);
  console.log('agent passkey smoke test passed');
  pages.close();
} finally {
  stopAgent = true;
  for (const child of children) child.kill('SIGTERM');
  await sleep(500);
  await rm(directory, {recursive: true, force: true});
}
