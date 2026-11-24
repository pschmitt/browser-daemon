/* Local tab API: push this browser's tabs to browser-daemon's native messaging
 * host on every change, so `browser-daemon tabs` and other local clients read
 * them from memory (docs/local-api.md). Opt-in. Tabs of private windows are
 * included when the browser lets the extension into them (Firefox: "Run in
 * Private Windows", Chromium: "Allow in Incognito"). Nothing here goes to a
 * daemon. */
globalThis.localApi = (() => {
  const HOST = 'dev.pschmitt.browser_daemon_local';
  const ext = globalThis.browser || chrome;
  const onChromium = typeof ext.runtime.getBrowserInfo !== 'function';
  const MAX_RETRY = 5 * 60 * 1000;
  let port = null;
  let enabled = false;
  let connectedAt = 0;
  let lastError = '';
  let sendTimer = null;
  let retryTimer = null;
  let retryDelay = 1000;
  // Chromium lets a service worker use native messaging only after it
  // restarts once the (optional) permission is granted.
  let grantedWhileRunning = false;

  async function profileId() {
    const {localApiProfile} = await ext.storage.local.get('localApiProfile');
    if (localApiProfile) return localApiProfile;
    const id = crypto.randomUUID();
    await ext.storage.local.set({localApiProfile: id});
    return id;
  }

  async function identity() {
    if (!onChromium) {
      const info = await ext.runtime.getBrowserInfo();
      return {browser: 'firefox', name: info.name || 'Firefox', version: info.version || ''};
    }
    const brands = navigator.userAgentData?.brands || [];
    const brand = brands.find(b => !/Not.?A.?Brand|Chromium/i.test(b.brand)) || brands.find(b => /Chromium/i.test(b.brand));
    return {browser: 'chromium', name: brand?.brand || 'Chromium', version: brand?.version || ''};
  }

  function tabUrl(tab) {
    return tab.url || tab.pendingUrl || '';
  }

  async function snapshot() {
    // Firefox for Android has no windows API: one window per tab's windowId.
    const [windows, tabs] = await Promise.all([
      ext.windows ? ext.windows.getAll({windowTypes: ['normal']}) : Promise.resolve(null),
      ext.tabs.query({}),
    ]);
    return {
      type: 'snapshot',
      windows: windows
        ? windows.map(w => ({id: w.id, focused: w.focused === true}))
        : [...new Set(tabs.map(t => t.windowId))].map(id => ({id, focused: true})),
      tabs: tabs.filter(t => t.id >= 0).map(t => ({
        id: t.id,
        window_id: t.windowId,
        index: t.index,
        title: t.title || '',
        url: tabUrl(t),
        active: t.active === true,
        audible: t.audible === true,
        muted: t.mutedInfo?.muted === true,
        pinned: t.pinned === true,
        discarded: t.discarded === true,
        private: t.incognito === true,
        favicon: /^https?:/.test(t.favIconUrl || '') ? t.favIconUrl : '',
      })),
    };
  }

  async function send() {
    sendTimer = null;
    if (!port) return;
    try {
      port.postMessage(await snapshot());
    } catch (error) {
      lastError = error.message;
    }
  }

  // Coalesce bursts (a page load fires several updates) into one snapshot.
  function sendSoon() {
    if (!port || sendTimer) return;
    sendTimer = setTimeout(() => void send(), 100);
  }

  function scheduleRetry() {
    clearTimeout(retryTimer);
    retryTimer = setTimeout(() => void connect(), retryDelay);
    retryDelay = Math.min(retryDelay * 2, MAX_RETRY);
  }

  // A command from the local server (`browser-daemon tabs activate|close|open|…`),
  // only ever from our own native host.
  async function runCommand(from, message) {
    if (!message || message.type !== 'command' || !Number.isSafeInteger(message.id)) return;
    let reply = {type: 'result', id: message.id, ok: true};
    try {
      if (message.action === 'content') {
        const tab = await ext.tabs.get(message.tab);
        reply.content = await tabContent.get(tab.id, message.kind);
      } else if (message.action === 'activate' || message.action === 'close') {
        const tab = await ext.tabs.get(message.tab);
        if (message.action === 'close') {
          await ext.tabs.remove(tab.id);
        } else {
          await ext.tabs.update(tab.id, {active: true});
          if (ext.windows) await ext.windows.update(tab.windowId, {focused: true});
        }
      } else if (message.action === 'open') {
        if (!/^https?:\/\//i.test(message.url || '')) throw new Error('Only http and https URLs open.');
        const tab = await ext.tabs.create({url: message.url, active: message.background !== true});
        if (message.background !== true && ext.windows && tab.windowId !== undefined) {
          await ext.windows.update(tab.windowId, {focused: true});
        }
        reply.tab = tab.id;
      } else {
        throw new Error(`Unknown command: ${message.action}`);
      }
    } catch (error) {
      reply = {type: 'result', id: message.id, ok: false, error: String(error?.message || error)};
    }
    try { from.postMessage(reply); } catch { /* The host went away. */ }
  }

  async function connect() {
    if (port || !enabled) return;
    let next;
    try {
      next = ext.runtime.connectNative(HOST);
    } catch (error) {
      lastError = error.message;
      scheduleRetry();
      return;
    }
    port = next;
    connectedAt = Date.now();
    next.onDisconnect.addListener(() => {
      if (port !== next) return;
      lastError = ext.runtime.lastError?.message || next.error?.message || 'disconnected';
      port = null;
      clearTimeout(sendTimer);
      sendTimer = null;
      // A host that stayed up a while was working: start the backoff over.
      if (Date.now() - connectedAt > 10000) retryDelay = 1000;
      if (enabled) scheduleRetry();
    });
    next.onMessage.addListener(message => void runCommand(next, message));
    try {
      next.postMessage({type: 'hello', profile: await profileId(), ...await identity()});
      lastError = '';
      await send();
    } catch (error) {
      lastError = error.message;
    }
  }

  function disconnect() {
    clearTimeout(retryTimer);
    clearTimeout(sendTimer);
    sendTimer = null;
    retryDelay = 1000;
    const old = port;
    port = null;
    try { old?.disconnect(); } catch { /* Already gone. */ }
  }

  async function permitted() {
    if (!onChromium) return true;
    try { return await ext.permissions.contains({permissions: ['nativeMessaging']}); } catch { return false; }
  }

  async function wanted() {
    let managed = false;
    try { managed = (await ext.storage.managed.get('localApi')).localApi === true; } catch { /* No policy. */ }
    const {localApi} = await ext.storage.local.get('localApi');
    return (managed || localApi === true) && await permitted();
  }

  async function refresh() {
    enabled = await wanted();
    if (enabled) await connect();
    else disconnect();
  }

  async function setEnabled(value) {
    await ext.storage.local.set({localApi: value === true});
    if (value === true && onChromium && grantedWhileRunning) {
      // Answer the popup first; the reload closes it and reconnects the rest.
      setTimeout(() => ext.runtime.reload(), 300);
      return;
    }
    await refresh();
  }

  async function status() {
    let managed = false;
    try { managed = (await ext.storage.managed.get('localApi')).localApi === true; } catch { /* No policy. */ }
    return {enabled, managed, connected: !!port && !lastError, error: enabled ? lastError : '', permitted: await permitted()};
  }

  ext.tabs.onCreated.addListener(sendSoon);
  ext.tabs.onRemoved.addListener(sendSoon);
  ext.tabs.onActivated.addListener(sendSoon);
  ext.tabs.onMoved.addListener(sendSoon);
  ext.tabs.onAttached?.addListener(sendSoon);
  ext.tabs.onDetached?.addListener(sendSoon);
  ext.tabs.onReplaced?.addListener(sendSoon);
  ext.tabs.onUpdated.addListener((id, change) => {
    if (change.url !== undefined || change.title !== undefined || change.audible !== undefined ||
        change.mutedInfo !== undefined || change.pinned !== undefined || change.discarded !== undefined ||
        change.favIconUrl !== undefined || change.status === 'complete') sendSoon();
  });
  ext.windows?.onCreated.addListener(sendSoon);
  ext.windows?.onRemoved.addListener(sendSoon);
  ext.windows?.onFocusChanged.addListener(sendSoon);
  ext.storage.onChanged.addListener((changes, area) => {
    if ((area === 'local' || area === 'managed') && changes.localApi) void refresh();
  });
  ext.permissions.onAdded.addListener(added => {
    if (onChromium && added?.permissions?.includes('nativeMessaging')) grantedWhileRunning = true;
    void refresh();
  });
  ext.permissions.onRemoved.addListener(() => void refresh());
  void refresh();

  return {setEnabled, status};
})();
