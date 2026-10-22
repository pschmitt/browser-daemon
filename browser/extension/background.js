/* Local extension code only; dashboard bridge requires explicit daemon approval. */
if (typeof importScripts === 'function') importScripts('policy.js', 'audio.js', 'files-page.js', 'files.js');
const api = globalThis.browser || chrome;
const chromium = typeof api.runtime.getBrowserInfo !== 'function';
void api.storage.local.setAccessLevel?.({accessLevel: 'TRUSTED_CONTEXTS'});
let socket = null;
let registration = null;
let clientConnected = false;
let shared = new Map();
const attached = new Set();
const attaching = new Map();
// Live views (daemon relay channels) per tab, and Firefox's emulated screencasts.
const channels = new Map();
const casts = new Map();
const faviconCache = new Map();
const MAX_FAVICON_BYTES = 128 * 1024;
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
// follow: the tab a live view switches to comes to the front here as well.
let sharing = {all: true, autoShare: true, control: false, follow: false, audio: true, files: false, public: false};
let publicGrant = crypto.randomUUID();
const sharingReady = api.storage.local.get(['shareAll', 'autoShare', 'shareControl', 'followView', 'shareAudio', 'shareFiles', 'sharePublic']).then(async saved => {
  sharing = {all: saved.shareAll !== false, autoShare: saved.autoShare !== false, control: saved.shareControl === true, follow: saved.followView === true, audio: saved.shareAudio !== false, files: saved.shareFiles === true, public: saved.sharePublic === true};
  if (sharing.files && !(await api.permissions.contains({origins: chromium ? ['http://*/*', 'https://*/*'] : ['<all_urls>'],
      ...(!chromium ? {permissions: ['webRequest', 'webRequestBlocking', 'webRequestFilterResponse']} : {})}))) {
    sharing.files = false;
    await api.storage.local.set({shareFiles: false});
  }
});
// Controllers may open tabs where those are shared with control.
const canOpen = () => sharing.control && (sharing.all || sharing.autoShare);
async function shareEverything() {
  if (!registration || !sharing.all) return;
  for (const tab of await api.tabs.query({})) {
    if (!tab.incognito && !shared.has(tab.id)) shared.set(tab.id, {origin: null, control: sharing.control, paused: true});
  }
  if (!chromium) for (const [id, grant] of shared) if (grant.control && !grant.paused) void hookPasskeys(id).catch(() => hooked.delete(id));
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
let discoveryCache = null;
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
let nativeConnecting = null;
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
  const local = await api.storage.local.get(['daemonAllowlist', 'daemonAllowlistMode', 'daemonDiscoveryOrigin', 'managedAllowlistSeen']);
  let managed = {};
  try { managed = await api.storage.managed.get(['allowedDaemonOriginsMode', 'allowedDaemonOrigins', 'daemonDiscoveryOrigin']); } catch {
    if (local.managedAllowlistSeen) throw new Error('Managed daemon allowlist is unavailable. Connections are blocked until policy is restored.');
  }
  const hasManagedPolicy = Object.hasOwn(managed, 'allowedDaemonOriginsMode') ||
    Object.hasOwn(managed, 'allowedDaemonOrigins') || Object.hasOwn(managed, 'daemonDiscoveryOrigin');
  if (hasManagedPolicy) {
    if (!local.managedAllowlistSeen) await api.storage.local.set({managedAllowlistSeen: true});
    const mode = managed.allowedDaemonOriginsMode || 'custom';
    if (mode === 'auto') {
      const discoveryOrigin = NodePolicy.serverURL(managed.daemonDiscoveryOrigin || '');
      try { return {managed: true, mode, discoveryOrigin, origins: await discoverDaemonOrigins(discoveryOrigin), refreshedAt: discoveryCache.fetchedAt}; }
      catch (error) { return {managed: true, mode, discoveryOrigin, origins: [], error: `Could not load daemon origins from ${discoveryOrigin}: ${error.message}`}; }
    }
    if (mode !== 'custom') throw new Error('Managed daemon origin mode is invalid.');
    return {managed: true, mode, origins: NodePolicy.daemonOrigins(managed.allowedDaemonOrigins || [])};
  }
  if (local.managedAllowlistSeen) throw new Error('Managed daemon allowlist was removed. Connections are blocked until policy is restored.');
  const mode = local.daemonAllowlistMode || 'custom';
  if (mode === 'auto') {
    const discoveryOrigin = NodePolicy.serverURL(local.daemonDiscoveryOrigin || '');
    try { return {managed: false, mode, discoveryOrigin, origins: await discoverDaemonOrigins(discoveryOrigin), refreshedAt: discoveryCache.fetchedAt}; }
    catch (error) { return {managed: false, mode, discoveryOrigin, origins: [], error: `Could not load daemon origins from ${discoveryOrigin}: ${error.message}`}; }
  }
  return {managed: false, mode: 'custom', origins: NodePolicy.daemonOrigins(local.daemonAllowlist || [])};
}
async function discoverDaemonOrigins(discoveryOrigin) {
  if (discoveryCache?.origin === discoveryOrigin && discoveryCache.expires > Date.now()) {
    if (discoveryCache.error) throw new Error(discoveryCache.error);
    return discoveryCache.origins;
  }
  try {
    const response = await fetch(`${discoveryOrigin}/api/extension/config`, {cache: 'no-store', credentials: 'omit', redirect: 'error'});
    if (!response.ok) throw new Error(`server returned HTTP ${response.status}`);
    const result = await response.json();
    const origins = NodePolicy.daemonOrigins(result.origins);
    if (!origins.includes(discoveryOrigin)) throw new Error('the discovery server did not include itself in its origin list');
    discoveryCache = {origin: discoveryOrigin, origins, fetchedAt: Date.now(), expires: Number.POSITIVE_INFINITY};
    return origins;
  } catch (error) {
    discoveryCache = {origin: discoveryOrigin, error: error.message, expires: Date.now() + 30000};
    throw error;
  }
}
// Read once per change, not per request: live views send many small messages.
function cachedPolicy() {
  if (!policyCache) {
    policyCache = daemonPolicy().then(policy => {
      daemons = new Set(policy.origins);
      if (policy.error) policyCache = null;
      return policy;
    },
      error => { policyCache = null; throw error; });
  }
  return policyCache;
}
async function updateAutoOrigins(values) {
  if (!Array.isArray(values)) return;
  const policy = await cachedPolicy();
  if (policy.mode !== 'auto' || !policy.discoveryOrigin) return;
  const origins = NodePolicy.daemonOrigins(values).sort();
  if (!registration || !origins.includes(registration.server)) throw new Error('Daemon sent an invalid origin list.');
  if (discoveryCache?.origin === policy.discoveryOrigin && JSON.stringify(discoveryCache.origins) === JSON.stringify(origins)) return;
  discoveryCache = {origin: policy.discoveryOrigin, origins, fetchedAt: Date.now(), expires: Number.POSITIVE_INFINITY};
  policyCache = null;
  const updated = await cachedPolicy();
  if (!updated.origins.includes(registration.server)) {
    await stop('Daemon was removed from the automatic origin list');
    return;
  }
  await installDashboardBridge();
}
async function requireDaemon(server) {
  const policy = await cachedPolicy();
  if (policy.error) throw new Error(policy.error);
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
  if (nativeConnecting) return nativeConnecting;
  const connecting = new Promise((resolve, reject) => {
    const port = api.runtime.connectNative('dev.pschmitt.browser_daemon');
    nativePort = port;
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
      const detail = api.runtime.lastError?.message;
      reject(new Error(detail || 'Companion unavailable. Install it and restart Firefox with MOZ_REMOTE_ALLOW_SYSTEM_ACCESS=1 and --remote-debugging-port 9222.'));
      if (nativePort === port) {
        nativePort = null;
        nativeReady = false;
        publishSafely();
      }
    });
  });
  nativeConnecting = connecting;
  try { await connecting; }
  finally { if (nativeConnecting === connecting) nativeConnecting = null; }
}

async function connectNativeForRegistration() {
  if (chromium || !registration || nativeReady) return;
  try {
    await enableNative();
    if (!registration) return;
    await syncNativeGrants();
    publishSafely();
  } catch {
    // The extension session remains useful without trusted Firefox input.
    // The handoff page reports the missing companion and DOM input stays available.
    publishSafely();
  }
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
const connectionIcon = {bitmaps: null};
async function toolbarBitmaps() {
  const icon = api.runtime.getManifest().action?.default_icon;
  connectionIcon.bitmaps ||= Promise.all([16, 32, 48].map(async size => {
    const path = typeof icon === 'string' ? icon : icon?.[size] || icon?.[32] || 'icon.png';
    const image = await (await fetch(api.runtime.getURL(path))).blob();
    return [size, await createImageBitmap(image)];
  }));
  return connectionIcon.bitmaps;
}
function connectionFrame(bitmap, size, state) {
  const canvas = new OffscreenCanvas(size, size);
  const ctx = canvas.getContext('2d');
  ctx.drawImage(bitmap, 0, 0, size, size);
  const r = size * 0.2, cx = size - r - 0.5, cy = r + 0.5;
  ctx.fillStyle = state === 'offline' ? '#e11d48' : '#3b82f6';
  ctx.beginPath(); ctx.arc(cx, cy, r, 0, Math.PI * 2); ctx.fill();
  return ctx.getImageData(0, 0, size, size);
}
async function drawConnectionIcon(state) {
  try {
    const bitmaps = await toolbarBitmaps();
    if (!registration && state !== 'offline') return;
    if (registration && state === 'offline') return;
    await api.action.setIcon({imageData: Object.fromEntries(bitmaps.map(([size, bitmap]) =>
      [size, connectionFrame(bitmap, size, state)]))});
  } catch { /* Cosmetic only. */ }
}
// The top-right icon mark follows rbw's canvas-drawn toolbar badge. A "?"
// remains the approval prompt when a dashboard is waiting to pair.
async function badge() {
  if (approvalPending > Date.now() && !registration) {
    await api.action.setIcon({path: api.runtime.getManifest().action?.default_icon || 'icon.png'});
    await api.action.setBadgeText({text: '?'});
    await api.action.setBadgeBackgroundColor({color: '#6d4aff'});
    await api.action.setBadgeTextColor({color: '#ffffff'});
    await api.action.setTitle({title: 'Browser node: a dashboard asks to connect — click to answer'});
    return;
  }
  const connected = !!registration && clientConnected;
  await api.action.setBadgeText({text: ''});
  await api.action.setBadgeBackgroundColor({color: [0, 0, 0, 0]});
  await api.action.setBadgeTextColor({color: '#e11d48'});
  if (!registration) {
    await drawConnectionIcon('offline');
  } else if (connected) {
    await drawConnectionIcon('connected');
  } else {
    await api.action.setIcon({path: api.runtime.getManifest().action?.default_icon || 'icon.png'});
  }
  await api.action.setTitle({title: !registration ? 'Browser node: not connected to the daemon'
    : connected ? 'Browser node: connected — a client is using this browser'
      : 'Browser node: connected — waiting for a client'});
}

async function detach(tab) {
  stopTabAudio(tab);
  stopFileTab(tab);
  for (const [channel, owner] of channels) if (owner === tab) { channels.delete(channel); stopCast(channel); }
  hooked.delete(tab);
  if (!attached.delete(tab)) return;
  try { await api.debugger.detach({tabId: tab}); } catch { /* Already closed/detached. */ }
}

async function stop(reason = 'Disconnected', retry = false) {
  generation++;
  const clearedAlarm = api.alarms.clear('node-reconnect');
  status = reason;
  record(reason);
  clientConnected = false;
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
  for (const tab of mediaAudio.keys()) stopTabAudio(tab);
  for (const tab of fileTabs.keys()) stopFileTab(tab);
  disconnectNative();
  if (old) old.close();
  await clearedAlarm;
  await Promise.all([...attached].map(detach));
  await badge();
  if (retry) {
    const {rememberedNode} = await api.storage.local.get('rememberedNode');
    if (rememberedNode && !rememberedNode.paused) {
      await api.alarms.create('node-reconnect', {delayInMinutes: 1});
    }
  }
}

function send(value) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(value));
}

function faviconURL(value) {
  if (typeof value !== 'string') return '';
  if (value.length <= 176 * 1024 && /^data:image\/(?:png|jpeg|gif|x-icon|webp);base64,[A-Za-z0-9+/]+={0,2}$/.test(value)) return value;
  const normalized = value.startsWith('moz-anno:favicon:') ? value.slice('moz-anno:favicon:'.length) : value;
  return /^https?:\/\//i.test(normalized) ? normalized.slice(0, 2048) : '';
}

function faviconDataURL(bytes) {
  const starts = (...values) => values.every((value, index) => bytes[index] === value);
  let mime = '';
  if (starts(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)) mime = 'image/png';
  else if (starts(0xff, 0xd8, 0xff)) mime = 'image/jpeg';
  else if (starts(0x47, 0x49, 0x46, 0x38)) mime = 'image/gif';
  else if (starts(0x00, 0x00, 0x01, 0x00)) mime = 'image/x-icon';
  else if (bytes.length >= 12 && String.fromCharCode(...bytes.subarray(0, 4)) === 'RIFF' &&
      String.fromCharCode(...bytes.subarray(8, 12)) === 'WEBP') mime = 'image/webp';
  if (!mime) return '';
  let binary = '';
  for (let i = 0; i < bytes.length; i += 8192) binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
  return `data:${mime};base64,${btoa(binary)}`;
}

async function tabFavicon(value) {
  const url = faviconURL(value);
  if (!url) return '';
  if (url.startsWith('data:')) return url;
  if (faviconCache.has(url)) return faviconCache.get(url);
  const pending = (async () => {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 4000);
    try {
      const response = await fetch(url, {credentials: 'omit', cache: 'force-cache', redirect: 'follow', signal: controller.signal});
      if (!response.ok || !response.body) return url;
      const length = Number(response.headers.get('content-length'));
      if (Number.isFinite(length) && length > MAX_FAVICON_BYTES) return url;
      const reader = response.body.getReader();
      const chunks = [];
      let size = 0;
      while (true) {
        const {done, value: chunk} = await reader.read();
        if (done) break;
        size += chunk.length;
        if (size > MAX_FAVICON_BYTES) { await reader.cancel(); return url; }
        chunks.push(chunk);
      }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
      return faviconDataURL(bytes) || url;
    } catch { return url; }
    finally { clearTimeout(timeout); }
  })();
  faviconCache.set(url, pending);
  if (faviconCache.size > 512) faviconCache.delete(faviconCache.keys().next().value);
  const result = await pending;
  faviconCache.set(url, result);
  return result;
}

async function publish() {
  const currentGeneration = generation;
  if (!registration) return;
  await requireDaemon(registration.server);
  if (generation !== currentGeneration) return;
  const tabs = [];
  const faviconUpdates = [];
  const inlineFavicons = [];
  for (const [id, grant] of shared) {
    try {
      const tab = await api.tabs.get(id);
      // Sharing belongs to the tab and follows it across sites. While it shows a
      // page that must never be shared (a daemon, loopback, browser pages) it is
      // paused: unlisted, and nothing is read from it or sent to it.
      grant.paused = !safeTab(tab);
      if (grant.paused) continue;
      grant.origin = new URL(tab.url).origin;
      if (!chromium && grant.control) void hookPasskeys(id).catch(() => hooked.delete(id));
      if (shared.get(id) === grant) {
        const rawFavicon = faviconURL(tab.favIconUrl);
        const snapshot = {id, title: (tab.title || '').slice(0, 1024), url: tab.url.slice(0, 8192), favicon: rawFavicon.startsWith('data:') ? '' : rawFavicon, control: grant.control, active: tab.active === true, index: tab.index, window_id: tab.windowId, pinned: tab.pinned === true};
        tabs.push(snapshot);
        faviconUpdates.push(tabFavicon(tab.favIconUrl).then(favicon => {
          if (favicon.startsWith('data:')) inlineFavicons.push({id, favicon});
        }));
      }
    } catch { shared.delete(id); await detach(id); }
  }
  await Promise.all(faviconUpdates);
  if (generation !== currentGeneration) return;
  await syncNativeGrants();
  if (generation !== currentGeneration) return;
  published = tabs;
  for (const tab of mediaAudio.keys()) if (!tabs.some(t => t.id === tab)) stopTabAudio(tab);
  for (const tab of fileTabs.keys()) if (!tabs.some(t => t.id === tab && t.control)) stopFileTab(tab);
  send({type: 'tabs', tabs, native_control: nativeReady, can_open: canOpen(), follow: sharing.follow, audio: sharing.audio, files: sharing.files, public_grant: sharing.public ? publicGrant : null});
  for (let i = 0; i < inlineFavicons.length; i += 4) send({type: 'favicons', favicons: inlineFavicons.slice(i, i + 4)});
  await badge();
}

function safeURL(url) {
  if (!NodePolicy.pageURL(url)) return false;
  // A controller must never read its own pairing UI, a dashboard's credential,
  // or a code printed in a localhost daemon page.
  const u = new URL(url);
  return !['localhost', '127.0.0.1', '[::1]'].includes(u.hostname) &&
    !daemons.has(u.origin) && (!registration || u.origin !== registration.server);
}
function safeTab(tab) { return !tab.incognito && safeURL(tab.url); }

async function wakeDiscardedTab(id, tab) {
  if (chromium || !tab.discarded) return;
  const previous = sharing.follow ? null
    : (await api.tabs.query({active: true, windowId: tab.windowId}))[0]?.id ?? null;
  let finish;
  let timer;
  let cleanup = () => {};
  let settled = false;
  const loaded = new Promise((resolve, reject) => {
    finish = error => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      error ? reject(error) : resolve();
    };
    const onUpdated = (updatedId, change) => {
      if (updatedId !== id || change.status !== 'complete') return;
      void api.tabs.get(id).then(current => {
        if (!current.discarded) finish();
      }, finish);
    };
    api.tabs.onUpdated.addListener(onUpdated);
    timer = setTimeout(() => finish(new Error('Firefox did not load the discarded tab.')), 30000);
    void api.tabs.get(id).then(current => {
      if (!current.discarded && current.status === 'complete') finish();
    }, finish);
    cleanup = () => api.tabs.onUpdated.removeListener(onUpdated);
  });
  try {
    await api.tabs.update(id, {active: true});
    const current = await api.tabs.get(id);
    if (!current.discarded && current.status === 'complete') finish();
    await loaded;
    if (previous !== null && previous !== id) await api.tabs.update(previous, {active: true});
  } finally {
    clearTimeout(timer);
    cleanup();
  }
}

async function grantFor(command) {
  if (command.public_grant && (!sharing.public || command.public_grant !== publicGrant)) throw new Error("Public sharing ended.");
  if (registration) await requireDaemon(registration.server);
  const grant = shared.get(command.tab);
  if (!registration || (registration.expires_at !== 0 && Date.now() >= registration.expires_at * 1000) ||
      !grant || !NodePolicy.allowed(command, grant.control)) throw new Error('Tab grant does not permit this action.');
  const tab = await api.tabs.get(command.tab);
  if (!safeTab(tab) || shared.get(command.tab) !== grant) throw new Error('Tab is paused or sharing ended.');
  if (!['move', 'pin', 'close', 'suggest'].includes(command.action)) await wakeDiscardedTab(command.tab, tab);
  const current = await api.tabs.get(command.tab);
  if (!safeTab(current) || shared.get(command.tab) !== grant || grant.paused) throw new Error('Tab is paused or sharing ended.');
  grant.origin = new URL(current.url).origin;
  return grant;
}

async function addressSuggestions(query) {
  const saved = await api.storage.local.get('addressSuggestions');
  if (saved.addressSuggestions !== true) return [];
  const text = query.trim().slice(0, 200);
  const [bookmarks, history] = await Promise.all([
    api.permissions.contains({permissions: ['bookmarks']})
      .then(allowed => allowed ? api.bookmarks.search(text) : []).catch(() => []),
    api.permissions.contains({permissions: ['history']})
      .then(allowed => allowed ? api.history.search({text, startTime: 0, maxResults: 12}) : []).catch(() => []),
  ]);
  const seen = new Set(), items = [];
  for (const [records, kind] of [[bookmarks, 'bookmark'], [history, 'history']]) {
    for (const item of records) {
      if (!item.url || !safeURL(item.url) || seen.has(item.url)) continue;
      seen.add(item.url);
      items.push({title: item.title || '', url: item.url, kind});
      if (items.length === 8) return items;
    }
  }
  return items;
}

// The page asks a live view with control to use the vault while it is watching
// (__bdPasskeyAlive stays fresh); otherwise the browser handles the request.
function passkeyHook() {
  const c = navigator.credentials;
  if (!c || globalThis.__bdPasskeyRepair) return;
  const waiting = new Map();
  let current;
  const b64url = (buf) => btoa(String.fromCharCode(...new Uint8Array(buf.buffer ? buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength) : buf)))
    .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  const bytes = (s) => { s = s.replace(/-/g, "+").replace(/_/g, "/"); s += "===".slice((s.length + 3) % 4); return Uint8Array.from(atob(s), (ch) => ch.charCodeAt(0)).buffer; };
  // A signed assertion (from the vault, signed outside the browser) as the
  // PublicKeyCredential the page asked for.
  const credential = (a) => {
    const r = a.response, own = (o, props) => { for (const [k, v] of Object.entries(props)) Object.defineProperty(o, k, { value: v, enumerable: true }); return o; };
    const response = own(Object.create(AuthenticatorAssertionResponse.prototype), {
      clientDataJSON: bytes(r.clientDataJSON), authenticatorData: bytes(r.authenticatorData),
      signature: bytes(r.signature), userHandle: r.userHandle ? bytes(r.userHandle) : null,
    });
    const cred = own(Object.create(PublicKeyCredential.prototype), {
      id: a.id, rawId: bytes(a.rawId), type: "public-key", response, authenticatorAttachment: a.authenticatorAttachment || "platform",
    });
    Object.defineProperty(cred, "getClientExtensionResults", { value: () => ({}) });
    Object.defineProperty(cred, "toJSON", { value: () => ({ id: a.id, rawId: a.id, type: "public-key", authenticatorAttachment: a.authenticatorAttachment || "platform", response: r, clientExtensionResults: {} }) });
    return cred;
  };
  // answer: true = let the browser handle it (its authenticator has the passkey now),
  // false = cancelled, an object = the signed assertion to resolve with.
  globalThis.__bdPasskeyAnswer = (id, answer = true) => { const go = waiting.get(id); waiting.delete(id); go?.(answer); };
  const viewed = () => Date.now() - (globalThis.__bdPasskeyAlive || 0) < 8000;
  // Extensions such as rbw install their own handler after document creation.
  // Refresh from the live-view heartbeat, wrapping their handler as our fallback.
  const repair = () => {
    if (c.get === current) return;
    const get = c.get.bind(c);
    const hook = function (opts) {
      // A later extension may have captured an older wrapper. Bypass that wrapper
      // when falling back, so a request is never offered to the view twice.
      if (hook !== current) return get(opts);
      const pk = opts && opts.publicKey;
      // A conditional request (the passkey among a login field's suggestions) only where
      // the view offers it itself (__bdPasskeyOffer: a shared browser, whose suggestions
      // are not in the picture); elsewhere the browser shows them.
      const conditional = opts?.mediation === "conditional";
      if (!pk || (conditional && !globalThis.__bdPasskeyOffer) || !viewed() || typeof globalThis.__bdPasskey !== "function") return get(opts);
      const id = Math.random().toString(36).slice(2);
      let req;
      try {
        req = { id, rpId: pk.rpId || location.hostname, challenge: b64url(pk.challenge), allow: (pk.allowCredentials || []).map((x) => b64url(x.id)), conditional };
      } catch { return get(opts); }
      return new Promise((go, fail) => {
        waiting.set(id, go);
        setTimeout(() => globalThis.__bdPasskeyAnswer(id), 330000);
        // The page gave up (e.g. a conditional request it replaces): so does the view.
        opts.signal?.addEventListener("abort", () => {
          if (!waiting.delete(id)) return;
          try { globalThis.__bdPasskey(JSON.stringify({ id, cancel: true })); } catch {}
          fail(opts.signal.reason ?? new DOMException("The operation was aborted.", "AbortError"));
        }, { once: true });
        // A held conditional request goes back to the browser once nobody watches.
        if (conditional) {
          const check = setInterval(() => {
            if (!waiting.has(id)) clearInterval(check);
            else if (!viewed()) { clearInterval(check); globalThis.__bdPasskeyAnswer(id); }
          }, 4000);
        }
        try { globalThis.__bdPasskey(JSON.stringify(req)); } catch { globalThis.__bdPasskeyAnswer(id); }
      }).then((answer) => answer === true ? get(opts)
        : answer && typeof answer === "object" ? credential(answer)
        : Promise.reject(new DOMException("The operation either timed out or was not allowed.", "NotAllowedError")));
    };
    Object.defineProperty(c, "get", { configurable: true, writable: true, value: hook });
    current = hook;
  };
  globalThis.__bdPasskeyRepair = repair;
  repair();
}
const PASSKEY_HOOK = `(${passkeyHook.toString()})();`;
// Only tabs shared with control; reinjected after each top-level navigation.
const hooked = new Set();
async function hookPasskeys(tab) {
  if (hooked.has(tab) || !shared.get(tab)?.control || shared.get(tab)?.paused) return;
  hooked.add(tab);
  if (!chromium) {
    try {
      await api.scripting.executeScript({target: {tabId: tab}, world: 'MAIN', func: () => {
        if (globalThis.__bdExtensionPasskeyBridge) return;
        globalThis.__bdExtensionPasskeyBridge = true;
        globalThis.__bdPasskey = payload => window.dispatchEvent(new CustomEvent('browser-daemon-passkey-request', {detail: payload}));
        window.addEventListener('browser-daemon-passkey-answer', event => {
          globalThis.__bdPasskeyAnswer?.(event.detail.id, event.detail.answer);
        });
      }});
      await api.scripting.executeScript({target: {tabId: tab}, func: () => {
        if (globalThis.__bdExtensionPasskeyRelay) return;
        globalThis.__bdExtensionPasskeyRelay = true;
        window.addEventListener('browser-daemon-passkey-request', event => {
          void (globalThis.browser || globalThis.chrome).runtime.sendMessage({type: 'passkeyHook', payload: event.detail});
        });
      }});
      await api.scripting.executeScript({target: {tabId: tab}, world: 'MAIN', func: passkeyHook});
    } catch (error) {
      hooked.delete(tab);
      throw error;
    }
    return;
  }
  if (!attached.has(tab)) { hooked.delete(tab); return; }
  const send = (method, params = {}) => api.debugger.sendCommand({tabId: tab}, method, params);
  await send('Runtime.enable');
  await send('Runtime.addBinding', {name: '__bdPasskey'});
  await send('Page.addScriptToEvaluateOnNewDocument', {source: PASSKEY_HOOK});
  await send('Runtime.evaluate', {expression: PASSKEY_HOOK}).catch(() => {});
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
        await hookPasskeys(tab).catch(() => hooked.delete(tab));
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
  const hit = (x, y) => {
    let target = document.elementFromPoint(x, y);
    // Keep hit testing working through nested open shadow roots (common in SPAs).
    while (target?.shadowRoot) {
      const inner = target.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === target) break;
      target = inner;
    }
    return target;
  };
  if (action === 'snapshot') {
    return {title: document.title, url: location.href,
      text: (document.body?.innerText || '').slice(0, 100000),
      elements: [...document.querySelectorAll('a,button,input,textarea,select,[role="button"]')].slice(0, 400).map(e => ({
        tag: e.tagName, role: e.getAttribute('role'), name: e.getAttribute('aria-label') || e.innerText?.slice(0, 160) || e.getAttribute('placeholder') || '',
        type: e.getAttribute('type'), rect: (() => { const r = e.getBoundingClientRect(); return {x: r.x, y: r.y, width: r.width, height: r.height}; })(),
      }))};
  }
  if (action === 'scroll') {
    if (finite(params.deltaX) && finite(params.deltaY) && finite(params.x) && finite(params.y)) {
      const target = hit(params.x, params.y);
      const modifiers = params.modifiers || 0;
      const wheel = new WheelEvent('wheel', {bubbles: true, cancelable: true, composed: true, view: window,
        clientX: params.x, clientY: params.y, deltaX: params.deltaX, deltaY: params.deltaY,
        altKey: !!(modifiers & 1), ctrlKey: !!(modifiers & 2), metaKey: !!(modifiers & 4), shiftKey: !!(modifiers & 8)});
      const allowed = (target || document).dispatchEvent(wheel);
      if (allowed) {
        let scroller = target;
        while (scroller && scroller !== document.documentElement) {
          const style = getComputedStyle(scroller);
          const canX = params.deltaX < 0 ? scroller.scrollLeft > 0 : scroller.scrollLeft + scroller.clientWidth < scroller.scrollWidth;
          const canY = params.deltaY < 0 ? scroller.scrollTop > 0 : scroller.scrollTop + scroller.clientHeight < scroller.scrollHeight;
          if ((params.deltaX && canX && /(auto|scroll|overlay)/.test(style.overflowX)) ||
              (params.deltaY && canY && /(auto|scroll|overlay)/.test(style.overflowY))) break;
          scroller = scroller.parentElement || scroller.getRootNode()?.host || null;
        }
        (scroller && scroller !== document.documentElement ? scroller : window).scrollBy(params.deltaX, params.deltaY);
      }
    } else {
      if (!finite(params.x) || !finite(params.y)) throw new Error('Expected scroll offsets.');
      window.scrollBy(params.x, params.y);
    }
  } else if (action === 'move') {
    if (!finite(params.x) || !finite(params.y)) throw new Error('Expected pointer coordinates.');
    const target = hit(params.x, params.y);
    if (target) {
      const init = {bubbles: true, cancelable: true, composed: true, view: window,
        clientX: params.x, clientY: params.y, button: -1, buttons: 0};
      if (typeof PointerEvent === 'function') target.dispatchEvent(new PointerEvent('pointermove', {
        ...init, pointerId: 1, pointerType: 'mouse', isPrimary: true}));
      target.dispatchEvent(new MouseEvent('mousemove', init));
    }
  } else if (action === 'click') {
    const button = Number.isInteger(params.button) && params.button >= 0 && params.button <= 2 ? params.button : 0;
    const mask = button === 0 ? 1 : button === 1 ? 4 : 2;
    let target = params.selector ? document.querySelector(params.selector)
      : finite(params.x) && finite(params.y) ? hit(params.x, params.y) : null;
    if (!target) throw new Error('Element not found.');
    const rect = target.getBoundingClientRect();
    const start = {x: finite(params.x) ? params.x : rect.left + rect.width / 2,
      y: finite(params.y) ? params.y : rect.top + rect.height / 2};
    const path = Array.isArray(params.path) ? params.path.filter(p => finite(p?.x) && finite(p?.y)).slice(0, 128) : [];
    const end = path.at(-1) || start;
    const dragged = Math.hypot(end.x - start.x, end.y - start.y) > 2;
    const clickCount = Number.isInteger(params.clickCount) ? Math.max(1, Math.min(3, params.clickCount)) : 1;
    const modifiers = params.modifiers || 0;
    const selectWord = () => {
      const position = document.caretPositionFromPoint?.(end.x, end.y);
      const range = position ? null : document.caretRangeFromPoint?.(end.x, end.y);
      const node = position?.offsetNode || range?.startContainer;
      const offset = position?.offset ?? range?.startOffset;
      const field = node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement ? node : null;
      const text = field ? field.value : node?.nodeType === Node.TEXT_NODE ? node.data : null;
      if (typeof text !== 'string' || !Number.isInteger(offset)) return;
      const words = typeof Intl.Segmenter === 'function'
        ? [...new Intl.Segmenter(undefined, {granularity: 'word'}).segment(text)].filter(part => part.isWordLike)
        : [];
      const word = words.find(part => offset >= part.index && offset <= part.index + part.segment.length);
      if (!word) return;
      const from = word.index, to = from + word.segment.length;
      if (field) { field.focus(); field.setSelectionRange(from, to); }
      else {
        const selection = window.getSelection();
        if (selection?.setBaseAndExtent) selection.setBaseAndExtent(node, from, node, to);
        else if (selection) {
          const wordRange = document.createRange();
          wordRange.setStart(node, from); wordRange.setEnd(node, to);
          selection.removeAllRanges(); selection.addRange(wordRange);
        }
      }
    };
    const event = (type, point, buttons, detail = 0) => {
      const current = hit(point.x, point.y) || target;
      const init = {bubbles: true, cancelable: true, composed: true, view: window, detail,
        screenX: point.x + window.screenX, screenY: point.y + window.screenY,
        clientX: point.x, clientY: point.y, button, buttons,
        altKey: !!(modifiers & 1), ctrlKey: !!(modifiers & 2), metaKey: !!(modifiers & 4), shiftKey: !!(modifiers & 8)};
      if (type.startsWith('pointer') && typeof PointerEvent === 'function') {
        current.dispatchEvent(new PointerEvent(type, {...init, pointerId: 1, pointerType: 'mouse', isPrimary: true}));
      } else current.dispatchEvent(new MouseEvent(type, init));
      return current;
    };
    if (button === 0 && (!dragged || target.matches('input,textarea,[contenteditable="true"]'))) target.focus();
    event('pointerover', start, 0);
    event('mouseover', start, 0);
    event('pointerdown', start, mask, 1);
    event('mousedown', start, mask, 1);
    for (const point of path) {
      event('pointermove', point, mask);
      event('mousemove', point, mask, 0);
    }
    target = event('pointerup', end, 0);
    target = event('mouseup', end, 0);
    if (button === 0 && dragged) {
      // Synthetic mouse events cannot ask Firefox to select text. Create the
      // equivalent DOM selection at the drag endpoints for normal page text.
      const caret = (point) => {
        const position = document.caretPositionFromPoint?.(point.x, point.y);
        if (position) return {node: position.offsetNode, offset: position.offset};
        const range = document.caretRangeFromPoint?.(point.x, point.y);
        return range ? {node: range.startContainer, offset: range.startOffset} : null;
      };
      const first = caret(start), last = caret(end), selection = window.getSelection();
      if (first && last && (first.node instanceof HTMLInputElement || first.node instanceof HTMLTextAreaElement)) {
        const field = first.node;
        if (field === last.node && typeof field.setSelectionRange === 'function') {
          field.focus();
          field.setSelectionRange(first.offset, last.offset);
        }
      } else if (first && last && selection) {
        selection.removeAllRanges();
        if (selection.setBaseAndExtent) selection.setBaseAndExtent(first.node, first.offset, last.node, last.offset);
        else { selection.collapse(first.node, first.offset); selection.extend(last.node, last.offset); }
      }
    }
    if (button === 0 && !dragged) {
      for (let count = 1; count <= clickCount; count++) {
        if (count > 1) {
          event('pointerdown', end, mask, 1);
          event('mousedown', end, mask, count);
          event('pointerup', end, 0, 1);
          event('mouseup', end, 0, count);
        }
        target.dispatchEvent(new MouseEvent('click', {
          bubbles: true, cancelable: true, composed: true, view: window, detail: count,
          screenX: end.x + window.screenX, screenY: end.y + window.screenY,
          clientX: end.x, clientY: end.y, button: 0, buttons: 0,
          altKey: !!(modifiers & 1), ctrlKey: !!(modifiers & 2), metaKey: !!(modifiers & 4), shiftKey: !!(modifiers & 8),
        }));
        if (count === 2) target.dispatchEvent(new MouseEvent('dblclick', {
          bubbles: true, cancelable: true, composed: true, view: window, detail: 2,
          clientX: end.x, clientY: end.y, button: 0, buttons: 0,
        }));
      }
      if (clickCount >= 2) selectWord();
    } else if (button === 1 && !dragged) target.dispatchEvent(new MouseEvent('auxclick', {
      bubbles: true, cancelable: true, composed: true, view: window, detail: clickCount,
      clientX: end.x, clientY: end.y, button: 1, buttons: 0,
    }));
    else if (button === 2 && !dragged) {
      target.dispatchEvent(new MouseEvent('contextmenu', {
        bubbles: true, cancelable: true, composed: true, view: window, detail: 1,
        screenX: end.x + window.screenX, screenY: end.y + window.screenY,
        clientX: end.x, clientY: end.y, button: 2, buttons: 0,
        altKey: !!(modifiers & 1), ctrlKey: !!(modifiers & 2), metaKey: !!(modifiers & 4), shiftKey: !!(modifiers & 8),
      }));
      target.dispatchEvent(new MouseEvent('auxclick', {
        bubbles: true, cancelable: true, composed: true, view: window, detail: clickCount,
        clientX: end.x, clientY: end.y, button: 2, buttons: 0,
      }));
    }
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
      const value = e.value;
      let start = e.selectionStart;
      let end = e.selectionEnd;
      if (!Number.isInteger(start) || !Number.isInteger(end)) start = end = value.length;
      setter.call(e, value.slice(0, start) + params.text + value.slice(end));
      try { e.setSelectionRange(start + params.text.length, start + params.text.length); } catch {}
      e.dispatchEvent(new InputEvent('input', {bubbles: true, composed: true, inputType: 'insertText', data: params.text}));
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

// The vault login fill of src/autofill.rs (FILL with GUARD), run in the tab's
// isolated world: the same fields as in a daemon's own sessions.
// Only in a frame still at an address the daemon checked (`urls`).
const fillLogin = (c, urls) => {
  if (!urls.includes(location.href)) return null;
  const inputs = [], areas = [];
  const walk = (root) => root.querySelectorAll("*").forEach((el) => {
    if (el.tagName === "INPUT") inputs.push(el);
    if (el.tagName === "TEXTAREA") areas.push(el);
    if (el.shadowRoot) walk(el.shadowRoot);
  });
  walk(document);
  const type = (el) => (el.getAttribute("type") || "").toLowerCase();
  const ac = (el) => (el.getAttribute("autocomplete") || "").toLowerCase();
  const hint = (el) => [el.name, el.id, el.getAttribute("placeholder"), el.getAttribute("aria-label"), ac(el)].join(" ").toLowerCase();
  const shown = (el) => {
    if (el.disabled || el.readOnly) return false;
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) return false;
    const s = getComputedStyle(el);
    return s.visibility !== "hidden" && s.display !== "none" && Number(s.opacity) > 0;
  };
  const textTypes = new Set(["", "text", "email", "tel", "number", "url"]);
  const visible = inputs.filter(shown);
  const pw = visible.filter((el) => type(el) === "password");
  const texty = visible.filter((el) => textTypes.has(type(el)));
  const before = (a, b) => !!(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
  const areaSetter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value").set;
  const used = new Set();
  const guard = (el, css) => {
  el.setAttribute("data-bd-secret", "");
  const root = el.getRootNode();
  const sheets = root.adoptedStyleSheets;
  try {
    if (sheets.some((s) => s.cssRules[0] && s.cssRules[0].selectorText === "[data-bd-secret]")) return;
    const s = new CSSStyleSheet();
    s.replaceSync(css);
    root.adoptedStyleSheets = [...sheets, s];
  } catch (e) {
    const st = document.createElement("style");
    st.textContent = css;
    (root.head || root).appendChild(st);
  }
};
  const put = (el, v, mask = true) => {
    if (mask) guard(el, c.css);
    used.add(el);
    el.focus();
    (el.tagName === "TEXTAREA" ? areaSetter : setter).call(el, v);
    el.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, inputType: "insertReplacementText", data: v }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  };
  const userLike = (el) => /username|email/.test(ac(el));
  const filled = [];
  let otp = [];
  if (c.totp) {
    const boxes = texty.filter((el) => el.maxLength === 1);
    const otpHint = /one.?time|otp|2fa|mfa|two.?factor|verification|security.?code|auth.?code|passcode|token/;
    const one = texty.find((el) => ac(el) === "one-time-code") || (!pw.length && texty.find((el) => otpHint.test(hint(el))));
    if (one) otp = [one];
    else if (boxes.length === c.totp.length) otp = boxes;
  }
  if (c.password && pw.length) {
    const p = pw.find((el) => ac(el) !== "new-password") || pw[0];
    if (c.username) {
      const prior = texty.filter((el) => (!p.form || el.form === p.form) && before(el, p) && !otp.includes(el));
      const u = prior.findLast(userLike) || prior.at(-1);
      if (u) { put(u, c.username); filled.push("username"); }
    }
    put(p, c.password);
    filled.push("password");
  } else if (c.username && !otp.length) {
    const u = texty.find(userLike) || texty.find((el) => type(el) === "email")
      || texty.find((el) => /user|login|mail|account|identifier|benutzer|kennung|anmelde/.test(hint(el)));
    if (u) { put(u, c.username); filled.push("username"); }
  }
  if (otp.length === 1) put(otp[0], c.totp);
  else otp.forEach((el, i) => put(el, c.totp[i]));
  if (otp.length) filled.push("totp");
  // Custom fields (text ones only; hidden, boolean and linked never come here), as
  // rbw's extension: an input whose id or name is the field's name, else one not
  // yet filled whose id, name, autocomplete, aria-label, placeholder or label
  // text matches it.
  const norm = (t) => String(t || "").toLowerCase().replace(/[^\p{L}\p{N}]/gu, "");
  const labelText = (el) => {
    const ids = el.getAttribute("aria-labelledby");
    if (ids) return ids.split(/\s+/).map((i) => el.getRootNode().getElementById?.(i)?.textContent).filter(Boolean).join(" ");
    if (el.labels?.length) return [...el.labels].map((l) => l.textContent).join(" ");
    return "";
  };
  const keys = (el) => [el.id, el.name, el.getAttribute("autocomplete"), el.getAttribute("aria-label"), el.getAttribute("placeholder"), labelText(el)].map(norm).filter(Boolean);
  const targets = [...texty, ...areas.filter(shown)];
  let customs = 0;
  for (const f of c.custom || []) {
    if (!f.name || !f.value) continue;
    const el = targets.find((e) => e.id === f.name || e.name === f.name)
      || targets.find((e) => !used.has(e) && keys(e).includes(norm(f.name)));
    if (el) { put(el, f.value, false); customs++; }
  }
  if (customs) filled.push("custom");
  return { filled };
};

async function execute(command) {
  const grant = await grantFor(command);
  const {tab, action, params} = command;
  if (action === 'suggest') return addressSuggestions(params.query);
  if (action === 'publicAudio') {
    if (!command.public_grant || !sharing.audio) throw new Error('Audio sharing is disabled.');
    let state = mediaAudio.get(tab);
    if (!state?.channels.has('public')) { await startTabAudio(tab, 'public'); state = mediaAudio.get(tab); }
    state.publicUntil = Date.now() + 10000;
    const since = Number.isSafeInteger(params.since) ? params.since : 0;
    const chunks = (state.publicRing || []).filter(c => c.seq > since);
    return {chunks, cursor: state.publicSeq || 0, generation: state.publicGeneration ||= crypto.randomUUID()};
  }
  if (action === 'publicInput') {
    const allowed = ['click', 'scroll', 'key', 'text'];
    if (!command.public_grant || !allowed.includes(params.type)) throw new Error('Invalid public input.');
    let actual = params.type === 'text' ? 'type' : params.type;
    let values = {...params};
    if (params.type === 'click' || params.type === 'scroll') {
      let width, height;
      if (chromium) {
        const m = (await cdp(tab, 'Page.getLayoutMetrics')).cssVisualViewport;
        width = m.clientWidth; height = m.clientHeight;
      } else {
        const t = await api.tabs.get(tab); width = t.width; height = t.height;
      }
      values.x = Math.max(0, Math.min(1, params.x)) * width;
      values.y = Math.max(0, Math.min(1, params.y)) * height;
      if (params.type === 'scroll') {
        if (chromium) return cdp(tab, 'Input.dispatchMouseEvent', {type: 'mouseWheel', x: values.x, y: values.y,
          deltaX: Math.max(-3000, Math.min(3000, params.dx)), deltaY: Math.max(-3000, Math.min(3000, params.dy))});
        values = {x: params.dx, y: params.dy};
      }
    }
    await grantFor(command);
    return execute({...command, action: actual, params: values});
  }
  if (!chromium && nativeReady && ['screenshot', 'snapshot', 'click', 'move', 'type', 'key', 'scroll', 'evaluate'].includes(action)) {
    return nativeRequest({tab, action, params});
  }
  if (action === 'evaluate') throw new Error('Evaluation requires the Firefox companion; use CDP Runtime.evaluate on Chromium.');
  // Vault logins (picked in a live view, sent by the daemon): the frames first, so it
  // can keep to the entry's sites; then the fill, in those frames if still there.
  // Chromium: through the debugger (no host permission needed), in an isolated world per frame.
  if (action === 'frames') {
    if (chromium) {
      const out = [];
      const walk = node => { out.push({frame: node.frame.id, url: node.frame.url}); (node.childFrames || []).forEach(walk); };
      walk((await cdp(tab, 'Page.getFrameTree')).frameTree);
      return out;
    }
    const found = await api.scripting.executeScript({target: {tabId: tab, allFrames: true}, func: () => location.href});
    return found.map(f => ({frame: f.frameId, url: f.result}));
  }
  if (action === 'fill') {
    const frames = (Array.isArray(params.frames) ? params.frames : [])
      .filter(f => (chromium ? typeof f?.frame === 'string' : Number.isSafeInteger(f?.frame)) && typeof f.url === 'string');
    if (!frames.length) return {filled: []};
    const creds = {css: String(params.css || ''), username: params.username ?? null, password: params.password ?? null, totp: params.totp ?? null, custom: Array.isArray(params.custom) ? params.custom.filter(f => typeof f?.name === 'string' && typeof f?.value === 'string') : []};
    const urls = frames.map(f => f.url);
    if (chromium) {
      const filled = new Set();
      for (const {frame} of frames) {
        const world = await cdp(tab, 'Page.createIsolatedWorld', {frameId: frame, worldName: 'browser-daemon-fill'});
        const r = await cdp(tab, 'Runtime.evaluate', {expression: `(${fillLogin})(${JSON.stringify(creds)}, ${JSON.stringify(urls)})`, contextId: world.executionContextId, returnByValue: true});
        (r.result?.value?.filled || []).forEach(f => filled.add(f));
      }
      return {filled: [...filled]};
    }
    const done = await api.scripting.executeScript({
      target: {tabId: tab, frameIds: frames.map(f => f.frame)},
      func: fillLogin,
      args: [creds, urls],
    });
    return {filled: [...new Set(done.flatMap(d => d.result?.filled || []))]};
  }
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
  else if (action === 'move') {
    if (!Number.isSafeInteger(params.index) || params.index < 0) throw new Error('Expected a tab index.');
    await api.tabs.move(tab, {index: params.index});
    return {ok: true};
  } else if (action === 'pin') {
    if (typeof params.pinned !== 'boolean') throw new Error('Expected pinned state.');
    await api.tabs.update(tab, {pinned: params.pinned});
    return {ok: true};
  }
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
async function relayCdp(message, connection, currentGeneration, queued = false) {
  const fileState = fileTabs.get(message.tab);
  if (!queued && message.method?.startsWith('Input.') && fileState?.channels.has(message.channel)) {
    fileState.inputSerial = (fileState.inputSerial || Promise.resolve())
      .then(() => relayCdp(message, connection, currentGeneration, true)).catch(() => {});
    return;
  }
  const {channel, tab, id, method} = message;
  const params = message.params || {};
  // A CDP client's session (Playwright via the daemon): the automation set, Chromium only.
  const automation = message.automation === true && chromium;
  const reply = value => {
    if (socket === connection && currentGeneration === generation) connection.send(JSON.stringify({type: 'cdp_result', channel, id, ...value}));
  };
  try {
    const grant = shared.get(tab);
    if (typeof channel !== 'string' || !registration || !grant || grant.paused ||
        !(NodePolicy.relayAllowed(method, params, grant.control) || (automation && grant.control && NodePolicy.automationAllowed(method, params)))) {
      throw new ShimError('Not permitted for this shared tab.');
    }
    await wakeDiscardedTab(tab, await api.tabs.get(tab));
    if (shared.get(tab) !== grant || grant.paused || !safeTab(await api.tabs.get(tab))) throw new ShimError('Tab is paused or sharing ended.');
    channels.set(channel, tab);
    if (fileMethods.has(method)) {
      const result = await fileCommand(tab, channel, method, params);
      if (shared.get(tab) !== grant || grant.paused || !grant.control) throw new ShimError('File sharing ended.');
      reply({result});
      return;
    }
    await armFileInput(tab, channel, method, params);
    if (automation && method === 'Fetch.enable' && fileTabs.has(tab)) {
      fileTabs.get(tab).automationFetch = true; fileTabs.get(tab).network = false;
    }
    if (method === 'Page.startAudio' || method === 'Page.stopAudio') {
      if (method === 'Page.startAudio') {
        if (!sharing.audio) throw new ShimError('Enable Share sound in the browser extension first.');
        await startTabAudio(tab, channel);
      } else stopChannelAudio(channel);
      reply({result: {}});
      return;
    }
    if (automation) automations.set(channel, tab);
    if (chromium && method === 'Page.screencastFrameAck') {
      // The browser had its acknowledgement already: this one frees a slot.
      const cast = castState(tab);
      cast.inflight = Math.max(0, cast.inflight - 1);
      if (cast.pending) { const frame = cast.pending; cast.pending = null; cast.inflight++; sendEvent(tab, 'Page.screencastFrame', frame); }
      reply({result: {}});
      return;
    }
    if (method === 'Page.bringToFront') {
      // Only the tab in front of its window is painted, so following also keeps frames coming.
      if (sharing.follow) await api.tabs.update(tab, {active: true});
      reply({result: {}});
      return;
    }
    if (chromium && (method === 'Page.startScreencast' || method === 'Page.stopScreencast')) screencasts.delete(tab);
    const result = chromium ? await cdp(tab, method, params) : await shim(tab, channel, method, params);
    if (automation) leftBehind(tab, method, params, result);
    if (automation && method === 'Fetch.disable') await resumeFileNetwork(tab);
    // Never return data after local unsharing, pausing or replacement of a grant.
    if (shared.get(tab) !== grant || grant.paused) throw new Error('Sharing ended.');
    reply({result: result ?? {}});
  } catch (error) {
    // Browser/CDP errors can embed page content, input text or script source: only a
    // controlling CDP client gets them (it needs them, e.g. "Cannot find context").
    reply({error: {message: error instanceof ShimError || automation ? String(error?.message || error) : 'Action failed, or the tab is no longer shared.'}});
  }
}
// What CDP clients set in a tab's debugger session (which the live views share), to
// undo when the last one leaves: the owner's tab then behaves as before.
const automations = new Map();
const leftovers = new Map();
function leftBehind(tab, method, params, result) {
  const left = leftovers.get(tab) || {scripts: new Set(), bindings: new Set()};
  leftovers.set(tab, left);
  if (method === 'Page.addScriptToEvaluateOnNewDocument' && result?.identifier) left.scripts.add(result.identifier);
  if (method === 'Runtime.addBinding' && params.name && params.name !== '__bdPasskey') left.bindings.add(params.name);
}
const RESETS = [
  ['Fetch.disable', {}], ['Network.setExtraHTTPHeaders', {headers: {}}], ['Network.setCacheDisabled', {cacheDisabled: false}],
  ['Network.setBlockedURLs', {urls: []}], ['Network.setBypassServiceWorker', {bypass: false}],
  ['Network.emulateNetworkConditions', {offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: -1}],
  ['Network.disable', {}], ['Log.disable', {}],
  ['Emulation.clearDeviceMetricsOverride', {}], ['Emulation.setEmulatedMedia', {media: '', features: []}],
  ['Emulation.setTouchEmulationEnabled', {enabled: false}], ['Emulation.setFocusEmulationEnabled', {enabled: false}],
  ['Emulation.clearGeolocationOverride', {}], ['Emulation.setUserAgentOverride', {userAgent: ''}],
  ['Emulation.setLocaleOverride', {}], ['Emulation.setTimezoneOverride', {timezoneId: ''}],
  ['Emulation.setScriptExecutionDisabled', {value: false}], ['Emulation.setCPUThrottlingRate', {rate: 1}],
  ['Page.setBypassCSP', {enabled: false}], ['Page.setInterceptFileChooserDialog', {enabled: false}],
  ['Page.setLifecycleEventsEnabled', {enabled: false}],
];
async function resetAutomation(tab) {
  const left = leftovers.get(tab);
  leftovers.delete(tab);
  if (!chromium || !attached.has(tab)) return;
  const send = (method, params) => api.debugger.sendCommand({tabId: tab}, method, params).catch(() => {});
  for (const [method, params] of RESETS) await send(method, params);
  for (const identifier of left?.scripts || []) await send('Page.removeScriptToEvaluateOnNewDocument', {identifier});
  for (const name of left?.bindings || []) await send('Runtime.removeBinding', {name});
  await resumeFileNetwork(tab).catch(() => {});
}
function closeChannel(channel) {
  stopChannelAudio(channel);
  stopFileChannel(channel);
  const tab = channels.get(channel);
  channels.delete(channel);
  if (automations.delete(channel) && ![...automations.values()].includes(tab)) void resetAutomation(tab);
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

// Sound follows live-view channels; a lease also covers crashed/restarted workers.
const mediaAudio = new Map();
async function renewTabAudio(tab) {
  const state = mediaAudio.get(tab);
  if (!state || !registration || !sharing.audio || shared.get(tab)?.paused !== false) { stopTabAudio(tab); return; }
  if (state.channels.has('public') && (!sharing.public || Date.now() > (state.publicUntil || 0))) {
    state.channels.delete('public'); state.publicRing = [];
    if (!state.channels.size) { stopTabAudio(tab); return; }
  }
  try {
    await api.scripting.executeScript({target: {tabId: tab, allFrames: true}, func: captureMediaAudio, args: [state.token]});
  } catch { /* Navigation or an inaccessible frame: retry only while watched. */ }
}
async function startTabAudio(tab, channel) {
  let state = mediaAudio.get(tab);
  if (!state) {
    if (mediaAudio.size >= 8) throw new ShimError('Too many tabs are streaming sound.');
    state = {token: crypto.randomUUID(), channels: new Set(), timer: null};
    mediaAudio.set(tab, state);
    state.timer = setInterval(() => void renewTabAudio(tab), 5000);
  }
  state.channels.add(channel);
  try {
    await api.scripting.executeScript({target: {tabId: tab, allFrames: true}, func: captureMediaAudio, args: [state.token]});
  } catch {
    stopChannelAudio(channel);
    throw new ShimError('This page does not allow media sound capture.');
  }
}
function stopTabAudio(tab) {
  const state = mediaAudio.get(tab);
  if (!state) return;
  mediaAudio.delete(tab);
  clearInterval(state.timer);
  void api.scripting.executeScript({target: {tabId: tab, allFrames: true}, func: captureMediaAudio, args: [null, state.token]}).catch(() => {});
  for (const channel of state.channels) sendEvent(tab, 'Page.audioStopped', {}, channel);
}
function stopChannelAudio(channel) {
  for (const [tab, state] of mediaAudio) {
    state.channels.delete(channel);
    if (!state.channels.size) stopTabAudio(tab);
  }
}
function relayMediaAudio(message, sender) {
  const tab = sender.tab?.id;
  const state = mediaAudio.get(tab);
  if (sender.id !== api.runtime.id || !state || message.token !== state.token ||
      !registration || !sharing.audio || shared.get(tab)?.paused !== false ||
      !safeTab(sender.tab) || !safeURL(sender.url) ||
      typeof message.data !== 'string' || message.data.length > 24000 ||
      !/^[A-Za-z0-9+/]+={0,2}$/.test(message.data) || ![44100, 48000].includes(message.rate)) return {ok: false};
  if (state.channels.has('public') && sharing.public && Date.now() < state.publicUntil) {
    const chunk = {seq: state.publicSeq = (state.publicSeq || 0) + 1,
      data: message.data, rate: message.rate, stream: `${sender.frameId}:${sender.documentId || state.token}`};
    (state.publicRing ||= []).push(chunk);
    if (state.publicRing.length > 24) state.publicRing.shift();
  }
  // Don't accumulate latency on slow networks. Each frame has its own PCM clock.
  if (socket?.readyState !== WebSocket.OPEN || socket.bufferedAmount > 256 * 1024) return {ok: true};
  for (const channel of state.channels) {
    if (channels.get(channel) === tab) sendEvent(tab, 'Page.audioChunk', {
      data: message.data, rate: message.rate, stream: `${sender.frameId}:${sender.documentId || state.token}`,
    }, channel);
  }
  return {ok: true};
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
      if (!nativeReady) {
        // A live view uses this exact expression to answer the extension-backed
        // passkey hook, its heartbeat, or read a remote selection. Run only these
        // operations in the page; general evaluation still requires the companion.
        if (params.expression === REMOTE_SELECTION_EXPR) {
          const selection = await api.scripting.executeScript({target: {tabId: tab}, func: () => {
            let active = document.activeElement;
            while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
            if (active && ((active.tagName === 'INPUT' && active.type !== 'password') || active.tagName === 'TEXTAREA')) {
              try { if (active.selectionEnd > active.selectionStart) return active.value.substring(active.selectionStart, active.selectionEnd); } catch {}
            }
            return window.getSelection()?.toString() || '';
          }});
          return {result: {value: selection[0]?.result || ''}};
        }
        if (params.expression === 'globalThis.__bdPasskeyRepair?.(); globalThis.__bdPasskeyAlive = Date.now()' ||
            params.expression === 'globalThis.__bdPasskeyRepair?.(); globalThis.__bdPasskeyAlive = Date.now(); globalThis.__bdPasskeyOffer = true') {
          const offer = params.expression.endsWith('__bdPasskeyOffer = true');
          await api.scripting.executeScript({target: {tabId: tab}, world: 'MAIN', func: showOffer => {
            globalThis.__bdPasskeyRepair?.();
            globalThis.__bdPasskeyAlive = Date.now();
            if (showOffer) globalThis.__bdPasskeyOffer = true;
            return {ok: true};
          }, args: [offer]});
          return {result: {value: {ok: true}}};
        }
        const match = params.expression?.match(/^globalThis\.__bdPasskeyAnswer\?\.\(("(?:\\.|[^"\\])*"), (.+)\)$/);
        if (!match) throw new ShimError('Evaluation in Firefox requires the companion.');
        const passkeyId = JSON.parse(match[1]);
        const answer = JSON.parse(match[2]);
        if (typeof passkeyId !== 'string') throw new ShimError('Invalid passkey response.');
        await api.scripting.executeScript({target: {tabId: tab}, world: 'MAIN', func: (id, value) => {
          globalThis.__bdPasskeyAnswer?.(id, value);
          return {ok: true};
        }, args: [passkeyId, answer]});
        return {result: {value: {ok: true}}};
      }
      return {result: {value: await run('evaluate', {script: `return (${params.expression});`, args: []})}};
    }
    default: throw new ShimError(`${method} is not available in Firefox.`);
  }
}

const REMOTE_SELECTION_EXPR = `(() => {
  let a = document.activeElement;
  while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
  if (a && ((a.tagName === "INPUT" && a.type !== "password") || a.tagName === "TEXTAREA")) {
    try { if (a.selectionEnd > a.selectionStart) return a.value.substring(a.selectionStart, a.selectionEnd); } catch (e) {}
  }
  return window.getSelection()?.toString() || "";
})()`;
// A press, its drag path and the release become one trusted pointer action.
const pointers = new Map();
async function pointer(tab, params, run) {
  const button = ['left', 'middle', 'right'].indexOf(params.button);
  if (params.type === 'mouseWheel') {
    touchCast(tab);
    return run('scroll', {x: params.x || 0, y: params.y || 0, deltaX: params.deltaX || 0, deltaY: params.deltaY || 0, modifiers: params.modifiers || 0});
  }
  if (params.type === 'mousePressed') { pointers.set(tab, {x: params.x, y: params.y, button: Math.max(0, button), clickCount: params.clickCount, modifiers: params.modifiers || 0, path: []}); return {}; }
  const down = pointers.get(tab);
  if (!down && params.type === 'mouseMoved') return run('move', {x: params.x, y: params.y});
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
  return run('click', {x: down.x, y: down.y, button: down.button, clickCount: down.clickCount, modifiers: down.modifiers,
    path: [...down.path, {x: params.x, y: params.y}]});
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
    if (command.public_grant && (!sharing.public || command.public_grant !== publicGrant)) throw new Error('Public sharing ended.');
    if (command.tab === 0 && command.action === 'follow') {
      sharing.follow = command.params?.enabled === true;
      await api.storage.local.set({followView: sharing.follow});
      await publish();
      if (socket === connection && currentGeneration === generation) connection.send(JSON.stringify({type: 'result', id: command.id, result: {ok: true}}));
      return;
    }
    // Tab 0: the browser's first tab, where the owner lets controllers open tabs.
    if (command.tab === 0 && command.action === 'open') {
      if (!registration || !canOpen() || !NodePolicy.pageURL(command.params?.url)) throw new Error('Opening tabs is not allowed.');
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
  publicGrant = crypto.randomUUID();
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
    catch { if (socket === connection) await stop('Daemon is no longer on the allowlist', true); return; }
    if (socket !== connection) return;
    status = 'Socket connected — checking pairing code';
    record(status);
    connection.send(JSON.stringify({code, key, remember, name: name.trim(), browser: chromium ? 'chromium' : 'firefox', version: api.runtime.getManifest().version}));
    code = ''; key = '';
  };
  connection.onmessage = async event => {
    if (socket !== connection) return;
    if (event.data.length > 8 * 1024 * 1024) { void stop('Invalid daemon message'); return; }
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
      if (Array.isArray(message.origins)) void updateAutoOrigins(message.origins).catch(error => record(`Daemon origin update failed: ${error.message}`));
      clientConnected = false;
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
      void connectNativeForRegistration();
    } else if (message.type === 'authorization_revoked') {
      await api.storage.local.remove('rememberedNode');
      await stop('Browser authorization was revoked — pair again to reconnect');
    } else if (message.type === 'heartbeat') {
      lastAnswer = Date.now();
      if (Array.isArray(message.origins)) void updateAutoOrigins(message.origins).catch(error => record(`Daemon origin update failed: ${error.message}`));
      if (typeof message.client_connected === 'boolean' && clientConnected !== message.client_connected) {
        clientConnected = message.client_connected;
        await badge();
      }
    } else if (message.type === 'client_presence' && registration) {
      clientConnected = message.connected === true;
      await badge();
    }
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
    if (saved.origin !== api.runtime.getURL('').replace(/\/$/, '')) {
      status = 'This extension installation differs from the one that was paired — pair this browser again';
      record(status); return;
    }
    if (!(await api.permissions.contains({origins: [permissionPattern(saved.server)]}))) {
      if (status !== 'Remembered browser needs daemon site permission — open setup') {
        status = 'Remembered browser needs daemon site permission — open setup';
        record(status);
      }
      return;
    }
    if (force) await api.storage.local.set({rememberedNode: {...saved, paused: false}});
    await connect({server: saved.server, key: saved.key, name: saved.name});
  } catch (error) { await stop(error?.message || 'Remembered browser could not reconnect', true); }
  finally { restoring = false; }
}

async function localMessage(message, sender) {
  // Accept only our own extension UI, never a content script or web page.
  if (message.type === 'fileBridge') return fileBridgeMessage(message, sender);
  if (message.type === 'mediaAudio') return relayMediaAudio(message, sender);
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
    await api.storage.local.set({daemonAllowlist: origins, daemonAllowlistMode: 'custom'});
    policyCache = null;
    if (registration && !origins.includes(registration.server)) await stop('Daemon was removed from the allowlist');
    return {ok: true};
  }
  if (message.type === 'setDaemonOriginMode') {
    const current = await daemonPolicy();
    if (current.managed) throw new Error('Daemon origin settings are managed by your administrator.');
    if (message.mode === 'auto') {
      const discoveryOrigin = NodePolicy.serverURL(message.discoveryOrigin || '');
      discoveryCache = null;
      const origins = await discoverDaemonOrigins(discoveryOrigin);
      await api.storage.local.set({daemonAllowlistMode: 'auto', daemonDiscoveryOrigin: discoveryOrigin});
      if (registration && !origins.includes(registration.server)) await stop('Daemon is no longer on the automatic origin list');
    } else if (message.mode === 'custom') {
      await api.storage.local.set({daemonAllowlistMode: 'custom', daemonAllowlist: current.origins});
    } else throw new Error('Choose automatic or custom daemon origins.');
    policyCache = null;
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
    const permissions = await api.permissions.contains({permissions: ['history', 'bookmarks']});
    const savedSuggestions = await api.storage.local.get('addressSuggestions');
    const addressSuggestions = permissions && savedSuggestions.addressSuggestions === true;
    return {allowlist, rememberedBrowser: rememberedNode ? {server: rememberedNode.server, name: rememberedNode.name, paused: rememberedNode.paused} : null, dashboardRequest: dashboardPairing?.expires > Date.now() ? {server: dashboardPairing.server, remember: dashboardPairing.remember, control: dashboardPairing.control !== false} : null, status, history, connecting: !!pendingConnection || !!socket && !registration, registration, tabs, saved: await api.storage.local.get(['server', 'name', 'rememberPreference']), defaultName: await defaultName(), sharing, version: api.runtime.getManifest().version, chromium, nativeReady, addressSuggestions};
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
    else { disconnectNative(); publishSafely(); }
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
    else for (const id of shared.keys()) void hookPasskeys(id).catch(() => hooked.delete(id));
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
  if (message.type === 'shareFiles') {
    if (message.enabled && !(await api.permissions.contains({origins: chromium
        ? ['http://*/*', 'https://*/*'] : ['<all_urls>'],
        ...(!chromium ? {permissions: ['webRequest', 'webRequestBlocking', 'webRequestFilterResponse']} : {})}))) throw new Error('Site access is needed for file transfer.');
    sharing.files = message.enabled === true;
    await api.storage.local.set({shareFiles: sharing.files});
    if (!sharing.files) for (const tab of fileTabs.keys()) stopFileTab(tab);
    if (sharing.files && !chromium) installFirefoxFileFilter();
    publishSoon();
    return {ok: true};
  }
  if (message.type === 'sharePublic') {
    sharing.public = message.enabled === true;
    publicGrant = crypto.randomUUID();
    for (const state of mediaAudio.values()) { state.publicRing = []; state.publicUntil = 0; }
    await api.storage.local.set({sharePublic: sharing.public});
    publishSoon();
    return {ok: true};
  }
  if (message.type === 'shareAudio') {
    sharing.audio = message.enabled === true;
    await api.storage.local.set({shareAudio: sharing.audio});
    if (!sharing.audio) for (const tab of mediaAudio.keys()) stopTabAudio(tab);
    publishSoon();
    return {ok: true};
  }
  if (message.type === 'addressSuggestions') {
    const enabled = message.enabled === true;
    if (enabled && !(await api.permissions.contains({permissions: ['history', 'bookmarks']}))) {
      throw new Error('Grant history and bookmark access first.');
    }
    await api.storage.local.set({addressSuggestions: enabled});
    return {ok: true};
  }
  if (message.type === 'follow') {
    sharing.follow = message.enabled === true;
    await api.storage.local.set({followView: sharing.follow});
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
    for (const id of shared.keys()) void hookPasskeys(id).catch(() => hooked.delete(id));
    await publish();
    return {ok: true};
  }
  throw new Error('Unknown action.');
}

api.runtime.onMessage.addListener((message, sender, respond) => {
  if (message?.type === 'passkeyHook') {
    const tab = sender.tab?.id;
    const grant = shared.get(tab);
    void (async () => {
      if (sender.id !== api.runtime.id || sender.frameId !== 0 || !registration || !grant?.control || grant.paused ||
          !message.payload || typeof message.payload !== 'string') throw new Error('Passkey request is not allowed for this tab.');
      const current = await api.tabs.get(tab);
      if (!safeTab(current) || new URL(current.url).origin !== grant.origin) throw new Error('The shared page changed.');
      sendEvent(tab, 'Runtime.bindingCalled', {name: '__bdPasskey', payload: message.payload});
      respond({ok: true});
    })().catch(error => respond({error: error.message}));
    return true;
  }
  // Keep the response channel open in both Chromium and Firefox.
  // Serialize grant updates so Stop cannot race an in-flight Share.
  tabSerial = tabSerial.then(() => localMessage(message, sender)).then(
    value => respond(value), error => respond({error: error.message}));
  return true;
});
api.tabs.onRemoved.addListener(id => { shared.delete(id); hooked.delete(id); void detach(id); publishSafely(); });
let publishTimer = null;
function publishSoon() { clearTimeout(publishTimer); publishTimer = setTimeout(publishSafely, 150); }
api.tabs.onUpdated.addListener((id, change, tab) => {
  const grant = shared.get(id);
  if (!grant) return;
  if (change.url) {
    hooked.delete(id);
    // Sharing follows the tab. A page that must not be shared pauses it at once,
    // before anything of that page is captured or relayed.
    grant.paused = !safeTab({incognito: tab?.incognito, url: change.url});
    if (!grant.paused) grant.origin = new URL(change.url).origin;
    if (!chromium) sendEvent(id, 'Page.frameNavigated', {frame: {id: String(id), url: change.url}});
  }
  if (!chromium && change.status === 'loading') { sendEvent(id, 'Page.frameStartedLoading', {frameId: String(id)}); touchCast(id); }
  if (!chromium && change.status === 'complete') { sendEvent(id, 'Page.loadEventFired', {timestamp: Date.now() / 1000}); touchCast(id); }
  if (grant.paused) { stopTabAudio(id); stopFileTab(id); }
  if (change.status === 'complete' && mediaAudio.has(id)) void renewTabAudio(id);
  if (change.status === 'complete' && !chromium && grant.control && !grant.paused) void hookPasskeys(id).catch(() => hooked.delete(id));
  if (change.status === 'complete' && fileTabs.get(id)?.channels.size) void installFilePages(id);
  if (change.url || change.title || change.favIconUrl || change.status === 'complete' || change.pinned !== undefined) publishSoon();
});
api.tabs.onMoved.addListener(() => { if (registration) publishSoon(); });
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
  if (method === 'Fetch.requestPaused' && fileTabs.get(source.tabId)?.network && !fileTabs.get(source.tabId)?.automationFetch) {
    void interceptFileResponse(source.tabId, params).catch(() => cdp(source.tabId, 'Fetch.continueRequest', {requestId: params.requestId}).catch(() => {}));
    return;
  }
  if (method === 'Page.fileChooserOpened' && fileTabs.has(source.tabId)) {
    void interceptFileChooser(source.tabId, params).catch(() => {});
    return;
  }
  if (method === 'Page.frameNavigated' && !params.frame.parentId) {
    grant.paused = !safeURL(params.frame.url);
    if (!grant.paused) grant.origin = new URL(params.frame.url).origin;
    publishSoon();
  }
  // Runtime is enabled for the passkey hook: its events leave only for CDP clients.
  if (method.startsWith('Runtime.') && !(method === 'Runtime.bindingCalled' && params.name === '__bdPasskey') &&
      ![...automations.values()].includes(source.tabId)) return;
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
  hooked.delete(source.tabId);
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

// Safety net for a remembered browser: whatever ended the connection (a daemon
// restart, a failed attempt that scheduled nothing), try again every minute.
api.alarms.onAlarm.addListener(alarm => {
  if (alarm.name === 'node-reconnect' || alarm.name === 'node-keepalive') void historyReady.then(() => restoreRemembered());
});
void api.alarms.create('node-keepalive', {delayInMinutes: 1, periodInMinutes: 1});
void api.alarms.clear('daemon-policy-refresh');
void historyReady.then(() => restoreRemembered());

api.storage.onChanged.addListener((changes, area) => {
  if (area !== 'managed' && !(area === 'local' && (changes.daemonAllowlist || changes.daemonAllowlistMode || changes.daemonDiscoveryOrigin))) return;
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

void sharingReady.then(() => { if (!chromium && sharing.files) installFirefoxFileFilter(); }).catch(() => {});
