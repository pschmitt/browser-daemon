/* Local extension code only; dashboard bridge requires explicit daemon approval. */
if (typeof importScripts === 'function') importScripts('policy.js');
const api = globalThis.browser || chrome;
const chromium = typeof api.runtime.getBrowserInfo !== 'function';
void api.storage.local.setAccessLevel?.({accessLevel: 'TRUSTED_CONTEXTS'});
let socket = null;
let registration = null;
let shared = new Map();
const attached = new Set();
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
async function installDashboardBridge() {
  const {server} = await api.storage.local.get('server');
  if (!server) return;
  let base;
  try { base = await requireDaemon(server); } catch { return; }
  if (!(await api.permissions.contains({origins: [permissionPattern(base)]}))) return;
  const matches = [permissionPattern(base).replace('/*', '/'), permissionPattern(base).replace('/*', '/extension/*')];
  const registered = await api.scripting.getRegisteredContentScripts({ids: ['daemon-pairing']});
  if (JSON.stringify(registered[0]?.matches) !== JSON.stringify(matches)) {
    if (registered.length) await api.scripting.unregisterContentScripts({ids: ['daemon-pairing']});
    await api.scripting.registerContentScripts([{id: 'daemon-pairing', matches, js: ['dashboard-bridge.js'], runAt: 'document_idle', allFrames: false}]);
  }
  for (const tab of await api.tabs.query({url: permissionPattern(base)})) {
    const url = new URL(tab.url);
    if (url.origin === base && ['/', '/extension/'].includes(url.pathname)) {
      try { await api.scripting.executeScript({target: {tabId: tab.id}, files: ['dashboard-bridge.js']}); } catch { /* Tab may have closed. */ }
    }
  }
}
async function dashboardMessage(message, sender) {
  const {server} = await api.storage.local.get('server');
  const url = new URL(sender.url);
  if (sender.id !== api.runtime.id || sender.frameId !== 0 || !sender.tab ||
      !server || url.origin !== NodePolicy.serverURL(server) || !['/', '/extension/'].includes(url.pathname)) {
    throw new Error('Dashboard pairing is available only on the configured daemon.');
  }
  await requireDaemon(server);
  if (!(await api.permissions.contains({origins: [permissionPattern(server)]}))) throw new Error('Daemon site access was removed. Enable dashboard pairing again.');
  if (message.action === 'discover') return {origin: api.runtime.getURL('').replace(/\/$/, ''), connected: !!registration, browser: chromium ? 'Chromium' : 'Firefox'};
  if (message.action !== 'approve' || !/^[a-f0-9]{64}$/.test(message.code)) throw new Error('Invalid pairing request.');
  if (registration) throw new Error('Already connected. Stop sharing in the extension before pairing again.');
  if (Date.now() - lastDashboardRequest < 5000) throw new Error('A pairing request is already being shown.');
  lastDashboardRequest = Date.now();
  await api.storage.session.set({dashboardPairing: {server: url.origin, code: message.code, remember: message.remember === true, expires: Date.now() + 300000}});
  await api.tabs.create({url: api.runtime.getURL('popup.html')});
  record('Dashboard requested pairing — waiting for your approval in the extension');
  return {pending: true};
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
async function requireDaemon(server) {
  const policy = await daemonPolicy();
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
      reject(new Error('Companion unavailable. Install it and restart Firefox with the documented startup flags.'));
      if (nativePort === port) void stop('Firefox companion disconnected');
    });
  });
}

async function syncNativeGrants() {
  if (!nativeReady) return;
  try {
    await nativeRequest({type: 'grants', grants: [...shared].map(([tab, grant]) => ({
      tab, origin: grant.origin, control: grant.control, expires_at: registration.expires_at || Math.floor(Date.now() / 1000) + 300,
    }))});
  } catch {
    await stop('Could not synchronize native tab permissions');
    throw new Error('Native tab permission update failed; pair again to reconnect.');
  }
}

function publishSafely() { void publish().catch(() => stop('Could not update shared tabs')); }

async function badge() {
  const control = [...shared.values()].some(g => g.control);
  await api.action.setBadgeText({text: registration ? (control ? 'CTRL' : 'VIEW') : ''});
  await api.action.setBadgeBackgroundColor({color: control ? '#c73545' : '#23745d'});
  await api.action.setTitle({title: registration ? `Browser node: ${shared.size} shared tabs (${control ? 'control allowed' : 'view only'})` : 'Browser node: disconnected'});
}

async function detach(tab) {
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
      if (!safeTab(tab) || new URL(tab.url).origin !== grant.origin) {
        shared.delete(id);
        await detach(id);
        continue;
      }
      if (shared.get(id) === grant) tabs.push({id, title: (tab.title || '').slice(0, 1024), url: tab.url.slice(0, 8192), control: grant.control});
    } catch { shared.delete(id); await detach(id); }
  }
  if (generation !== currentGeneration) return;
  await syncNativeGrants();
  if (generation !== currentGeneration) return;
  send({type: 'tabs', tabs, native_control: nativeReady});
  await badge();
}

function safeTab(tab) {
  if (tab.incognito || !NodePolicy.webURL(tab.url)) return false;
  // A controller must never read its own pairing UI, the dashboard's credential,
  // or a code printed in a localhost daemon page.
  const u = new URL(tab.url);
  return !['localhost', '127.0.0.1', '[::1]'].includes(u.hostname) &&
    (!registration || u.origin !== registration.server);
}

async function grantFor(command) {
  if (registration) await requireDaemon(registration.server);
  const grant = shared.get(command.tab);
  if (!registration || (registration.expires_at !== 0 && Date.now() >= registration.expires_at * 1000) ||
      !grant || !NodePolicy.allowed(command, grant.control)) throw new Error('Tab grant does not permit this action.');
  const tab = await api.tabs.get(command.tab);
  if (!safeTab(tab) || new URL(tab.url).origin !== grant.origin || shared.get(command.tab) !== grant) {
    throw new Error('Tab changed site or sharing ended.');
  }
  return grant;
}

async function cdp(tab, method, params = {}) {
  if (!attached.has(tab)) {
    await api.debugger.attach({tabId: tab}, '1.3');
    attached.add(tab);
    if (!shared.has(tab) || !registration) { await detach(tab); throw new Error('Sharing ended.'); }
  }
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
    // Changing origin revokes the grant via onUpdated; it is not inherited by
    // the destination. The local user can share that destination again.
    await api.tabs.update(tab, {url: params.url});
    return {ok: true};
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

async function handleCommand(command, connection, currentGeneration) {
  if (busy >= 16) return;
  busy++;
  try {
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
    connection.send(JSON.stringify({code, key, remember, name: name.trim(), browser: chromium ? 'chromium' : 'firefox'}));
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
    } else if (message.type === 'authorization_revoked') {
      await api.storage.local.remove('rememberedNode');
      await stop('Browser authorization was revoked — pair again to reconnect');
    } else if (message.type === 'heartbeat') lastAnswer = Date.now();
    else if (message.type === 'command' && registration) void handleCommand(message, connection, myGeneration);
    else void stop('Invalid daemon message');
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
    if (!pending || pending.expires <= Date.now()) throw new Error('Dashboard request expired. Click Pair browser extension again.');
    const {server} = await api.storage.local.get('server');
    if (server !== pending.server || !(await api.permissions.contains({origins: [permissionPattern(pending.server)]}))) throw new Error('Daemon configuration or permission changed. Request pairing again.');
    await api.storage.session.remove('dashboardPairing');
    await connect({server: pending.server, code: pending.code, name: message.name, remember: pending.remember && message.remember === true});
    return {ok: true};
  }
  if (message.type === 'rejectDashboard') {
    await api.storage.session.remove('dashboardPairing');
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
    const tabs = (await api.tabs.query({})).filter(safeTab).map(t => ({id: t.id, title: t.title || '', url: t.url, control: shared.get(t.id)?.control || false, shared: shared.has(t.id)}));
    const {dashboardPairing} = await api.storage.session.get('dashboardPairing');
    const {rememberedNode} = await api.storage.local.get('rememberedNode');
    return {allowlist, rememberedBrowser: rememberedNode ? {server: rememberedNode.server, name: rememberedNode.name, paused: rememberedNode.paused} : null, dashboardRequest: dashboardPairing?.expires > Date.now() ? {server: dashboardPairing.server, remember: dashboardPairing.remember} : null, status, history, connecting: !!pendingConnection || !!socket && !registration, registration, tabs, saved: await api.storage.local.get(['server', 'name', 'rememberPreference']), chromium, nativeReady};
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
      next.set(id, {origin: new URL(tab.url).origin, control: message.control === true});
    }
    shared = next;
    await Promise.all([...attached].filter(id => !shared.has(id)).map(detach));
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
api.tabs.onUpdated.addListener((id, change) => {
  if (shared.has(id) && (change.url || change.status === 'loading')) {
    // Drop immediately on navigation: no old document's control or response can
    // bleed into the new one, including same-origin navigations.
    shared.delete(id);
    void detach(id);
    publishSafely();
  }
});
if (chromium) api.debugger.onDetach.addListener(source => {
  // Clicking the browser's "Cancel debugging" bar revokes sharing, and is never
  // silently undone by the next remote request.
  attached.delete(source.tabId);
  shared.delete(source.tabId);
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

void historyReady.then(installDashboardBridge).catch(() => record('Dashboard integration unavailable — open extension setup to grant site access'));

api.alarms.onAlarm.addListener(alarm => { if (alarm.name === 'node-reconnect') void historyReady.then(() => restoreRemembered()); });
void historyReady.then(() => restoreRemembered());

api.storage.onChanged.addListener((changes, area) => {
  if (area !== 'managed' && !(area === 'local' && changes.daemonAllowlist)) return;
  void (async () => {
    const server = registration?.server || pendingConnection?.server || (socket && new URL(socket.url).origin.replace(/^ws/, 'http'));
    if (server) {
      try { await requireDaemon(server); }
      catch { await stop('Daemon is no longer on the allowlist'); }
    }
    await installDashboardBridge();
  })().catch(() => stop('Daemon allowlist could not be loaded'));
});
