// Real policy boundaries plus background-worker behavior, using a mocked browser
// transport. Run with node --test browser/check.mjs; no npm dependencies.
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {test} from 'node:test';

const source = name => readFileSync(new URL(`extension/${name}`, import.meta.url), 'utf8');
const policy = vm.createContext({URL});
vm.runInContext(source('policy.js'), policy);
const {NodePolicy: p} = policy;

test('transport accepts TLS or loopback only, without URL credentials', () => {
  for (const u of ['https://browser.example.org', 'http://127.0.0.1:3001', 'http://[::1]:3001']) assert.equal(p.serverURL(u), u);
  for (const u of ['http://browser.example.org', 'ws://localhost', 'https://u:p@example.org', 'https://example.org/path', 'https://example.org/?auth=code', 'https://example.org/#code']) assert.throws(() => p.serverURL(u));
});
test('read grants never accept input, evaluation, navigation or protocol escapes', () => {
  for (const action of ['screenshot', 'snapshot']) assert.ok(p.allowed({tab: 1, action, params: {}}, false));
  for (const action of ['type', 'click', 'navigate', 'focus', 'close', 'cdp']) assert.equal(p.allowed({tab: 1, action, params: {method: 'Runtime.evaluate', url: 'https://example.org'}}, false), false);
  for (const method of ['Browser.close', 'Target.attachToTarget', 'Target.createTarget', 'Storage.getCookies', 'Network.getAllCookies', 'DOM.setFileInputFiles', 'Page.navigate', 'Page.addScriptToEvaluateOnNewDocument']) assert.equal(p.allowed({tab: 1, action: 'cdp', params: {method}}, true), false);
  assert.ok(p.allowed({tab: 1, action: 'cdp', params: {method: 'Runtime.evaluate'}}, true));
  for (const url of ['file:///etc/passwd', 'javascript:alert(1)', 'chrome://settings', 'https://u:p@example.org']) assert.equal(p.allowed({tab: 1, action: 'navigate', params: {url}}, true), false);
});
test('malformed and oversized commands are rejected', () => {
  for (const command of [null, {}, {tab: '1', action: 'screenshot', params: {}}, {tab: 1, action: 'snapshot', params: []}, {tab: 1, action: 'type', params: {text: 'x'.repeat(140000)}}]) assert.equal(p.allowed(command, true), false);
});

function harness(firefox = false) {
  const listeners = {};
  const calls = [];
  const sockets = [];
  const event = name => ({addListener: fn => { listeners[name] = fn; }});
  const tabs = new Map([
    [1, {id: 1, title: 'Shared', url: 'https://one.example/', incognito: false}],
    [2, {id: 2, title: 'Private', url: 'https://two.example/', incognito: true}],
    [3, {id: 3, title: 'Daemon', url: 'https://daemon.example/', incognito: false}],
    [4, {id: 4, title: 'Other', url: 'https://four.example/', incognito: false}],
  ]);
  class Socket {
    static OPEN = 1;
    constructor(url) { this.url = url; this.readyState = 1; this.sent = []; sockets.push(this); }
    send(data) { this.sent.push(JSON.parse(data)); }
    close() { this.readyState = 3; }
    receive(message) { this.onmessage({data: JSON.stringify(message)}); }
  }
  const api = {
    runtime: {id: 'test', getURL: s => `chrome-extension://test/${s}`, onMessage: event('message')},
    tabs: {get: async id => { if (!tabs.has(id)) throw Error('No tab'); return tabs.get(id); }, query: async () => [...tabs.values()], onRemoved: event('removed'), onUpdated: event('updated'),
      update: async (...args) => { calls.push(['update', ...args]); }, remove: async id => tabs.delete(id)},
    debugger: {attach: async target => { calls.push(['attach', target]); }, detach: async target => { calls.push(['detach', target]); },
      sendCommand: async (...args) => { calls.push(['cdp', ...args]); return {data: 'image'}; }, onDetach: event('detach')},
    alarms: {clear: async () => false, create: async () => {}, onAlarm: event('alarm')},
    action: {setBadgeText: async () => {}, setBadgeBackgroundColor: async () => {}, setTitle: async () => {}},
    storage: {local: {set: async data => { calls.push(['store', data]); }, get: async () => ({}), remove: async () => {}},
      session: {get: async () => ({}), set: async () => {}, remove: async () => {}}},
    scripting: {executeScript: async args => { calls.push(['script', args]); return [{result: 'snapshot'}]; }},
    permissions: {contains: async () => true, onAdded: event('permissionAdded'), onRemoved: event('permission')}, commands: {onCommand: event('shortcut')},
  };
  if (firefox) api.runtime.getBrowserInfo = async () => ({name: 'Firefox'});
  const context = vm.createContext({URL, console, chrome: api, WebSocket: Socket,
    setTimeout: () => 1, clearTimeout() {}, setInterval: () => 1, clearInterval() {}});
  vm.runInContext(source('policy.js'), context);
  vm.runInContext(source('background.js'), context);
  const ui = {id: 'test', url: 'chrome-extension://test/popup.html'};
  async function message(value, sender = ui) {
    if (value.type === 'share' && !value.urls) value.urls = Object.fromEntries(value.tabs.map(id => [id, tabs.get(id)?.url]));
    return new Promise(resolve => listeners.message(value, sender, resolve));
  }
  async function pair() {
    await message({type: 'connect', server: 'https://daemon.example', code: 'a'.repeat(64), name: 'Test browser'});
    const socket = sockets.at(-1); socket.onopen();
    socket.receive({type: 'registered', id: 'node', expires_at: Math.floor(Date.now() / 1000) + 600});
    return socket;
  }
  const flush = () => new Promise(resolve => setImmediate(resolve));
  return {listeners, calls, sockets, tabs, message, pair, flush};
}

test('only extension UI may grant tabs; private and daemon tabs are excluded', async () => {
  const h = harness(); await h.pair();
  const evil = await h.message({type: 'share', tabs: [1], control: true}, {id: 'test', url: 'https://one.example/', tab: {id: 1}});
  assert.ok(evil.error);
  for (const tab of [2, 3]) assert.ok((await h.message({type: 'share', tabs: [tab], control: true})).error);
  const state = await h.message({type: 'state'});
  assert.deepEqual(Array.from(state.tabs, t => t.id), [1, 4]);
  const stored = h.calls.filter(c => c[0] === 'store');
  assert.equal(stored.length, 1);
  assert.deepEqual(Object.keys(stored[0][1]).sort(), ['name', 'server']);
  assert.ok(!h.sockets[0].url.href?.includes('aaaa'));
});
test('unshared and view-only tabs reject control locally even if the daemon asks', async () => {
  const h = harness(); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: false});
  for (const tab of [1, 4]) s.receive({type: 'command', id: `request-${tab}`, tab, action: 'type', params: {text: 'no'}});
  await h.flush();
  assert.equal(h.calls.filter(c => ['attach', 'cdp', 'script'].includes(c[0])).length, 0);
  assert.equal(s.sent.filter(c => c.type === 'result' && c.error).length, 2);
  s.receive({type: 'command', id: 'screen', tab: 1, action: 'screenshot', params: {method: 'Runtime.evaluate'}});
  await h.flush();
  assert.ok(h.calls.some(c => c[0] === 'cdp' && c[2] === 'Page.captureScreenshot'));
  assert.ok(!h.calls.some(c => c[2] === 'Runtime.evaluate'));
});
test('navigation, debugger Cancel and Stop revoke locally without waiting for the daemon', async () => {
  const h = harness(); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  h.listeners.updated(1, {status: 'loading'}); await h.flush();
  s.receive({type: 'command', id: 'after-nav', tab: 1, action: 'cdp', params: {method: 'Runtime.evaluate'}});
  await h.flush(); assert.ok(s.sent.find(c => c.id === 'after-nav').error);
  await h.message({type: 'share', tabs: [1], control: true});
  h.listeners.detach({tabId: 1}); await h.flush();
  assert.equal((await h.message({type: 'state'})).tabs[0].shared, false);
  await h.message({type: 'share', tabs: [1], control: true});
  await h.message({type: 'stop'});
  assert.equal((await h.message({type: 'state'})).registration, null);
  assert.equal(s.readyState, 3);
});
test('a stale popup cannot share a replacement URL under its previous label', async () => {
  const h = harness(); await h.pair();
  const response = await h.message({type: 'share', tabs: [1], urls: {1: 'https://previous.example/'}, control: true});
  assert.ok(response.error);
  assert.equal((await h.message({type: 'state'})).tabs[0].shared, false);
});
test('Firefox detection does not depend on optional capture permissions', async () => {
  const h = harness(true);
  assert.equal((await h.message({type: 'state'})).chromium, false);
});
test('extension and daemon CDP allowlists agree', () => {
  const rust = readFileSync(new URL('../src/extension.rs', import.meta.url), 'utf8');
  const rustMethods = new Set([...rust.slice(rust.indexOf('pub fn cdp_allowed'), rust.indexOf('fn web_url')).matchAll(/"([A-Z][A-Za-z]+\.[A-Za-z]+)"/g)].map(m => m[1]));
  const jsMethods = new Set([...source('policy.js').matchAll(/'([A-Z][A-Za-z]+\.[A-Za-z]+)'/g)].map(m => m[1]));
  assert.deepEqual(jsMethods, rustMethods);
});
