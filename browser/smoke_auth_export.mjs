// A page's login for an agent's tools (POST /api/pages/{id}/auth): the cookies the
// browser sends to the page (HttpOnly too) and to extra addresses, and the page's
// storage. Real daemon and browsers (Chromium; Firefox too with FIREFOX set),
// per-session Xvfb. The node's browsers may not reach loopback, so the page is
// served over HTTPS (a throwaway certificate, ignored by browser wrappers) on
// this host's first LAN address. Needs openssl. Node 22+, no npm dependencies.
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
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) { try { const value = await fn(); if (value) return value; } catch {} await sleep(250); }
  throw new Error('Auth export check timed out');
}
async function freePort() {
  const s = tcpServer(); await new Promise(resolve => s.listen(0, '127.0.0.1', resolve));
  const port = s.address().port; await new Promise(resolve => s.close(resolve)); return port;
}

const directory = await mkdtemp(join(tmpdir(), 'browser-auth-export-'));
const children = [];
try {
  const daemonPort = await freePort();
  const secret = randomBytes(32).toString('hex');
  const secretFile = join(directory, 'test-secret');
  await writeFile(secretFile, secret, {mode: 0o600});
  const base = `http://127.0.0.1:${daemonPort}`;
  const headers = {'X-Browser-Cluster-Secret': secret, 'Content-Type': 'application/json'};
  const lan = process.env.SMOKE_HOST || Object.values(networkInterfaces()).flat().find(i => i.family === 'IPv4' && !i.internal)?.address;
  if (!lan) throw new Error('No LAN address (set SMOKE_HOST)');
  execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-days', '1', '-subj', '/CN=smoke', '-keyout', join(directory, 'key.pem'), '-out', join(directory, 'cert.pem')], {stdio: 'ignore'});
  const wrap = async (name, program, flags) => {
    const path = join(directory, name);
    await writeFile(path, `#!/bin/sh\nexec ${JSON.stringify(program)} ${flags} "$@"\n`);
    await chmod(path, 0o755);
    return path;
  };
  const chromium = await wrap('chromium', process.env.CHROMIUM || 'chromium', '--ignore-certificate-errors');
  const args = ['run', '--bind', `127.0.0.1:${daemonPort}`, '--public-url', base, '--profile-dir', join(directory, 'profiles'), '--recordings-dir', join(directory, 'recordings'), '--cluster-secret-file', secretFile, '--record-sessions', 'false', '--display-mode', 'per-session', '--chromium', chromium, '--xvfb', process.env.XVFB || 'Xvfb'];
  if (process.env.FIREFOX) args.push('--firefox', process.env.FIREFOX);
  children.push(spawn(resolve('target/debug/browser-daemon'), args, {stdio: process.env.SMOKE_DEBUG ? 'inherit' : 'ignore'}));
  await until(async () => (await fetch(`${base}/health`)).ok);

  // The page sets a HttpOnly and a path-limited cookie and fills both storages.
  const pages = httpsServer({key: await readFile(join(directory, 'key.pem')), cert: await readFile(join(directory, 'cert.pem'))}, (req, res) => {
    res.writeHead(200, {'Content-Type': 'text/html', 'Set-Cookie': ['sid=s3cr3t-session; Path=/; Secure; HttpOnly; SameSite=Lax', 'api=api-only-value; Path=/api; Secure']});
    res.end('<!doctype html><title>App</title><script>localStorage.setItem("token", JSON.stringify({secret: "eyJ.local.token"})); sessionStorage.setItem("tab", "one");</script><h1>App</h1>');
  });
  await new Promise(r => pages.listen(0, lan, r));
  const pageURL = `https://${lan}:${pages.address().port}/app`;

  const check = async (engine, page) => {
    const auth = (body) => fetch(`${base}/api/pages/${encodeURIComponent(page.id)}/auth`, {method: 'POST', headers, body: JSON.stringify({session_id: page.sessionId, ...body})});
    // Cookies only, for the page: the HttpOnly one, not the /api one.
    const plain = await (await auth({})).json();
    assert.equal(plain.url, pageURL, engine);
    assert.deepEqual(plain.cookies.map(c => c.name).sort(), ['sid'], engine);
    assert.equal(plain.cookies[0].value, 's3cr3t-session', engine);
    assert.equal(plain.cookies[0].http_only, true, engine);
    assert.deepEqual(plain.local_storage, {}, `${engine}: no storage unless asked`);
    // More addresses and the storage.
    const full = await (await auth({urls: [`https://${lan}:${pages.address().port}/api/me`], storage: true})).json();
    assert.deepEqual(full.cookies.map(c => c.name).sort(), ['api', 'sid'], engine);
    assert.equal(JSON.parse(full.local_storage.token).secret, 'eyJ.local.token', engine);
    assert.equal(full.session_storage.tab, 'one', engine);
    // Without the cluster secret: refused. Other schemes: refused.
    assert.equal((await fetch(`${base}/api/pages/${encodeURIComponent(page.id)}/auth`, {method: 'POST', headers: {'Content-Type': 'application/json'}, body: '{}'})).status, 401, engine);
    assert.equal((await auth({urls: ['file:///etc/passwd']})).status, 400, engine);
    console.log(`${engine}: ok`);
  };

  // Chromium.
  const created = await (await fetch(`${base}/json/new?${encodeURIComponent(pageURL)}`, {method: 'PUT', headers})).json();
  const page = await until(async () => (await (await fetch(`${base}/json/list`, {headers})).json()).find(t => t.id === created.id && t.url === pageURL && t.sessionId));
  await sleep(1500);
  await check('chromium', page);

  // Firefox (BiDi), when there is one.
  if (process.env.FIREFOX) {
    const session = await (await fetch(`${base}/api/sessions`, {method: 'POST', headers, body: JSON.stringify({engine: 'firefox', persistent: false})})).json();
    const sid = session.id || session.session?.id;
    assert.ok(sid, JSON.stringify(session));
    await until(async () => (await (await fetch(`${base}/json/list`, {headers})).json()).some(t => t.sessionId === sid));
    const blank = (await (await fetch(`${base}/json/list`, {headers})).json()).find(t => t.sessionId === sid);
    await fetch(`${base}/json/close/${encodeURIComponent(blank.id)}`, {method: 'PUT', headers}).catch(() => {});
    const opened = await (await fetch(`${base}/json/new?url=${encodeURIComponent(pageURL)}&session_id=${sid}`, {method: 'PUT', headers})).json();
    const ffPage = await until(async () => (await (await fetch(`${base}/json/list`, {headers})).json()).find(t => t.sessionId === sid && t.url === pageURL));
    assert.ok(opened);
    await sleep(2000);
    await check('firefox', ffPage);
  }

  // The shared-browser command route does not take the internal action.
  const fake = '00000000-0000-0000-0000-000000000000';
  const viaCommand = await fetch(`${base}/api/extension/nodes/${fake}/command`, {method: 'POST', headers, body: JSON.stringify({tab: 1, action: 'authExport', params: {}})});
  assert.notEqual(viaCommand.status, 200);
  console.log('auth export smoke test passed');
  pages.close();
} finally {
  for (const child of children) child.kill('SIGTERM');
  await sleep(500);
  await rm(directory, {recursive: true, force: true});
}
