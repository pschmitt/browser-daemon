/* Local extension code only; dashboard bridge requires explicit daemon approval. */
if (typeof importScripts === 'function') importScripts('policy.js');
const api = globalThis.browser || chrome;
const chromium = typeof api.runtime.getBrowserInfo !== 'function';
void api.storage.local.setAccessLevel?.({accessLevel: 'TRUSTED_CONTEXTS'});
let socket = null;
let registration = null;
let shared = new Map();
const attached = new Set();
const attaching = new Map();
// Live views (daemon relay channels) per tab, and Firefox's emulated screencasts.
const channels = new Map();
const casts = new Map();
// Chromium screencasts, per tab: frames are acknowledged here at once, so the
// browser never waits for a viewer's round trip; viewers get at most
// CAST_WINDOW in flight (enough for ~50 fps at 80 ms) and, while they are
// behind, only the newest frame.
const CAST_WINDOW = 4;
const screencasts = new Map();
function castState(tab) {
  if (!screencasts.has(tab)) screencasts.set(tab, {inflight: 0, pending: null});
  return screencasts.get(tab);
}
// By default every tab is shared (all: true), with the remote control choice.
// Without that, chosen tabs are, and new ones join if autoShare.
let sharing = {all: true, autoShare: true, control: false};
const sharingReady = api.storage.local.get(['shareAll', 'autoShare', 'shareControl']).then(saved => {
  sharing = {all: saved.shareAll !== false, autoShare: saved.autoShare !== false, control: saved.shareControl === true};
});
// Controllers may open tabs where those are shared with control.
const canOpen = () => sharing.control && (sharing.all || sharing.autoShare);
async function shareEverything() {
  if (!registration || !sharing.all) return;
  for (const tab of await api.tabs.query({})) {
    if (!tab.incognito && !shared.has(tab.id)) shared.set(tab.id, {origin: null, control: sharing.control, paused: true});
  }
  publishSoon();
}
async function openTab(url, windowId) {
  const created = await api.tabs.create({url, active: true, ...(windowId ? {windowId} : {})});
  shared.set(created.id, {origin: null, control: true, paused: true});
  publishSoon();
  return {tab: created.id};
}
// Every allowed daemon origin, so no shared tab can ever show a daemon page.
let daemons = new Set();
let policyCache = null;
let heartbeat = null;
let expiryTimer = null;
let lastAnswer = 0;
let restoring = false;
let status = 'Disconnected';
let generation = 0;
let busy = 0;
let tabSerial = Promise.resolve();
let nativePort = null;
let nativeReady = false;
let pendingConnection = null;
let pendingTimer = null;
let history = [];
let historyWrites = Promise.resolve();
const historyReady = api.storage.session.get('connectionHistory').then(saved => {
  history = Array.isArray(saved.connectionHistory) ? saved.connectionHistory.slice(-12) : [];
  if (history.at(-1)?.message === 'Connected') status = 'Background restarted — pair again to reconnect';
});
function record(message) {
  history.push({at: Date.now(), message});
  history = history.slice(-12);
  historyWrites = historyWrites.then(() => api.storage.session.set({connectionHistory: history})).catch(() => {});
}
function permissionPattern(server) {
  const u = new URL(server);
  return `${u.protocol}//${u.hostname}/*`;
}
async function connectPrepared() {
  if (!pendingConnection) return;
  const candidate = pendingConnection;
  if (!(await api.permissions.contains({origins: [permissionPattern(candidate.server)]}))) return;
  if (pendingConnection !== candidate) return;
  pendingConnection = null;
  clearTimeout(pendingTimer);
  try { await connect(candidate); }
  catch { await stop('Connection could not start — check the daemon URL and permissions'); }
}
const nativePending = new Map();

let lastDashboardRequest = 0;
// Allowed daemons whose site access was granted: their dashboards can pair.
async function dashboardOrigins() {
  let policy;
  try { policy = await cachedPolicy(); } catch { return []; }
  const granted = [];
  for (const origin of policy.origins) {
    if (await api.permissions.contains({origins: [permissionPattern(origin)]})) granted.push(origin);
  }
  return granted;
}
async function installDashboardBridge() {
  const bases = await dashboardOrigins();
  const matches = bases.flatMap(base => [permissionPattern(base).replace('/*', '/'), permissionPattern(base).replace('/*', '/extension/*')]);
  const registered = await api.scripting.getRegisteredContentScripts({ids: ['daemon-pairing']});
  if (JSON.stringify(registered[0]?.matches) !== JSON.stringify(matches)) {
    if (registered.length) await api.scripting.unregisterContentScripts({ids: ['daemon-pairing']});
    if (matches.length) await api.scripting.registerContentScripts([{id: 'daemon-pairing', matches, js: ['dashboard-bridge.js'], runAt: 'document_idle', allFrames: false}]);
  }
  for (const base of bases) {
    for (const tab of await api.tabs.query({url: permissionPattern(base)})) {
      const url = new URL(tab.url);
      if (url.origin === base && ['/', '/extension/'].includes(url.pathname)) {
        try { await api.scripting.executeScript({target: {tabId: tab.id}, files: ['dashboard-bridge.js']}); } catch { /* Tab may have closed. */ }
      }
    }
  }
}
// The administrator's name for this browser (e.g. laptop-firefox), else os-browser.
async function defaultName() {
  try {
    const {defaultName: name} = await api.storage.managed.get('defaultName');
    if (typeof name === 'string' && name.trim()) return name.trim().slice(0, 80);
  } catch { /* No managed policy. */ }
  const os = (await api.runtime.getPlatformInfo?.().catch(() => null))?.os;
  return `${os && os !== 'linux' ? os : 'linux'}-${chromium ? 'chromium' : 'firefox'}`;
}
async function dashboardMessage(message, sender) {
  const url = new URL(sender.url);
  if (sender.id !== api.runtime.id || sender.frameId !== 0 || !sender.tab ||
      !(await dashboardOrigins()).includes(url.origin) || !['/', '/extension/'].includes(url.pathname)) {
    throw new Error('Dashboard pairing is available only on allowed daemons.');
  }
  // The node ID lets the dashboard mark this browser's own entry.
  if (message.action === 'discover') return {origin: api.runtime.getURL('').replace(/\/$/, ''), connected: !!registration, node: registration?.id || null, browser: chromium ? 'Chromium' : 'Firefox'};
  if (message.action !== 'approve' || !/^[a-f0-9]{64}$/.test(message.code)) throw new Error('Invalid pairing request.');
  if (registration) throw new Error('Already connected. Stop sharing in the extension before pairing again.');
  if (Date.now() - lastDashboardRequest < 5000) throw new Error('A pairing request is already being shown.');
  lastDashboardRequest = Date.now();
  await api.storage.session.set({dashboardPairing: {server: url.origin, code: message.code, remember: message.remember === true, control: message.control !== false, expires: Date.now() + 300000}});
  approvalPending = Date.now() + 300000;
  await badge();
  record('Dashboard requested pairing — waiting for your approval');
  return {pending: true, where: await openApproval(sender.tab.windowId)};
}

// The toolbar popup where the browser allows opening it, else an extension tab.
async function openApproval(windowId) {
  try { await api.action.openPopup(windowId ? {windowId} : {}); return 'popup'; } catch { /* Not allowed here. */ }
  await api.tabs.create({url: api.runtime.getURL('popup.html?tab'), windowId});
  return 'tab';
}

async function daemonPolicy() {
  const local = await api.storage.local.get(['daemonAllowlist', 'managedAllowlistSeen']);
  let managed = {};
  try { managed = await api.storage.managed.get('allowedDaemonOrigins'); } catch {
    if (local.managedAllowlistSeen) throw new Error('Managed daemon allowlist is unavailable. Connections are blocked until policy is restored.');
  }
  if (Object.hasOwn(managed, 'allowedDaemonOrigins')) {
    if (!local.managedAllowlistSeen) await api.storage.local.set({managedAllowlistSeen: true});
    return {managed: true, origins: NodePolicy.daemonOrigins(managed.allowedDaemonOrigins)};
  }
  if (local.managedAllowlistSeen) throw new Error('Managed daemon allowlist was removed. Connections are blocked until policy is restored.');
  return {managed: false, origins: NodePolicy.daemonOrigins(local.daemonAllowlist || [])};
}
// Read once per change, not per request: live views send many small messages.
function cachedPolicy() {
  policyCache ||= daemonPolicy().then(policy => { daemons = new Set(policy.origins); return policy; },
    error => { policyCache = null; throw error; });
  return policyCache;
}
async function requireDaemon(server) {
  const policy = await cachedPolicy();
  return NodePolicy.approvedServer(server, policy.origins);
}

function nativeRequest(message) {
  if (!nativeReady || !nativePort) return Promise.reject(new Error('Firefox companion is not ready.'));
  if (nativePending.size >= 32) return Promise.reject(new Error('Firefox companion is busy.'));
  const id = crypto.randomUUID();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { nativePending.delete(id); reject(new Error('Native action timed out.')); }, 18000);
    nativePending.set(id, {resolve, reject, timer, chunks: '', size: 0});
    nativePort.postMessage({...message, id});
  });
}

function disconnectNative() {
  const port = nativePort;
  nativePort = null;
  nativeReady = false;
  for (const pending of nativePending.values()) {
    clearTimeout(pending.timer);
    pending.reject(new Error('Firefox companion disconnected.'));
  }
  nativePending.clear();
  if (port) port.disconnect();
}

async function enableNative() {
  if (chromium) throw new Error('The companion is for Firefox.');
  if (nativeReady) return;
  const port = api.runtime.connectNative('dev.pschmitt.browser_daemon');
  nativePort = port;
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { disconnectNative(); reject(new Error('Firefox companion did not start.')); }, 18000);
    port.onMessage.addListener(message => {
      if (nativePort !== port) return;
      if (message.type === 'ready' && !nativeReady) {
        nativeReady = true;
        clearTimeout(timer);
        resolve();
        return;
      }
      const pending = nativePending.get(message.id);
      if (!pending || typeof message.chunk !== 'string') return;
      pending.size += message.chunk.length;
      if (pending.size > 8 * 1024 * 1024) { void stop('Native response is too large'); return; }
      pending.chunks += message.chunk;
      if (message.done) {
        clearTimeout(pending.timer);
        nativePending.delete(message.id);
        try {
          const result = JSON.parse(pending.chunks);
          if (result.error) pending.reject(new Error(result.error));
          else pending.resolve(result.result);
        } catch { pending.reject(new Error('Invalid native response.')); }
      }
    });
    port.onDisconnect.addListener(() => {
      clearTimeout(timer);
      reject(new Error('Companion unavailable. Install it and restart Firefox with MOZ_REMOTE_ALLOW_SYSTEM_ACCESS=1 and --remote-debugging-port 9222.'));
      if (nativePort === port) void stop('Firefox companion disconnected');
    });
  });
}

async function syncNativeGrants() {
  if (!nativeReady) return;
  try {
    await nativeRequest({type: 'grants', grants: [...shared].filter(([, grant]) => grant.origin).map(([tab, grant]) => ({
      tab, origin: grant.origin, control: grant.control, expires_at: registration.expires_at || Math.floor(Date.now() / 1000) + 300,
    }))});
  } catch {
    await stop('Could not synchronize native tab permissions');
    throw new Error('Native tab permission update failed; pair again to reconnect.');
  }
}

function publishSafely() { void publish().catch(() => stop('Could not update shared tabs')); }

let published = [];
let approvalPending = 0;
// The number of shared tabs: red when any may be controlled, green when view only.
// A "?" asks for a decision: a dashboard waits for pairing approval.
async function badge() {
  if (approvalPending > Date.now() && !registration) {
    await api.action.setBadgeText({text: '?'});
    await api.action.setBadgeBackgroundColor({color: '#6d4aff'});
    await api.action.setTitle({title: 'Browser node: a dashboard asks to connect — click to answer'});
    return;
  }
  const count = registration ? published.length : 0;
  const control = published.some(t => t.control);
  await api.action.setBadgeText({text: registration ? String(count) : ''});
  await api.action.setBadgeBackgroundColor({color: !count ? '#6b7280' : control ? '#c73545' : '#23745d'});
  await api.action.setTitle({title: !registration ? 'Browser node: disconnected'
    : `Browser node: ${count} shared tab${count === 1 ? '' : 's'}${count ? (control ? ', remote control allowed' : ', view only') : ''}`});
}

async function detach(tab) {
  for (const [channel, owner] of channels) if (owner === tab) { channels.delete(channel); stopCast(channel); }
  if (!attached.delete(tab)) return;
  try { await api.debugger.detach({tabId: tab}); } catch { /* Already closed/detached. */ }
}

async function stop(reason = 'Disconnected', retry = false) {
  generation++;
  const clearedAlarm = api.alarms.clear('node-reconnect');
  status = reason;
  record(reason);
  pendingConnection = null;
  clearTimeout(pendingTimer);
  const old = socket;
  socket = null;
  registration = null;
  clearInterval(heartbeat);
  clearTimeout(expiryTimer);
  shared.clear();
  published = [];
  for (const channel of casts.keys()) stopCast(channel);
  channels.clear();
  disconnectNative();
  if (old) old.close();
  await clearedAlarm;
  await Promise.all([...attached].map(detach));
  await badge();
  if (retry) {
    const {rememberedNode} = await api.storage.local.get('rememberedNode');
    if (rememberedNode && !rememberedNode.paused) {
      status = 'Disconnected — reconnecting automatically within a minute'; record(status);
      await api.alarms.create('node-reconnect', {delayInMinutes: 1});
    }
  }
}

function send(value) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(value));
}

async function publish() {
  const currentGeneration = generation;
  if (!registration) return;
  await requireDaemon(registration.server);
  if (generation !== currentGeneration) return;
  const tabs = [];
  for (const [id, grant] of shared) {
    try {
      const tab = await api.tabs.get(id);
      // Sharing belongs to the tab and follows it across sites. While it shows a
      // page that must never be shared (a daemon, loopback, browser pages) it is
      // paused: unlisted, and nothing is read from it or sent to it.
      grant.paused = !safeTab(tab);
      if (grant.paused) continue;
      grant.origin = new URL(tab.url).origin;
      if (shared.get(id) === grant) tabs.push({id, title: (tab.title || '').slice(0, 1024), url: tab.url.slice(0, 8192), control: grant.control, active: tab.active === true});
    } catch { shared.delete(id); await detach(id); }
  }
  if (generation !== currentGeneration) return;
  await syncNativeGrants();
  if (generation !== currentGeneration) return;
  published = tabs;
  send({type: 'tabs', tabs, native_control: nativeReady, can_open: canOpen()});
  await badge();
}

function safeURL(url) {
  if (!NodePolicy.webURL(url)) return false;
  // A controller must never read its own pairing UI, a dashboard's credential,
  // or a code printed in a localhost daemon page.
  const u = new URL(url);
  return !['localhost', '127.0.0.1', '[::1]'].includes(u.hostname) &&
    !daemons.has(u.origin) && (!registration || u.origin !== registration.server);
}
function safeTab(tab) { return !tab.incognito && safeURL(tab.url); }

async function grantFor(command) {
  if (registration) await requireDaemon(registration.server);
  const grant = shared.get(command.tab);
  if (!registration || (registration.expires_at !== 0 && Date.now() >= registration.expires_at * 1000) ||
      !grant || !NodePolicy.allowed(command, grant.control)) throw new Error('Tab grant does not permit this action.');
  const tab = await api.tabs.get(command.tab);
  if (!safeTab(tab) || shared.get(command.tab) !== grant) throw new Error('Tab is paused or sharing ended.');
  grant.origin = new URL(tab.url).origin;
  return grant;
}

// One attachment per tab, shared by concurrent requests so input keeps its order.
function attach(tab) {
  if (attached.has(tab)) return Promise.resolve();
  if (!attaching.has(tab)) {
    attaching.set(tab, (async () => {
      try {
        await api.debugger.attach({tabId: tab}, '1.3');
        attached.add(tab);
        if (!shared.has(tab) || !registration) { await detach(tab); throw new Error('Sharing ended.'); }
        // Page events tell navigations apart before any frame of the new page.
        await api.debugger.sendCommand({tabId: tab}, 'Page.enable', {});
      } finally { attaching.delete(tab); }
    })());
  }
  return attaching.get(tab);
}
async function cdp(tab, method, params = {}) {
  if (!attached.has(tab)) await attach(tab);
  return api.debugger.sendCommand({tabId: tab}, method, params);
}

// Runs in an isolated extension world. Arguments are data; never eval/code.
function pageAction(action, params, expectedOrigin) {
  if (location.origin !== expectedOrigin) throw new Error('Page changed site.');
  const finite = x => typeof x === 'number' && Number.isFinite(x);
  if (action === 'snapshot') {
    return {title: document.title, url: location.href,
      text: (document.body?.innerText || '').slice(0, 100000),
      elements: [...document.querySelectorAll('a,button,input,textarea,select,[role="button"]')].slice(0, 400).map(e => ({
        tag: e.tagName, role: e.getAttribute('role'), name: e.getAttribute('aria-label') || e.innerText?.slice(0, 160) || e.getAttribute('placeholder') || '',
        type: e.getAttribute('type'), rect: (() => { const r = e.getBoundingClientRect(); return {x: r.x, y: r.y, width: r.width, height: r.height}; })(),
      }))};
  }
  if (action === 'scroll') {
    if (!finite(params.x) || !finite(params.y)) throw new Error('Expected scroll offsets.');
    window.scrollBy(params.x, params.y);
  } else if (action === 'click') {
    const e = params.selector ? document.querySelector(params.selector) : document.elementFromPoint(params.x, params.y);
    if (!e) throw new Error('Element not found.');
    e.focus(); e.click();
  } else if (action === 'type') {
    const e = params.selector ? document.querySelector(params.selector) : document.activeElement;
    if (!e || typeof params.text !== 'string') throw new Error('Expected a field and text.');
    e.focus();
    if (e.isContentEditable) {
      document.execCommand('insertText', false, params.text);
    } else {
      const proto = e instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
      if (!setter) throw new Error('Not a text field.');
      setter.call(e, params.text);
      e.dispatchEvent(new Event('input', {bubbles: true}));
      e.dispatchEvent(new Event('change', {bubbles: true}));
    }
  } else if (action === 'key') {
    const e = document.activeElement;
    if (params.key === 'Enter' && e?.form) e.form.requestSubmit();
    else if (params.key === 'Tab') {
      const fields = [...document.querySelectorAll('a[href],button,input,select,textarea,[tabindex="0"]')].filter(e => !e.disabled);
      fields[(fields.indexOf(e) + 1) % fields.length]?.focus();
    } else throw new Error('Firefox supports Enter (forms) and Tab; other trusted keys require Chromium.');
  } else throw new Error('Unsupported page action.');
  return {ok: true};
}

async function execute(command) {
  const grant = await grantFor(command);
  const {tab, action, params} = command;
  if (!chromium && nativeReady && ['screenshot', 'snapshot', 'click', 'type', 'key', 'scroll', 'evaluate'].includes(action)) {
    return nativeRequest({tab, action, params});
  }
  if (action === 'evaluate') throw new Error('Evaluation requires the Firefox companion; use CDP Runtime.evaluate on Chromium.');
  if (action === 'screenshot') {
    if (chromium) {
      const metrics = await cdp(tab, 'Page.getLayoutMetrics');
      return {data: (await cdp(tab, 'Page.captureScreenshot', {format: 'jpeg', quality: 65})).data,
        format: 'jpeg', width: metrics.cssVisualViewport?.clientWidth, height: metrics.cssVisualViewport?.clientHeight};
    }
    const data = await api.tabs.captureTab(tab, {format: 'jpeg', quality: 65});
    const currentTab = await api.tabs.get(tab);
    return {data: data.split(',')[1], format: 'jpeg', width: currentTab.width, height: currentTab.height};
  }
  if (action === 'snapshot') {
    if (chromium) return cdp(tab, 'Accessibility.getFullAXTree');
  } else if (action === 'navigate') {
    await api.tabs.update(tab, {url: params.url});
    return {ok: true};
  } else if (action === 'open') {
    // A controller opens a tab next to the one it controls; it is shared with control.
    return openTab(params.url, (await api.tabs.get(tab)).windowId);
  } else if (action === 'reload') { await api.tabs.reload(tab); return {ok: true}; }
  else if (action === 'back') { await api.tabs.goBack(tab); return {ok: true}; }
  else if (action === 'forward') { await api.tabs.goForward(tab); return {ok: true}; }
  else if (action === 'focus') { await api.tabs.update(tab, {active: true}); return {ok: true}; }
  else if (action === 'close') { await api.tabs.remove(tab); return {ok: true}; }
  else if (action === 'cdp') {
    if (!chromium) throw new Error('CDP is available only on Chromium.');
    return cdp(tab, params.method, params.params || {});
  }
  if (chromium && !params.selector) {
    if (action === 'click') {
      if (![params.x, params.y].every(Number.isFinite)) throw new Error('Expected coordinates.');
      await cdp(tab, 'Input.dispatchMouseEvent', {type: 'mousePressed', x: params.x, y: params.y, button: 'left', clickCount: 1});
      return cdp(tab, 'Input.dispatchMouseEvent', {type: 'mouseReleased', x: params.x, y: params.y, button: 'left', clickCount: 1});
    }
    if (action === 'type') {
      if (typeof params.text !== 'string') throw new Error('Expected text.');
      return cdp(tab, 'Input.insertText', {text: params.text});
    }
    if (action === 'key') {
      const keys = {Enter: 13, Tab: 9, Escape: 27, Backspace: 8, Delete: 46, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Home: 36, End: 35, PageUp: 33, PageDown: 34};
      if (!Object.hasOwn(keys, params.key)) throw new Error('Unsupported key.');
      const key = {key: params.key, code: params.key, windowsVirtualKeyCode: keys[params.key]};
      await cdp(tab, 'Input.dispatchKeyEvent', {type: 'keyDown', ...key});
      return cdp(tab, 'Input.dispatchKeyEvent', {type: 'keyUp', ...key});
    }
  }
  return (await api.scripting.executeScript({target: {tabId: tab}, func: pageAction, args: [action, params, grant.origin]}))[0]?.result;
}

// The live view's CDP socket, relayed by the daemon. Chromium answers from the
// tab's debugger; Firefox emulates the subset the view uses (see `shim`).
async function relayCdp(message, connection, currentGeneration) {
  const {channel, tab, id, method} = message;
  const params = message.params || {};
  const reply = value => {
    if (socket === connection && currentGeneration === generation) connection.send(JSON.stringify({type: 'cdp_result', channel, id, ...value}));
  };
  try {
    const grant = shared.get(tab);
    if (typeof channel !== 'string' || !registration || !grant || grant.paused ||
        !NodePolicy.relayAllowed(method, params, grant.control)) throw new Error('Not permitted for this shared tab.');
    channels.set(channel, tab);
    if (chromium && method === 'Page.screencastFrameAck') {
      // The browser had its acknowledgement already: this one frees a slot.
      const cast = castState(tab);
      cast.inflight = Math.max(0, cast.inflight - 1);
      if (cast.pending) { const frame = cast.pending; cast.pending = null; cast.inflight++; sendEvent(tab, 'Page.screencastFrame', frame); }
      reply({result: {}});
      return;
    }
    if (chromium && (method === 'Page.startScreencast' || method === 'Page.stopScreencast')) screencasts.delete(tab);
    const result = chromium ? await cdp(tab, method, params) : await shim(tab, channel, method, params);
    // Never return data after local unsharing, pausing or replacement of a grant.
    if (shared.get(tab) !== grant || grant.paused) throw new Error('Sharing ended.');
    reply({result: result ?? {}});
  } catch (error) {
    // Browser/CDP errors can embed page content, input text or script source.
    reply({error: {message: error instanceof ShimError ? error.message : 'Action failed, or the tab is no longer shared.'}});
  }
}
function closeChannel(channel) {
  const tab = channels.get(channel);
  channels.delete(channel);
  stopCast(channel);
  screencasts.delete(tab);
  // Stop encoding frames nobody watches.
  if (chromium && attached.has(tab) && ![...channels.values()].includes(tab)) {
    api.debugger.sendCommand({tabId: tab}, 'Page.stopScreencast', {}).catch(() => {});
  }
}
function sendEvent(tab, method, params, channel) {
  if (shared.get(tab)?.paused === false) send({type: 'cdp_event', tab, method, params, ...(channel ? {channel} : {})});
}

class ShimError extends Error {}
// Firefox has no tab debugger for extensions. Frames are captured on change and
// pushed like a screencast (two in flight at most); input goes through the
// companion's trusted input when it runs, else DOM actions.
async function shim(tab, channel, method, params) {
  const run = (action, extra = {}) => execute({tab, action, params: extra});
  const modifiers = params.modifiers || 0;
  const mods = {alt: !!(modifiers & 1), ctrl: !!(modifiers & 2), meta: !!(modifiers & 4), shift: !!(modifiers & 8)};
  switch (method) {
    case 'Page.enable': case 'Page.getFrameTree': return {};
    case 'Page.startScreencast': startCast(tab, channel, params.quality); return {};
    case 'Page.stopScreencast': stopCast(channel); return {};
    case 'Page.screencastFrameAck': { const cast = casts.get(channel); if (cast) { cast.inflight = Math.max(0, cast.inflight - 1); pump(channel); } return {}; }
    case 'Page.captureScreenshot': return {data: (await capture(tab, params.quality)).data};
    case 'Page.getLayoutMetrics': {
      const t = await api.tabs.get(tab);
      const viewport = {clientWidth: t.width, clientHeight: t.height, pageX: 0, pageY: 0, offsetX: 0, offsetY: 0, scale: 1, zoom: 1};
      return {cssVisualViewport: viewport, cssLayoutViewport: {clientWidth: t.width, clientHeight: t.height, pageX: 0, pageY: 0}};
    }
    // Extensions cannot read session history: the view gets a back and a forward entry.
    case 'Page.getNavigationHistory': {
      const t = await api.tabs.get(tab);
      return {currentIndex: 1, entries: [{id: -1, url: ''}, {id: 0, url: t.url}, {id: 1, url: ''}]};
    }
    case 'Page.navigateToHistoryEntry': return run(params.entryId < 0 ? 'back' : 'forward');
    case 'Page.navigate': return run('navigate', {url: params.url});
    case 'Page.reload': return run('reload');
    case 'Input.insertText': touchCast(tab); return run('type', {text: params.text});
    case 'Input.dispatchKeyEvent':
      if (params.type === 'keyUp' || params.type === 'char') return {};
      touchCast(tab); return run('key', {key: params.key, modifiers: mods});
    case 'Input.dispatchMouseEvent': return pointer(tab, params, run);
    case 'Input.dispatchTouchEvent': {
      const point = params.touchPoints?.[0];
      if (params.type === 'touchStart' && point) return pointer(tab, {type: 'mousePressed', x: point.x, y: point.y, button: 'left'}, run);
      if (params.type === 'touchMove' && point) return pointer(tab, {type: 'mouseMoved', x: point.x, y: point.y, button: 'left'}, run);
      if (params.type === 'touchEnd') { const last = pointers.get(tab); return last ? pointer(tab, {type: 'mouseReleased', x: last.x, y: last.y, button: 'left'}, run) : {}; }
      return {};
    }
    case 'Runtime.evaluate': {
      if (!nativeReady) throw new ShimError('Evaluation in Firefox requires the companion.');
      return {result: {value: await run('evaluate', {script: `return (${params.expression});`, args: []})}};
    }
    default: throw new ShimError(`${method} is not available in Firefox.`);
  }
}
// A press, its drag path and the release become one trusted pointer action.
const pointers = new Map();
async function pointer(tab, params, run) {
  const button = ['left', 'middle', 'right'].indexOf(params.button);
  if (params.type === 'mouseWheel') { touchCast(tab); return run('scroll', {x: params.deltaX || 0, y: params.deltaY || 0}); }
  if (params.type === 'mousePressed') { pointers.set(tab, {x: params.x, y: params.y, button: Math.max(0, button), path: []}); return {}; }
  const down = pointers.get(tab);
  if (!down) return {};
  if (params.type === 'mouseMoved') {
    down.path.push({x: params.x, y: params.y});
    if (down.path.length > 127) down.path.splice(1, 1);
    Object.assign(down, {lastX: params.x, lastY: params.y});
    return {};
  }
  if (params.type !== 'mouseReleased') return {};
  pointers.delete(tab);
  touchCast(tab);
  return run('click', {x: down.x, y: down.y, button: down.button, path: [...down.path, {x: params.x, y: params.y}]});
}
// The browser's own capture: a JPEG at CSS pixels, quick and small. The
// companion's PNG screenshot only without access to the tab's site.
async function capture(tab, quality) {
  // The quality the view asks for (its picture-quality setting), else 65.
  quality = Number.isInteger(quality) && quality >= 10 && quality <= 100 ? quality : 65;
  try {
    const data = await api.tabs.captureTab(tab, {format: 'jpeg', quality, scale: 1});
    return {data: data.split(',')[1], format: 'jpeg'};
  } catch (error) {
    if (!nativeReady) throw error;
    const r = await execute({tab, action: 'screenshot', params: {}});
    return {data: r.data, format: r.format || 'png'};
  }
}
function startCast(tab, channel, quality) {
  stopCast(channel);
  casts.set(channel, {tab, quality, inflight: 0, last: '', seq: 0, timer: null, busy: false});
  pump(channel);
}
function stopCast(channel) {
  const cast = casts.get(channel);
  if (cast) { clearTimeout(cast.timer); casts.delete(channel); }
}
// Input and page loads make the next frames come quickly; an idle page is checked slowly.
function touchCast(tab) {
  for (const [channel, cast] of casts) if (cast.tab === tab) { cast.hot = Date.now(); clearTimeout(cast.timer); cast.timer = setTimeout(() => pump(channel), 60); }
}
async function pump(channel) {
  const cast = casts.get(channel);
  if (!cast || cast.busy || cast.inflight >= 2) return;
  clearTimeout(cast.timer);
  cast.busy = true;
  try {
    const grant = shared.get(cast.tab);
    if (grant && !grant.paused) {
      const t = await api.tabs.get(cast.tab);
      const frame = await capture(cast.tab, cast.quality);
      // Unchanged pictures are not sent: an idle page costs no bandwidth.
      if (casts.get(channel) === cast && frame.data !== cast.last) {
        cast.last = frame.data;
        cast.inflight++;
        sendEvent(cast.tab, 'Page.screencastFrame', {data: frame.data, sessionId: ++cast.seq,
          metadata: {deviceWidth: t.width, deviceHeight: t.height, pageScaleFactor: 1, offsetTop: 0, scrollOffsetX: 0, scrollOffsetY: 0, timestamp: Date.now() / 1000}}, channel);
      }
    }
  } catch { /* The next round tries again; a revoked grant ends the cast. */ }
  finally { cast.busy = false; }
  if (casts.get(channel) !== cast) return;
  cast.timer = setTimeout(() => pump(channel), Date.now() - (cast.hot || 0) < 2000 ? 120 : 500);
}

async function handleCommand(command, connection, currentGeneration) {
  if (busy >= 16) return;
  busy++;
  try {
    // Tab 0: the browser's first tab, where the owner lets controllers open tabs.
    if (command.tab === 0 && command.action === 'open') {
      if (!registration || !canOpen() || !NodePolicy.webURL(command.params?.url)) throw new Error('Opening tabs is not allowed.');
      const result = await openTab(command.params.url);
      if (socket === connection && currentGeneration === generation) connection.send(JSON.stringify({type: 'result', id: command.id, result}));
      return;
    }
    const grant = await grantFor(command);
    const result = await execute(command);
    // Never return data after local unsharing, expiry or replacement of a grant.
    // Navigation/close have no page data to return and may have ended the grant.
    if (!['navigate', 'back', 'forward', 'close'].includes(command.action)) {
      if (shared.get(command.tab) !== grant) throw new Error('Sharing ended.');
      await grantFor(command);
    }
    if (socket === connection && currentGeneration === generation) {
      const response = JSON.stringify({type: 'result', id: command.id, result: result ?? {}});
      connection.send(response.length < 8 * 1024 * 1024 ? response : JSON.stringify({type: 'result', id: command.id, error: 'Response is too large.'}));
    }
  } catch {
    // Browser/CDP errors can embed page content, input text or script source.
    if (socket === connection && connection.readyState === WebSocket.OPEN) connection.send(JSON.stringify({type: 'result', id: command.id, error: 'Action failed, permission was denied, or tab grant ended.'}));
  } finally { busy--; }
}

async function connect({server, code = '', key = '', name, remember = false}) {
  if ((!/^[a-f0-9]{64}$/.test(code) && !/^[a-f0-9]{64}$/.test(key)) || code && key || !name?.trim() || name.length > 80) throw new Error('Enter a pairing code and browser name.');
  server = await requireDaemon(server);
  if (code) {
    const {rememberedNode} = await api.storage.local.get('rememberedNode');
    if (rememberedNode) await api.storage.local.set({rememberedNode: {...rememberedNode, paused: true}});
  }
  await stop();
  const myGeneration = generation;
  status = 'Connecting to daemon';
  record(status);
  const url = new URL('/api/extension/connect', server);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  const connection = new WebSocket(url);
  socket = connection;
  const timeout = setTimeout(() => { if (socket === connection && !registration) void stop('Pairing timed out', true); }, 10000);
  connection.onopen = async () => {
    try { await requireDaemon(server); }
    catch { if (socket === connection) await stop('Daemon is no longer on the allowlist'); return; }
    if (socket !== connection) return;
    status = 'Socket connected — checking pairing code';
    record(status);
    connection.send(JSON.stringify({code, key, remember, name: name.trim(), browser: chromium ? 'chromium' : 'firefox', version: api.runtime.getManifest().version}));
    code = ''; key = '';
  };
  connection.onmessage = async event => {
    if (socket !== connection) return;
    if (event.data.length > 256 * 1024) { void stop('Invalid daemon message'); return; }
    let message;
    try { message = JSON.parse(event.data); } catch { void stop('Invalid daemon message'); return; }
    if (message.type === 'registered' && !registration) {
      if (!Number.isSafeInteger(message.expires_at) || message.expires_at !== 0 &&
          (message.expires_at * 1000 <= Date.now() || message.expires_at * 1000 > Date.now() + 28800000)) { void stop('Invalid grant'); return; }
      clearTimeout(timeout);
      const {remember_key: savedKey, ...grant} = message;
      if (savedKey) {
        if (!/^[a-f0-9]{64}$/.test(savedKey)) { void stop('Invalid remembered browser key'); return; }
        await api.storage.local.set({rememberedNode: {server, name: name.trim(), key: savedKey,
          origin: api.runtime.getURL('').replace(/\/$/, ''), paused: false}});
        if (socket !== connection) {
          const {rememberedNode} = await api.storage.local.get('rememberedNode');
          if (rememberedNode?.key === savedKey) await api.storage.local.set({rememberedNode: {...rememberedNode, paused: true}});
          return;
        }
      }
      registration = {...grant, server};
      status = 'Connected';
      record(status);
      void api.storage.session.remove('pairingDraft');
      lastAnswer = Date.now();
      if (message.expires_at) expiryTimer = setTimeout(() => void stop('Grant expired'), message.expires_at * 1000 - Date.now());
      heartbeat = setInterval(() => {
        if (Date.now() - lastAnswer > 50000) void stop('Daemon stopped answering', true);
        else { send({type: 'heartbeat'}); if (nativeReady && registration?.expires_at === 0) void syncNativeGrants(); }
      }, 20000);
      void badge();
      await sharingReady;
      if (sharing.all) await shareEverything(); else publishSafely();
    } else if (message.type === 'authorization_revoked') {
      await api.storage.local.remove('rememberedNode');
      await stop('Browser authorization was revoked — pair again to reconnect');
    } else if (message.type === 'heartbeat') lastAnswer = Date.now();
    else if (message.type === 'command' && registration) void handleCommand(message, connection, myGeneration);
    else if (message.type === 'cdp' && registration) void relayCdp(message, connection, myGeneration);
    else if (message.type === 'cdp_close') closeChannel(message.channel);
    // A newer daemon may know more message types: ignore them, stay connected.
    else if (typeof message.type !== 'string') void stop('Invalid daemon message');
  };
  connection.onclose = event => { clearTimeout(timeout); if (socket === connection) void stop(registration
    ? `Connection closed (${event.code}) — pair again to reconnect`
    : `Pairing rejected or connection closed (${event.code}). Check that the code is unused, under five minutes old, and issued for this exact extension origin.`, true); };
  connection.onerror = () => { if (socket === connection) void stop('WebSocket connection failed. Check daemon URL, network/TLS, and that /api/extension/connect bypasses the login proxy.', true); };
  // Remember only non-secret convenience settings. Grants/codes/tab IDs never
  // enter persistent storage, and are never restored after a browser restart.
  await api.storage.local.set({server, name});
}

async function restoreRemembered(force = false) {
  if (restoring || registration || socket) return;
  restoring = true;
  try {
    const {rememberedNode: saved} = await api.storage.local.get('rememberedNode');
    if (!saved || saved.paused && !force) return;
    await requireDaemon(saved.server);
    if (saved.origin !== api.runtime.getURL('').replace(/\/$/, '') ||
        !(await api.permissions.contains({origins: [permissionPattern(saved.server)]}))) {
      status = 'Remembered browser needs daemon site permission — open setup';
      record(status); return;
    }
    if (force) await api.storage.local.set({rememberedNode: {...saved, paused: false}});
    await connect({server: saved.server, key: saved.key, name: saved.name});
  } catch { await stop('Remembered browser could not reconnect', true); }
  finally { restoring = false; }
}

async function localMessage(message, sender) {
  // Accept only our own extension UI, never a content script or web page.
  if (message.type === 'dashboardPair') return dashboardMessage(message, sender);
  if (sender.id !== api.runtime.id ||
      sender.url !== api.runtime.getURL('popup.html')) throw new Error('Extension UI only.');
  await historyReady;
  if (message.type === 'configureDashboard') {
    const server = await requireDaemon(message.server);
    await api.storage.local.set({server, name: message.name || ''});
    await installDashboardBridge();
    return {ok: true};
  }
  if (message.type === 'approveDashboard') {
    const {dashboardPairing: pending} = await api.storage.session.get('dashboardPairing');
    if (!pending || pending.expires <= Date.now()) throw new Error('Dashboard request expired. Click Pair browser again.');
    if (!(await dashboardOrigins()).includes(pending.server)) throw new Error('Daemon policy or permission changed. Request pairing again.');
    await api.storage.session.remove('dashboardPairing');
    approvalPending = 0;
    // The control choice applies to the tabs shared from now on (and new tabs).
    sharing.control = message.control === true;
    await api.storage.local.set({shareControl: sharing.control});
    const name = (message.name || '').trim() || await defaultName();
    await connect({server: pending.server, code: pending.code, name, remember: pending.remember && message.remember === true});
    return {ok: true};
  }
  if (message.type === 'rejectDashboard') {
    await api.storage.session.remove('dashboardPairing');
    approvalPending = 0;
    await badge();
    record('Dashboard pairing declined');
    return {ok: true};
  }
  if (message.type === 'prepareConnect') {
    const server = await requireDaemon(message.server);
    if (!/^[a-f0-9]{64}$/.test(message.code) || !message.name?.trim() || message.name.length > 80) throw new Error('Enter a 64-character pairing code and browser name.');
    await api.storage.local.set({server, name: message.name});
    await stop('Waiting for daemon site permission');
    pendingConnection = {server, code: message.code, name: message.name, remember: message.remember === true};
    pendingTimer = setTimeout(() => { if (pendingConnection) void stop('Daemon permission was not granted — connect again when ready'); }, 60000);
    await connectPrepared();
    return {ok: true};
  }
  if (message.type === 'finishConnect') { await connectPrepared(); return {ok: true}; }
  if (message.type === 'setDaemonAllowlist') {
    const current = await daemonPolicy();
    if (current.managed) throw new Error('The daemon allowlist is managed by your administrator.');
    const origins = NodePolicy.daemonOrigins(message.origins);
    await api.storage.local.set({daemonAllowlist: origins});
    if (registration && !origins.includes(registration.server)) await stop('Daemon was removed from the allowlist');
    return {ok: true};
  }
  if (message.type === 'state') {
    let allowlist;
    try { allowlist = await daemonPolicy(); } catch (error) { allowlist = {managed: true, origins: [], error: error.message}; }
    await sharingReady;
    try { await cachedPolicy(); } catch { /* Shown through `allowlist` below. */ }
    const tabs = (await api.tabs.query({})).filter(safeTab).map(t => ({id: t.id, title: t.title || '', url: t.url, active: t.active,
      icon: /^(https:|data:image\/)/.test(t.favIconUrl || '') ? t.favIconUrl : '',
      control: shared.get(t.id)?.control || false, shared: shared.has(t.id)}));
    const {dashboardPairing} = await api.storage.session.get('dashboardPairing');
    const {rememberedNode} = await api.storage.local.get('rememberedNode');
    return {allowlist, rememberedBrowser: rememberedNode ? {server: rememberedNode.server, name: rememberedNode.name, paused: rememberedNode.paused} : null, dashboardRequest: dashboardPairing?.expires > Date.now() ? {server: dashboardPairing.server, remember: dashboardPairing.remember, control: dashboardPairing.control !== false} : null, status, history, connecting: !!pendingConnection || !!socket && !registration, registration, tabs, saved: await api.storage.local.get(['server', 'name', 'rememberPreference']), defaultName: await defaultName(), sharing, version: api.runtime.getManifest().version, chromium, nativeReady};
  }
  if (message.type === 'connect') { await connect(message); return {ok: true}; }
  if (message.type === 'cancelConnect') { await stop('Daemon site permission denied — setup fields saved'); return {ok: true}; }
  if (message.type === 'resumeRemembered') { await restoreRemembered(true); return {ok: true}; }
  if (message.type === 'forgetRemembered') {
    if (socket?.readyState !== WebSocket.OPEN) throw new Error('Reconnect first, or revoke this browser from the dashboard.');
    send({type: 'forget'});
    await api.storage.local.remove('rememberedNode');
    await stop('Remembered browser forgotten'); return {ok: true};
  }
  if (message.type === 'stop') {
    const {rememberedNode} = await api.storage.local.get('rememberedNode');
    if (rememberedNode) await api.storage.local.set({rememberedNode: {...rememberedNode, paused: true}});
    await stop('Sharing stopped — automatic reconnect paused'); return {ok: true};
  }
  if (message.type === 'native') {
    if (!registration) throw new Error('Pair this browser first.');
    if (message.enable) { await enableNative(); if (registration) await syncNativeGrants(); }
    else { await stop('Firefox companion disabled'); }
    return {ok: true};
  }
  if (message.type === 'share') {
    if (!registration || (registration.expires_at !== 0 && Date.now() >= registration.expires_at * 1000)) throw new Error('Pair first.');
    if (!Array.isArray(message.tabs) || message.tabs.length > 128) throw new Error('Select at most 128 tabs.');
    const next = new Map();
    for (const id of new Set(message.tabs)) {
      const tab = await api.tabs.get(id);
      if (!safeTab(tab)) throw new Error('This tab cannot be shared.');
      if (message.urls?.[id] !== tab.url) throw new Error('Tab changed since it was displayed. Reopen the popup and select it again.');
      // An unchanged tab keeps its grant object, so its live views stay open.
      const previous = shared.get(id);
      next.set(id, previous?.control === (message.control === true) ? previous : {origin: new URL(tab.url).origin, control: message.control === true, paused: false});
    }
    shared = next;
    sharing.control = message.control === true;
    await api.storage.local.set({shareControl: sharing.control});
    if (chromium) for (const id of shared.keys()) void attach(id).catch(() => {});
    await Promise.all([...attached].filter(id => !shared.has(id)).map(detach));
    await publish();
    return {ok: true};
  }
  if (message.type === 'autoShare') {
    sharing.autoShare = message.enabled === true;
    await api.storage.local.set({autoShare: sharing.autoShare});
    publishSoon();
    return {ok: true};
  }
  if (message.type === 'shareAll') {
    // Off keeps what is shared now; the tabs can then be switched off one by one.
    sharing.all = message.enabled === true;
    await api.storage.local.set({shareAll: sharing.all});
    if (sharing.all) await shareEverything(); else publishSoon();
    return {ok: true};
  }
  if (message.type === 'setControl') {
    // The same grant objects stay: open live views keep running, with or without input.
    sharing.control = message.control === true;
    await api.storage.local.set({shareControl: sharing.control});
    for (const grant of shared.values()) grant.control = sharing.control;
    await publish();
    return {ok: true};
  }
  throw new Error('Unknown action.');
}

api.runtime.onMessage.addListener((message, sender, respond) => {
  // Keep the response channel open in both Chromium and Firefox.
  // Serialize grant updates so Stop cannot race an in-flight Share.
  tabSerial = tabSerial.then(() => localMessage(message, sender)).then(
    value => respond(value), error => respond({error: error.message}));
  return true;
});
api.tabs.onRemoved.addListener(id => { shared.delete(id); void detach(id); publishSafely(); });
let publishTimer = null;
function publishSoon() { clearTimeout(publishTimer); publishTimer = setTimeout(publishSafely, 150); }
api.tabs.onUpdated.addListener((id, change, tab) => {
  const grant = shared.get(id);
  if (!grant) return;
  if (change.url) {
    // Sharing follows the tab. A page that must not be shared pauses it at once,
    // before anything of that page is captured or relayed.
    grant.paused = !safeTab({incognito: tab?.incognito, url: change.url});
    if (!grant.paused) grant.origin = new URL(change.url).origin;
    if (!chromium) sendEvent(id, 'Page.frameNavigated', {frame: {id: String(id), url: change.url}});
  }
  if (!chromium && change.status === 'loading') { sendEvent(id, 'Page.frameStartedLoading', {frameId: String(id)}); touchCast(id); }
  if (!chromium && change.status === 'complete') { sendEvent(id, 'Page.loadEventFired', {timestamp: Date.now() / 1000}); touchCast(id); }
  if (change.url || change.title || change.status === 'complete') publishSoon();
});
// Which shared tab is in front: recordings follow it.
api.tabs.onActivated.addListener(() => { if (registration) publishSoon(); });
api.tabs.onCreated.addListener(tab => {
  if (!registration || !(sharing.all || sharing.autoShare) || tab.incognito) return;
  // Paused until it shows a shareable page; then listed like any other shared tab.
  shared.set(tab.id, {origin: null, control: sharing.control, paused: true});
  publishSoon();
});
if (chromium) api.debugger.onEvent.addListener((source, method, params) => {
  const grant = shared.get(source.tabId);
  if (!grant || !registration) return;
  if (method === 'Page.frameNavigated' && !params.frame.parentId) {
    grant.paused = !safeURL(params.frame.url);
    if (!grant.paused) grant.origin = new URL(params.frame.url).origin;
    publishSoon();
  }
  if (method === 'Page.screencastFrame') {
    api.debugger.sendCommand({tabId: source.tabId}, 'Page.screencastFrameAck', {sessionId: params.sessionId}).catch(() => {});
    const cast = castState(source.tabId);
    if (cast.inflight >= CAST_WINDOW) { cast.pending = params; return; }
    cast.inflight++;
  }
  sendEvent(source.tabId, method, params);
});
if (chromium) api.debugger.onDetach.addListener((source, reason) => {
  attached.delete(source.tabId);
  // Clicking the browser's "Cancel debugging" bar revokes sharing, and is never
  // silently undone by the next remote request. Other detaches (a browser page,
  // a crash) only end the attachment; the next request attaches again.
  if (reason === 'canceled_by_user') shared.delete(source.tabId);
  for (const [channel, tab] of channels) if (tab === source.tabId) channels.delete(channel);
  publishSafely();
});
api.permissions.onAdded.addListener(() => { void historyReady.then(async () => { await connectPrepared(); await installDashboardBridge(); }).catch(() => stop('Could not start connection after permission grant')); });
api.permissions.onRemoved.addListener(() => { void stop('Permissions removed'); });
api.commands.onCommand.addListener(command => { if (command === 'stop-sharing') {
  void api.storage.local.get('rememberedNode').then(async ({rememberedNode}) => {
    if (rememberedNode) await api.storage.local.set({rememberedNode: {...rememberedNode, paused: true}});
    await stop('Sharing stopped by keyboard shortcut — automatic reconnect paused');
  });
} });
void badge();

// An update waits until the extension is idle, which a connected browser never
// is: apply it at once. A remembered browser reconnects by itself afterwards.
api.runtime.onUpdateAvailable?.addListener(() => {
  record('Extension update available — restarting it');
  void historyWrites.then(() => api.runtime.reload());
});

void historyReady.then(installDashboardBridge).catch(() => record('Dashboard integration unavailable — open extension setup to grant site access'));

api.alarms.onAlarm.addListener(alarm => { if (alarm.name === 'node-reconnect') void historyReady.then(() => restoreRemembered()); });
void historyReady.then(() => restoreRemembered());

api.storage.onChanged.addListener((changes, area) => {
  if (area !== 'managed' && !(area === 'local' && changes.daemonAllowlist)) return;
  policyCache = null;
  void (async () => {
    const server = registration?.server || pendingConnection?.server || (socket && new URL(socket.url).origin.replace(/^ws/, 'http'));
    if (server) {
      try { await requireDaemon(server); }
      catch { await stop('Daemon is no longer on the allowlist'); }
    }
    await installDashboardBridge();
  })().catch(() => stop('Daemon allowlist could not be loaded'));
});
