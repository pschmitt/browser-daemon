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

// Manual sharing unless a test says otherwise: most check per-tab grants.
function harness(firefox = false, stored = {shareAll: false}) {
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
    runtime: {id: 'test', getURL: s => `chrome-extension://test/${s}`, getManifest: () => ({version: '0.3.1'}), onMessage: event('message')},
    tabs: {get: async id => { if (!tabs.has(id)) throw Error('No tab'); return tabs.get(id); }, query: async () => [...tabs.values()], onRemoved: event('removed'), onUpdated: event('updated'), onCreated: event('created'), onActivated: event('activated'),
      update: async (...args) => { calls.push(['update', ...args]); }, remove: async id => tabs.delete(id),
      captureTab: async () => { calls.push(['capture']); return 'data:image/jpeg;base64,/9jframe'; },
      create: async props => { const id = 100 + tabs.size; tabs.set(id, {id, title: '', url: props.url, incognito: false, windowId: 1}); calls.push(['create', props]); return tabs.get(id); }},
    debugger: {attach: async target => { calls.push(['attach', target]); }, detach: async target => { calls.push(['detach', target]); },
      sendCommand: async (...args) => { calls.push(['cdp', ...args]); return {data: 'image'}; }, onDetach: event('detach'), onEvent: event('cdpEvent')},
    alarms: {clear: async () => false, create: async () => {}, onAlarm: event('alarm')},
    action: {setBadgeText: async () => {}, setBadgeBackgroundColor: async () => {}, setTitle: async () => {}},
    storage: {local: {set: async data => { calls.push(['store', data]); }, get: async () => ({managedAllowlistSeen: true, ...stored}), remove: async () => {}},
      session: {get: async () => ({}), set: async () => {}, remove: async () => {}},
      managed: {get: async () => ({allowedDaemonOrigins: ['https://daemon.example']})}, onChanged: event('storage')},
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
  const stored = h.calls.filter(c => c[0] === 'store' && 'server' in c[1]);
  // (The pairing code never reaches storage; checked below.)
  assert.equal(stored.length, 1);
  assert.deepEqual(Object.keys(stored[0][1]).sort(), ['name', 'server']);
  assert.ok(stored.every(([, data]) => !JSON.stringify(data).includes('aaaa')));
  assert.ok(!h.sockets[0].url.href?.includes('aaaa'));
  assert.equal(h.sockets[0].sent[0].version, '0.3.1');
});
test('unshared and view-only tabs reject control locally even if the daemon asks', async () => {
  const h = harness(); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: false});
  for (const tab of [1, 4]) s.receive({type: 'command', id: `request-${tab}`, tab, action: 'type', params: {text: 'no'}});
  await h.flush();
  // Sharing attaches the debugger for page events; nothing else may reach the tab.
  assert.equal(h.calls.filter(c => c[0] === 'script' || c[0] === 'cdp' && c[2] !== 'Page.enable').length, 0);
  assert.equal(s.sent.filter(c => c.type === 'result' && c.error).length, 2);
  s.receive({type: 'command', id: 'screen', tab: 1, action: 'screenshot', params: {method: 'Runtime.evaluate'}});
  await h.flush();
  assert.ok(h.calls.some(c => c[0] === 'cdp' && c[2] === 'Page.captureScreenshot'));
  assert.ok(!h.calls.some(c => c[2] === 'Runtime.evaluate'));
});
test('sharing follows the tab across sites and pauses on daemon pages', async () => {
  const h = harness(); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  const go = url => { h.tabs.get(1).url = url; h.listeners.updated(1, {url}, h.tabs.get(1)); };
  go('https://elsewhere.example/'); await h.flush();
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 1, method: 'Runtime.evaluate', params: {expression: '1'}});
  await h.flush();
  assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 1).result);
  go('https://daemon.example/'); await h.flush();
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 2, method: 'Page.captureScreenshot', params: {}});
  await h.flush();
  assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 2).error);
  h.listeners.cdpEvent({tabId: 1}, 'Page.screencastFrame', {data: 'dashboard'});
  assert.ok(!s.sent.some(c => c.type === 'cdp_event'));
  // Page events reveal the navigation before any frame of the new page.
  h.listeners.cdpEvent({tabId: 1}, 'Page.frameNavigated', {frame: {id: 'main', url: 'https://one.example/'}});
  h.listeners.cdpEvent({tabId: 1}, 'Page.screencastFrame', {data: 'page'});
  assert.equal(s.sent.filter(c => c.type === 'cdp_event' && c.method === 'Page.screencastFrame').length, 1);
});
test('the live view relay enforces view-only grants locally', async () => {
  const h = harness(); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: false});
  for (const [id, method, params] of [[1, 'Input.dispatchMouseEvent', {type: 'mousePressed', x: 1, y: 1}], [2, 'Page.navigate', {url: 'https://x.example/'}], [3, 'Target.createTarget', {}], [4, 'Page.startScreencast', {}]]) {
    s.receive({type: 'cdp', channel: 'view', tab: id === 4 ? 1 : 1, id, method, params});
  }
  s.receive({type: 'cdp', channel: 'view', tab: 4, id: 5, method: 'Page.startScreencast', params: {}});
  await h.flush();
  const result = id => s.sent.find(c => c.type === 'cdp_result' && c.id === id);
  for (const id of [1, 2, 3, 5]) assert.ok(result(id).error, `request ${id} must fail`);
  assert.ok(result(4).result);
  assert.ok(!h.calls.some(c => c[0] === 'cdp' && ['Input.dispatchMouseEvent', 'Page.navigate', 'Target.createTarget'].includes(c[2])));
});
test('debugger Cancel and Stop revoke locally; other detaches do not', async () => {
  const h = harness(); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  h.listeners.detach({tabId: 1}, 'target_closed'); await h.flush();
  assert.equal((await h.message({type: 'state'})).tabs[0].shared, true);
  h.listeners.detach({tabId: 1}, 'canceled_by_user'); await h.flush();
  assert.equal((await h.message({type: 'state'})).tabs[0].shared, false);
  await h.message({type: 'share', tabs: [1], control: true});
  await h.message({type: 'stop'});
  assert.equal((await h.message({type: 'state'})).registration, null);
  assert.equal(s.readyState, 3);
});
test('new tabs join sharing while connected, with the last control choice', async () => {
  const h = harness(); await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  h.tabs.set(5, {id: 5, title: 'New', url: 'https://five.example/', incognito: false});
  h.listeners.created(h.tabs.get(5));
  h.tabs.set(6, {id: 6, title: 'Private', url: 'https://six.example/', incognito: true});
  h.listeners.created(h.tabs.get(6));
  const tabs = (await h.message({type: 'state'})).tabs;
  const added = tabs.find(t => t.id === 5);
  assert.equal(added.shared, true);
  assert.equal(added.control, true);
  assert.ok(!tabs.some(t => t.id === 6));
  await h.message({type: 'autoShare', enabled: false});
  h.tabs.set(7, {id: 7, title: 'Later', url: 'https://seven.example/', incognito: false});
  h.listeners.created(h.tabs.get(7));
  assert.equal((await h.message({type: 'state'})).tabs.find(t => t.id === 7).shared, false);
});
test('by default every shareable tab is shared once connected; control applies to all', async () => {
  const h = harness(false, {}); const s = await h.pair();
  await h.flush();
  const state = await h.message({type: 'state'});
  assert.equal(state.sharing.all, true);
  assert.deepEqual(state.tabs.filter(t => t.shared).map(t => t.id), [1, 4]);
  await h.message({type: 'setControl', control: true});
  assert.ok((await h.message({type: 'state'})).tabs.every(t => t.control));
  assert.equal(s.sent.filter(m => m.type === 'tabs').at(-1).can_open, true);
  // The first tab can be opened by a controller: tab 0, web pages only.
  s.receive({type: 'command', id: 'open', tab: 0, action: 'open', params: {url: 'https://new.example/'}});
  s.receive({type: 'command', id: 'file', tab: 0, action: 'open', params: {url: 'file:///etc/passwd'}});
  await h.flush();
  assert.ok(s.sent.find(m => m.id === 'open').result.tab);
  assert.ok(s.sent.find(m => m.id === 'file').error);
  // Off: no tab-less opening without control.
  await h.message({type: 'setControl', control: false});
  s.receive({type: 'command', id: 'denied', tab: 0, action: 'open', params: {url: 'https://x.example/'}});
  await h.flush();
  assert.ok(s.sent.find(m => m.id === 'denied').error);
});
test('a stale popup cannot share a replacement URL under its previous label', async () => {
  const h = harness(); await h.pair();
  const response = await h.message({type: 'share', tabs: [1], urls: {1: 'https://previous.example/'}, control: true});
  assert.ok(response.error);
  assert.equal((await h.message({type: 'state'})).tabs[0].shared, false);
});
test('Firefox emulates the live view: pushed frames, flow control and input', async () => {
  const h = harness(true); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 1, method: 'Page.startScreencast', params: {}});
  await h.flush(); await h.flush();
  const frame = s.sent.find(c => c.type === 'cdp_event' && c.method === 'Page.screencastFrame');
  assert.equal(frame.channel, 'view');
  assert.equal(frame.params.data, '/9jframe');
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 2, method: 'Input.insertText', params: {text: 'hi'}});
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 3, method: 'Target.createTarget', params: {}});
  await h.flush();
  assert.ok(h.calls.some(c => c[0] === 'script' && c[1].args[0] === 'type' && c[1].args[1].text === 'hi'));
  assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 3).error);
  s.receive({type: 'cdp_close', channel: 'view'});
  const captures = h.calls.filter(c => c[0] === 'capture').length;
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 4, method: 'Page.screencastFrameAck', params: {sessionId: 1}});
  await h.flush();
  assert.equal(h.calls.filter(c => c[0] === 'capture').length, captures);
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
