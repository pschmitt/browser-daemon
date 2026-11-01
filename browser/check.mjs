// Real policy boundaries plus background-worker behavior, using a mocked browser
// transport. Run with node --test browser/check.mjs; no npm dependencies.
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {test} from 'node:test';
import {webcrypto} from 'node:crypto';

const source = name => readFileSync(new URL(`extension/${name}`, import.meta.url), 'utf8');

test('the session chip and profile menu follow the selected tab owner', () => {
  const html = readFileSync(new URL('../handoff.html', import.meta.url), 'utf8');
  const start = html.indexOf('function currentTab() {');
  const end = html.indexOf('function showProfMenu(', start);
  assert.ok(start >= 0 && end > start);
  const elements = {profChipOwner: {hidden: true, innerHTML: ''}, profMenu: {hidden: false}, pmContext: {innerHTML: ''}};
  const alpha = {agent_name: 'Alpha', since: 1, from: 'node', conversation: 'a'};
  const beta = {agent_name: 'Beta', since: 2, from: 'node', conversation: 'b'};
  const context = vm.createContext({
    $: id => elements[id], E: {profChip: {title: ''}},
    sess: {profile: 'test', name: 'Demo', clients: [alpha, beta]},
    target: {sessionId: 'session'}, pageId: 'one', sharedBrowserId: null,
    tabsAll: [{id: 'one', sessionId: 'session', owner: alpha}, {id: 'two', sessionId: 'session', owner: beta}, {id: 'three', sessionId: 'session'}],
    agentShort: c => c.agent_name, tabOwnerLabel: c => c.agent_name,
    agentSvg: () => '', esc: value => String(value).replaceAll('<', '&lt;').replaceAll('>', '&gt;'),
  });
  vm.runInContext(html.slice(start, end), context);
  context.renderTabOwner();
  assert.match(elements.profChipOwner.innerHTML, /Alpha/);
  context.pageId = 'two';
  context.renderTabOwner();
  assert.match(elements.profChipOwner.innerHTML, /Beta/);
  assert.match(elements.pmContext.innerHTML, /class="pm-client current"[^>]*>[^<]*<span>Beta · Current tab/);
  context.pageId = 'three';
  context.renderTabOwner();
  assert.match(elements.profChipOwner.innerHTML, /Unowned/);
  assert.doesNotMatch(elements.pmContext.innerHTML, /class="pm-client current"/);
  context.target.sessionId = 'other';
  context.pageId = 'two';
  context.renderTabOwner();
  assert.match(elements.profChipOwner.innerHTML, /Checking/);
  assert.doesNotMatch(elements.profChipOwner.innerHTML, /Beta/);
  context.sharedBrowserId = 'shared';
  context.renderTabOwner();
  assert.equal(elements.profChipOwner.hidden, true);
});

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
    tabs: {get: async id => { if (!tabs.has(id)) throw Error('No tab'); return tabs.get(id); }, query: async () => [...tabs.values()], onRemoved: event('removed'), onUpdated: event('updated'), onCreated: event('created'), onActivated: event('activated'), onMoved: event('moved'),
      update: async (...args) => { calls.push(['update', ...args]); }, remove: async id => tabs.delete(id),
      captureTab: async () => { calls.push(['capture']); return 'data:image/jpeg;base64,/9jframe'; },
      create: async props => { const id = 100 + tabs.size; tabs.set(id, {id, title: '', url: props.url, incognito: false, windowId: 1}); calls.push(['create', props]); return tabs.get(id); }},
    debugger: {attach: async target => { calls.push(['attach', target]); }, detach: async target => { calls.push(['detach', target]); },
      sendCommand: async (...args) => { calls.push(['cdp', ...args]); return {data: 'image'}; }, onDetach: event('detach'), onEvent: event('cdpEvent')},
    alarms: {clear: async () => false, create: async () => {}, onAlarm: event('alarm')},
    webNavigation: {onBeforeNavigate: event('beforeNavigate'), onCommitted: event('committed'), onErrorOccurred: event('navigationError')},
    action: {setBadgeText: async () => {}, setBadgeBackgroundColor: async () => {}, setBadgeTextColor: async () => {}, setTitle: async () => {}, setIcon: async () => {}},
    storage: {local: {set: async data => { calls.push(['store', data]); }, get: async () => ({managedAllowlistSeen: true, ...stored}), remove: async () => {}},
      session: {get: async () => ({}), set: async () => {}, remove: async () => {}},
      managed: {get: async () => ({allowedDaemonOrigins: ['https://daemon.example']})}, onChanged: event('storage')},
    scripting: {executeScript: async args => {
      calls.push(['script', args]);
      const url = tabs.get(args.target.tabId)?.url;
      if (args.func.name === 'installDomRoute') return [{frameId: 0, result: {url}}];
      if (args.func.toString().includes('__bdDomRoutes?.get(token)?.route')) {
        listeners.message({type: 'domRoute', token: args.args[0], x: args.args[2], y: args.args[3], url},
          {id: 'test', tab: {id: args.target.tabId}, frameId: 0, url}, () => {});
      }
      return [{frameId: 0, result: 'snapshot'}];
    }},
    permissions: {contains: async () => true, onAdded: event('permissionAdded'), onRemoved: event('permission')}, commands: {onCommand: event('shortcut')},
  };
  const menu = {created: [], updated: []};
  if (firefox) api.menus = {removeAll: async () => { menu.created.length = 0; }, create: o => { menu.created.push(o); }, update: async (id, o) => { menu.updated.push([id, o]); }, refresh: async () => {},
    onShown: event('menuShown'), onClicked: event('menuClicked')};
  if (firefox) api.runtime.getBrowserInfo = async () => ({name: 'Firefox'});
  const context = vm.createContext({URL, console, crypto: webcrypto, chrome: api, WebSocket: Socket,
    setTimeout: () => 1, clearTimeout() {}, setInterval: () => 1, clearInterval() {}});
  vm.runInContext(source('policy.js'), context);
  vm.runInContext(source('proxy.js'), context);
  vm.runInContext(source('files-page.js'), context);
  vm.runInContext(source('files.js'), context);
  vm.runInContext(source('favicon.js'), context);
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
  return {listeners, calls, sockets, tabs, message, pair, flush, api, menu};
}

test('setup tab can manage sharing, but other extension pages cannot', async () => {
  const h = harness();
  const state = await h.message({type: 'state'}, {id: 'test', url: 'chrome-extension://test/popup.html?tab'});
  assert.equal(state.chromium, true);
  for (const url of ['chrome-extension://other/popup.html?tab', 'https://test/popup.html', 'chrome-extension://test/files.html']) {
    assert.ok((await h.message({type: 'state'}, {id: 'test', url})).error);
  }
});
test('loopback spellings cannot be shared', async () => {
  const h = harness(); await h.pair();
  for (const host of ['127.0.0.2', '127.1', '2130706433', 'localhost.', 'sub.localhost', '[::ffff:127.0.0.1]', '[::1]', '0.0.0.0']) {
    h.tabs.set(4, {id: 4, url: `http://${host}/`, incognito: false});
    assert.ok((await h.message({type: 'share', tabs: [4], control: true})).error, host);
  }
});
test('automation admits Playwright setup but rejects persistent code and unknown methods', () => {
  assert.ok(p.automationAllowed('Page.addScriptToEvaluateOnNewDocument', {source: '', worldName: 'utility'}));
  assert.equal(p.automationAllowed('Page.addScriptToEvaluateOnNewDocument', {source: 'steal()', worldName: 'utility'}), false);
  for (const method of ['Page.futureMethod', 'Network.getAllCookies', 'Network.loadNetworkResource', 'DOM.setFileInputFiles']) {
    assert.equal(p.automationAllowed(method, {}), false, method);
  }
  assert.equal(p.automationAllowed('Page.reload', {scriptToEvaluateOnLoad: 'steal()'}), false);
  assert.equal(p.relayAllowed('Page.reload', {scriptToEvaluateOnLoad: 'steal()'}, true), false);
  assert.ok(p.automationAllowed('Runtime.evaluate', {expression: 'document.title'}));
});
test('automation method lists stay identical in Rust and JavaScript', () => {
  const js = source('policy.js').split('const automationMethods = new Set([')[1].split(']);')[0];
  const rust = readFileSync(new URL('../src/extension.rs', import.meta.url), 'utf8').split('pub fn automation_allowed(')[1].split('/// A web page')[0];
  const names = text => [...new Set(text.match(/[A-Z][A-Za-z]+\.[A-Za-z]+/g))].sort();
  assert.deepEqual(names(js), names(rust));
});

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
test('protected navigation detaches automation before commit and safe tabs can reconnect', async () => {
  for (const url of ['https://daemon.example/', 'http://127.0.0.1:3001/']) {
    const h = harness(); const s = await h.pair();
    await h.message({type: 'share', tabs: [1], control: true});
    s.receive({type: 'cdp', automation: true, channel: 'pw', tab: 1, id: 1, method: 'Fetch.enable', params: {}});
    await h.flush();
    assert.ok(h.calls.some(c => c[0] === 'cdp' && c[2] === 'Fetch.enable'));
    h.listeners.beforeNavigate({tabId: 1, frameId: 0, url});
    // No tabs.onUpdated / frameNavigated has happened yet.
    assert.ok(h.calls.some(c => c[0] === 'detach' && c[1].tabId === 1));
    assert.ok(!(await h.message({type: 'state'})).tabs.some(t => t.id === 1));
    s.receive({type: 'cdp', automation: true, channel: 'pw', tab: 1, id: 2, method: 'Runtime.evaluate', params: {expression: 'document.cookie'}});
    await h.flush();
    assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 2).error);
    h.listeners.cdpEvent({tabId: 1}, 'Network.requestWillBeSent', {request: {url}});
    assert.ok(!s.sent.some(c => c.type === 'cdp_event' && c.method === 'Network.requestWillBeSent'));
    h.tabs.get(1).url = 'https://one.example/';
    h.listeners.committed({tabId: 1, frameId: 0});
    h.listeners.updated(1, {url: 'https://one.example/'}, h.tabs.get(1));
    s.receive({type: 'cdp', automation: true, channel: 'new', tab: 1, id: 3, method: 'Runtime.evaluate', params: {expression: '1'}});
    await h.flush();
    assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 3).result);
    assert.equal(h.calls.filter(c => c[0] === 'attach').length, 2);
  }
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
test('a live view follows focus only when the owner enables it', async () => {
  const h = harness(); const s = await h.pair();
  await h.message({type: 'share', tabs: [1], control: false});
  assert.equal((await h.message({type: 'state'})).sharing.follow, false);
  let before = h.calls.filter(c => c[0] === 'update').length;
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 1, method: 'Page.bringToFront', params: {}});
  await h.flush();
  assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 1).result);
  assert.equal(h.calls.filter(c => c[0] === 'update').length, before);
  await h.message({type: 'follow', enabled: true});
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 2, method: 'Page.bringToFront', params: {}});
  await h.flush();
  assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 2).result);
  assert.ok(h.calls.some(c => c[0] === 'update' && c[1] === 1 && c[2].active === true));
  await h.message({type: 'follow', enabled: false});
  before = h.calls.filter(c => c[0] === 'update').length;
  s.receive({type: 'cdp', channel: 'view', tab: 1, id: 3, method: 'Page.bringToFront', params: {}});
  await h.flush();
  assert.ok(s.sent.find(c => c.type === 'cdp_result' && c.id === 3).result);
  assert.equal(h.calls.filter(c => c[0] === 'update').length, before);
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
  assert.ok(h.calls.some(c => c[0] === 'script' && c[1].args?.[0] === 'type' && c[1].args[1].text === 'hi'));
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
test('Firefox hover uses pointer input rather than the tab-move action', async () => {
  const h = harness(true); const socket = await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  socket.receive({type: 'cdp', channel: 'view', tab: 1, id: 1, method: 'Input.dispatchMouseEvent', params: {type: 'mouseMoved', x: 20, y: 30}});
  await h.flush();
  assert.ok(h.calls.some(call => call[0] === 'script' && call[1].args?.[0] === 'pointerMove'));
  assert.ok(!socket.sent.find(message => message.type === 'cdp_result' && message.id === 1)?.error);
});
test('revoking control while Firefox routes input prevents the edit', async () => {
  const h = harness(true); const socket = await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  const executeScript = h.api.scripting.executeScript;
  let release;
  h.api.scripting.executeScript = async args => {
    if (args.func.toString().includes('__bdDomRoutes?.get(token)?.route')) {
      await new Promise(resolve => { release = resolve; });
    }
    return executeScript(args);
  };
  socket.receive({type: 'cdp', channel: 'view', tab: 1, id: 1, method: 'Input.insertText', params: {text: 'forbidden'}});
  await h.flush();
  assert.ok(release);
  await h.message({type: 'setControl', control: false});
  release(); await h.flush();
  assert.ok(socket.sent.find(message => message.type === 'cdp_result' && message.id === 1)?.error);
  assert.ok(!h.calls.some(call => call[0] === 'script' && call[1].args?.[0] === 'type'));
});
test('Firefox viewers share captures, and revocation discards an in-flight frame', async () => {
  const h = harness(true); const socket = await h.pair();
  await h.message({type: 'share', tabs: [1], control: true});
  let finish, count = 0;
  h.api.tabs.captureTab = async () => { count++; return new Promise(resolve => { finish = resolve; }); };
  for (const [id, channel] of [[1, 'first'], [2, 'second']]) {
    socket.receive({type: 'cdp', channel, tab: 1, id, method: 'Page.startScreencast', params: {}});
  }
  await h.flush();
  assert.equal(count, 1);
  finish('data:image/jpeg;base64,/9jfirst'); await h.flush();
  const frames = socket.sent.filter(message => message.method === 'Page.screencastFrame');
  assert.deepEqual(frames.map(message => message.channel).sort(), ['first', 'second']);
  socket.receive({type: 'cdp', channel: 'first', tab: 1, id: 3, method: 'Page.screencastFrameAck', params: {}});
  await h.flush();
  assert.equal(count, 2);
  await h.message({type: 'share', tabs: [], control: false});
  finish('data:image/jpeg;base64,/9jprivate'); await h.flush();
  assert.equal(socket.sent.filter(message => message.method === 'Page.screencastFrame').length, 2);
});
test('Firefox capture failures report status and recover on retry', async () => {
  const h = harness(true); const socket = await h.pair();
  await h.message({type: 'share', tabs: [1], control: false});
  h.api.tabs.captureTab = async () => { throw Error('Permission denied'); };
  socket.receive({type: 'cdp', channel: 'view', tab: 1, id: 1, method: 'Page.startScreencast', params: {}});
  await h.flush();
  assert.equal(socket.sent.find(message => message.method === 'Page.captureStatus').params.ok, false);
  h.api.tabs.captureTab = async () => 'data:image/jpeg;base64,/9jrecovered';
  socket.receive({type: 'cdp', channel: 'view', tab: 1, id: 2, method: 'Page.screencastFrameAck', params: {}});
  await h.flush();
  assert.equal(socket.sent.filter(message => message.method === 'Page.captureStatus').at(-1).params.ok, true);
});
test('extension and daemon CDP allowlists agree', () => {
  const rust = readFileSync(new URL('../src/extension.rs', import.meta.url), 'utf8');
  const rustMethods = new Set([...rust.slice(rust.indexOf('pub fn cdp_allowed'), rust.indexOf('fn web_url')).matchAll(/"([A-Z][A-Za-z]+\.[A-Za-z]+)"/g)].map(m => m[1]));
  const jsMethods = new Set([...source('policy.js').matchAll(/'([A-Z][A-Za-z]+\.[A-Za-z]+)'/g)].map(m => m[1]));
  assert.deepEqual(jsMethods, rustMethods);
});
test('the extension fills logins exactly as the daemon does', () => {
  const rust = readFileSync(new URL('../src/autofill.rs', import.meta.url), 'utf8');
  const raw = name => rust.match(new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`))[1];
  // The extension takes rbw's fill-targets.js before its address check; the daemon at the top.
  const takeTargets = '  /*RBW_FILL_TARGETS*/\n  const rbwTargets = globalThis.rbwFillTargets;\n  delete globalThis.rbwFillTargets;\n';
  const body = raw('FILL').replace(takeTargets, '').replace('/*GUARD*/', raw('GUARD')).replaceAll('return JSON.stringify({ filled });', 'return { filled };').replace('(c) => {\n', '');
  assert.ok(source('background.js').includes(body), 'fillLogin in background.js drifted from FILL in src/autofill.rs');
});
test('the extension and live views share the passkey request and answer flow', () => {
  const extension = source('background.js');
  const liveView = readFileSync(new URL('../handoff.html', import.meta.url), 'utf8');
  for (const marker of ['__bdPasskeyRepair', '__bdPasskeyAnswer', 'allowCredentials', 'AbortError']) {
    assert.ok(extension.includes(marker) && liveView.includes(marker), `passkey flow is missing ${marker}`);
  }
});

test('passkey repair wraps later providers and falls back without asking twice', async () => {
  const declaration = source('background.js').match(/function passkeyHook\(\) \{[\s\S]*?\n\}/)[0];
  let nativeCalls = 0, providerCalls = 0, offers = 0;
  const credentials = {get: async () => { nativeCalls++; return 'native'; }};
  const context = vm.createContext({navigator: {credentials}, Date, Map, Math,
    location: {hostname: 'example.org'}, Uint8Array, ArrayBuffer,
    btoa: value => Buffer.from(value, 'binary').toString('base64'),
    setTimeout: () => 1, setInterval: () => 1, clearInterval() {}, DOMException});
  vm.runInContext(declaration + '; globalThis.hookSource = `(${passkeyHook.toString()})();`', context);
  vm.runInContext(context.hookSource, context);
  const old = credentials.get.bind(credentials);
  Object.defineProperty(credentials, 'get', {configurable: true, writable: false,
    value: async options => { providerCalls++; return old(options); }});
  context.__bdPasskeyRepair();
  const repaired = credentials.get;
  context.__bdPasskeyRepair();
  assert.equal(credentials.get, repaired, 'heartbeat must not stack wrappers');
  context.__bdPasskeyAlive = Date.now();
  context.__bdPasskey = payload => { offers++; context.__bdPasskeyAnswer(JSON.parse(payload).id, true); };
  const options = {publicKey: {challenge: new Uint8Array([1, 2, 3])}};
  assert.equal(await credentials.get(options), 'native');
  assert.equal(offers, 1);
  assert.equal(providerCalls, 1);
  assert.equal(nativeCalls, 1);
  context.__bdPasskeyAlive = 0;
  assert.equal(await credentials.get(options), 'native');
  assert.equal(offers, 1, 'unwatched requests must go straight to the provider');
  assert.equal(providerCalls, 2);
  assert.equal(nativeCalls, 2);
});

const proxyContext = vm.createContext({URL, btoa});
vm.runInContext(source('proxy.js'), proxyContext);
const {NodeProxy: np} = proxyContext;

test('the daemon proxy offer is checked, and the login names the proxy', () => {
  const offer = np.offer({port: 3128, password: 'abc123', proxies: [{id: 'node:a', name: 'a (node)'}]}, 'https://Daemon.example:8443');
  assert.deepEqual({...offer, proxies: offer.proxies.length}, {host: 'daemon.example', port: 3128, password: 'abc123', node: '', user: '', proxies: 1});
  assert.equal(np.offer({port: 1, password: 'x', node: 'fnuc'}, 'https://d.example').node, 'fnuc');
  assert.equal(np.offer({host: '[fd00::1]', port: 1, password: 'x'}, 'https://d.example').host, 'fd00::1');
  for (const bad of [null, {}, {port: 0, password: 'x'}, {port: 70000, password: 'x'}, {port: 1, password: ''}, {port: 1, password: 'a b'},
    {port: 1, password: 'x', host: 'a/b'}, {port: 1, password: 'x', host: 'evil.example\r\nX: y'},
    {port: 1, password: 'x', proxies: [{id: 'direct', name: 'x'}]}, {port: 1, password: 'x', proxies: [{id: '', name: 'x'}]},
    {port: 1, password: 'x', proxies: 'nope'}]) {
    assert.throws(() => np.offer(bad, 'https://d.example'), undefined, JSON.stringify(bad));
  }
  // The colon of an id would end the user name: it travels percent-encoded.
  assert.equal(np.identity({user: ''}, 'node:a'), 'node%3Aa');
  assert.equal(np.identity({user: 'abc-1'}, 'node:a'), 'abc-1');
  assert.equal(np.login('node%3Aa', 'pw'), 'Basic ' + btoa('node%3Aa:pw'));
  assert.equal(np.offer({port: 1, password: 'x', user: 'bad user'}, 'https://d.example').user, '');
});

test('local names and private networks never go through the proxy', () => {
  const daemons = ['daemon.example'];
  for (const host of ['localhost', 'printer', 'nas.local', 'a.localhost', '127.0.0.1', '10.1.2.3', '172.20.0.1', '192.168.1.1',
    '100.64.9.9', '169.254.1.1', '[::1]', 'fd00::5', 'fe80::1', 'daemon.example', 'DAEMON.example.']) {
    assert.equal(np.direct(host, daemons), true, host);
  }
  for (const host of ['example.com', '8.8.8.8', '172.32.0.1', '100.128.0.1', '11.0.0.1', '2001:db8::1']) {
    assert.equal(np.direct(host, daemons), false, host);
  }
  const rules = np.bypass(['daemon.example', 'daemon.example']);
  assert.ok(rules.includes('10.0.0.0/8') && rules.includes('<local>'));
  assert.equal(rules.filter(r => r === 'daemon.example').length, 1);
});

const plain = value => JSON.parse(JSON.stringify(value));
test('a paired browser routes through the proxy it picks, only while connected', async () => {
  const h = harness();
  const applied = [];
  h.api.proxy = {settings: {set: async value => { applied.push(['set', value]); }, clear: async () => { applied.push(['clear']); }}};
  await h.message({type: 'connect', server: 'https://daemon.example', code: 'a'.repeat(64), name: 'Test browser'});
  const socket = h.sockets.at(-1); socket.onopen();
  socket.receive({type: 'registered', id: 'node', expires_at: Math.floor(Date.now() / 1000) + 600, origins: ['https://peer.example'],
    proxy: {port: 3128, password: 'secret-login', proxies: [{id: 'node:peer', name: 'peer (node)'}]}});
  await h.flush();
  let state = await h.message({type: 'state'});
  assert.deepEqual(plain(state.proxy.choices), [{id: 'node:peer', name: 'peer (node)'}]);
  assert.equal(state.proxy.selected, null);
  // The login stays out of everything the popup is handed.
  assert.doesNotMatch(JSON.stringify(state), /secret-login/);
  assert.equal(applied.some(([what]) => what === 'set'), false);

  assert.ok((await h.message({type: 'setProxy', id: 'node:nobody'})).error);
  assert.deepEqual(plain(await h.message({type: 'setProxy', id: 'node:peer'})), {ok: true});
  const [, set] = applied.findLast(([what]) => what === 'set');
  assert.deepEqual(plain(set.value.rules.singleProxy), {scheme: 'http', host: 'daemon.example', port: 3128});
  assert.ok(set.value.rules.bypassList.includes('peer.example') && set.value.rules.bypassList.includes('daemon.example'));
  assert.equal((await h.message({type: 'state'})).proxy.selected, 'node:peer');

  // The heartbeat refreshes the choices.
  socket.receive({type: 'heartbeat', proxies: [{id: 'node:other', name: 'other (node)'}]});
  await h.flush();
  assert.deepEqual(plain((await h.message({type: 'state'})).proxy.choices), [{id: 'node:other', name: 'other (node)'}]);

  // Leaving the daemon leaves the proxy: nothing else would answer for it.
  applied.length = 0;
  await h.message({type: 'stop'});
  await h.flush();
  assert.deepEqual(plain(applied.at(-1)), ['clear']);
  assert.equal((await h.message({type: 'state'})).proxy, null);
});

test('a daemon without a proxy offers none', async () => {
  const h = harness();
  await h.pair();
  assert.equal((await h.message({type: 'state'})).proxy, null);
});

for (const firefox of [false, true]) {
  test(`${firefox ? 'Firefox' : 'Chromium'} answers the proxy's login with the proxy's name and password`, async () => {
    const h = harness(firefox);
    let answer;
    h.api.webRequest = {onAuthRequired: {hasListener: () => false, addListener: listener => { answer = listener; }}};
    h.api.proxy = {settings: {set: async () => {}, clear: async () => {}}, onRequest: {hasListener: () => false, addListener() {}}};
    await h.message({type: 'connect', server: 'https://daemon.example', code: 'a'.repeat(64), name: 'Test browser'});
    const socket = h.sockets.at(-1); socket.onopen();
    socket.receive({type: 'registered', id: 'node', expires_at: Math.floor(Date.now() / 1000) + 600,
      proxy: {port: 3128, password: 'pw123', proxies: [{id: 'node:peer', name: 'peer'}]}});
    await h.flush();
    await h.message({type: 'setProxy', id: 'node:peer'});
    const ask = details => firefox ? answer(details) : new Promise(resolve => answer(details, resolve));
    const proxy = {isProxy: true, challenger: {host: 'daemon.example', port: 3128}, requestId: '1'};
    assert.deepEqual(plain(await ask(proxy)), {authCredentials: {username: 'node%3Apeer', password: 'pw123'}});
    // Not offered again for a request the proxy refused, nor to another server.
    assert.deepEqual(plain(await ask(proxy)), {cancel: true});
    assert.deepEqual(plain(await ask({...proxy, requestId: '2', isProxy: false})), {});
    assert.deepEqual(plain(await ask({...proxy, requestId: '3', challenger: {host: 'evil.example', port: 3128}})), {});
  });
}

test('a daemon that issues the browser its login gets the pick over the connection, under a login that stays', async () => {
  const h = harness();
  let answer;
  h.api.webRequest = {onAuthRequired: {hasListener: () => false, addListener: listener => { answer = listener; }}};
  h.api.proxy = {settings: {set: async () => {}, clear: async () => {}}};
  await h.message({type: 'connect', server: 'https://daemon.example', code: 'a'.repeat(64), name: 'Test browser'});
  const socket = h.sockets.at(-1); socket.onopen();
  socket.receive({type: 'registered', id: 'node', expires_at: Math.floor(Date.now() / 1000) + 600,
    proxy: {port: 3128, password: 'pw123', user: 'browser-1', proxies: [{id: 'node:a', name: 'a'}, {id: 'node:b', name: 'b'}]}});
  await h.flush();
  const picks = () => socket.sent.filter(m => m.type === 'proxy').map(m => m.choice);
  const ask = id => new Promise(resolve => answer({isProxy: true, challenger: {host: 'daemon.example', port: 3128}, requestId: id}, resolve));
  await h.message({type: 'setProxy', id: 'node:a'});
  assert.equal(plain(await ask('1')).authCredentials.username, 'browser-1');
  await h.message({type: 'setProxy', id: 'node:b'});
  assert.equal(plain(await ask('2')).authCredentials.username, 'browser-1');
  await h.message({type: 'setProxy', id: ''});
  assert.deepEqual(plain(picks()), [null, 'node:a', 'node:b', null]);
});

test('SOCKS answer names the proxy in the user', () => {
  const offer = {host: 'daemon.example', port: 3128, password: 'pw'};
  assert.deepEqual(plain(np.socks(offer, 'node:a')), {type: 'socks', host: 'daemon.example', port: 3128, username: 'node:a', password: 'pw', proxyDNS: true});
});

test('Firefox routes each tab by its own pick, and the tab menu sets it and shares the tab', async () => {
  const h = harness(true);
  let route;
  h.api.proxy = {onRequest: {hasListener: () => false, addListener: fn => { route = fn; }}};
  await h.message({type: 'connect', server: 'https://daemon.example', code: 'a'.repeat(64), name: 'Test browser'});
  const socket = h.sockets.at(-1); socket.onopen();
  socket.receive({type: 'registered', id: 'node', expires_at: Math.floor(Date.now() / 1000) + 600, origins: ['https://daemon.example'],
    proxy: {port: 3128, password: 'pw', user: 'browser-1', node: 'fnuc', proxies: [{id: 'node:a', name: 'a (node)'}, {id: 'node:b', name: 'b (node)'}]}});
  await h.flush();
  await h.message({type: 'setProxy', id: 'node:a'});
  const to = (tabId, url = 'https://site.example/') => plain(route({tabId, url}));
  assert.equal(to(1).username, 'node:a');
  assert.equal(to(1).type, 'socks');
  // The menu: this daemon's own network, a proxy, none, and back to the default.
  const items = h.menu.created.map(o => o.id);
  assert.ok(items.includes('bd-share') && items.includes('bd-proxy:follow') && items.includes('bd-proxy:node:b'), items.join());
  assert.equal(h.menu.created.find(o => o.id === 'bd-proxy:direct').title, 'fnuc (this daemon)');
  const click = id => h.listeners.menuClicked({menuItemId: id}, {id: 1, url: 'https://one.example/'});
  click('bd-proxy:node:b'); await h.flush();
  assert.equal(to(1).username, 'node:b');
  assert.equal(to(4).username, 'node:a');
  click('bd-proxy:none'); await h.flush();
  assert.equal(to(1).type, 'direct');
  click('bd-proxy:direct'); await h.flush();
  assert.equal(to(1).username, 'direct');
  click('bd-proxy:follow'); await h.flush();
  assert.equal(to(1).username, 'node:a');
  // Local addresses and the daemon stay direct, whatever the pick.
  assert.equal(to(1, 'http://192.168.1.5/').type, 'direct');
  assert.equal(to(1, 'https://daemon.example/').type, 'direct');
  // The menu shows what is picked for the tab it opens on.
  click('bd-proxy:node:b'); await h.flush();
  h.menu.updated.length = 0;
  h.listeners.menuShown({contexts: ['tab']}, {id: 1, url: 'https://one.example/', incognito: false});
  assert.ok(h.menu.updated.some(([id, o]) => id === 'bd-proxy:node:b' && o.checked === true));
  assert.ok(h.menu.updated.some(([id, o]) => id === 'bd-proxy:follow' && o.checked === false));
  // Sharing and unsharing the tab.
  const shareTitles = () => h.menu.updated.filter(([id]) => id === 'bd-share').map(([, o]) => o.title).at(-1);
  h.menu.updated.length = 0;
  h.listeners.menuShown({contexts: ['tab']}, {id: 1, url: 'https://one.example/', incognito: false});
  assert.equal(shareTitles(), 'Share this tab with the daemon');
  click('bd-share'); await h.flush();
  h.menu.updated.length = 0;
  h.listeners.menuShown({contexts: ['tab']}, {id: 1, url: 'https://one.example/', incognito: false});
  assert.equal(shareTitles(), 'Stop sharing this tab');
  click('bd-share'); await h.flush();
  h.menu.updated.length = 0;
  h.listeners.menuShown({contexts: ['tab']}, {id: 1, url: 'https://one.example/', incognito: false});
  assert.equal(shareTitles(), 'Share this tab with the daemon');
  // The popup says which proxy the tab in front uses.
  h.tabs.get(1).active = true;
  const state = plain((await h.message({type: 'state'})).proxy);
  assert.equal(state.perTab, true);
});
