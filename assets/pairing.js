/* Pair only through the installed extension's local approval page. */
(() => {
  const button = document.getElementById('pairBrowser');
  const status = document.getElementById('pairBrowserStatus');
  if (!button || !status) return;
  function extension(action, extra = {}) {
    const id = crypto.randomUUID();
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        window.removeEventListener('message', receive);
        reject(new Error('No extension found. In Browser Daemon Node, choose this daemon and click “Pair” once.'));
      }, 5000);
      function receive(event) {
        if (event.source !== window || event.origin !== location.origin ||
            event.data?.source !== 'browser-daemon-extension' || event.data.id !== id) return;
        clearTimeout(timeout); window.removeEventListener('message', receive);
        if (event.data.error) reject(new Error(event.data.error));
        else resolve(event.data);
      }
      window.addEventListener('message', receive);
      window.postMessage({source: 'browser-daemon-dashboard', id, action, ...extra}, location.origin);
    });
  }
  const EXT_NODES_CACHE = 'bd-extension-browser-nodes-cache';
  const EXT_PEERS_CACHE = 'bd-extension-peers-cache';
  let refreshing = false;

  // Emit cached extension nodes immediately on load so the dashboard never has a blank delay.
  try {
    const cached = localStorage.getItem(EXT_NODES_CACHE);
    if (cached) {
      const list = JSON.parse(cached);
      if (Array.isArray(list) && list.length) {
        window.dispatchEvent(new CustomEvent('browser-daemon-extension-nodes', {detail: list}));
      }
    }
  } catch { /* ignore */ }

  async function refreshBrowsers() {
    if (refreshing || document.hidden) return;
    refreshing = true;
    try {
      const headers = typeof authHeaders === 'function' ? authHeaders() : {};
      const fetchBase = async base => {
        const response = await fetch(`${base}/extension/nodes`, {headers, cache: 'no-store', signal: AbortSignal.timeout(8000)});
        if (!response.ok) throw new Error();
        return (await response.json()).nodes.map(node => ({...node, daemon: base === '/api' ? '' : decodeURIComponent(base.slice('/api/peers/'.length))}));
      };

      // Query local daemon immediately without waiting for mesh overview
      let cachedPeers = [];
      try {
        const cp = localStorage.getItem(EXT_PEERS_CACHE);
        if (cp) cachedPeers = JSON.parse(cp);
      } catch { /* ignore */ }

      const initialBases = ['/api', ...cachedPeers.map(p => `/api/peers/${encodeURIComponent(p)}`)];
      const initialFetches = initialBases.map(fetchBase);

      // Concurrently query mesh overview to discover any updated peers
      const meshPromise = fetch('/api/mesh/overview', {headers, cache: 'no-store', signal: AbortSignal.timeout(8000)})
        .then(r => r.ok ? r.json() : null)
        .catch(() => null);

      const [initialReports, mesh] = await Promise.all([
        Promise.allSettled(initialFetches),
        meshPromise,
      ]);

      const onlinePeers = mesh?.peers?.filter(p => p.status === 'online').map(p => p.name) || [];
      try { localStorage.setItem(EXT_PEERS_CACHE, JSON.stringify(onlinePeers)); } catch { /* ignore */ }

      // If mesh discovered online peers that weren't in cachedPeers, query them now
      const pendingBases = onlinePeers
        .filter(p => !cachedPeers.includes(p))
        .map(p => `/api/peers/${encodeURIComponent(p)}`);

      let extraReports = [];
      if (pendingBases.length) {
        extraReports = await Promise.allSettled(pendingBases.map(fetchBase));
      }

      const allNodes = [...initialReports, ...extraReports].flatMap(r => r.status === 'fulfilled' ? r.value : []);
      try { localStorage.setItem(EXT_NODES_CACHE, JSON.stringify(allNodes)); } catch { /* ignore */ }
      window.dispatchEvent(new CustomEvent('browser-daemon-extension-nodes', {detail: allNodes}));
    } catch { /* Retried on the next round. */ }
    finally { refreshing = false; }
  }
  // Which listed browser is this one: asked of the extension, if it is installed.
  async function identify() {
    const node = await extension('discover').then(r => r.node || null, () => null);
    window.dispatchEvent(new CustomEvent('browser-daemon-this-browser', {detail: node}));
  }
  window.addEventListener('browser-daemon-refresh-extensions', () => { void refreshBrowsers(); void identify(); });
  void refreshBrowsers(); void identify();
  setInterval(refreshBrowsers, 5000);
  setInterval(identify, 15000);
  // Close the sheet once the extension reports it is registered, i.e. the popup was approved.
  async function closeWhenConnected() {
    const dialog = button.closest('dialog');
    for (let i = 0; i < 120 && dialog?.open; i++) {
      await new Promise(resolve => setTimeout(resolve, 1000));
      const browser = await extension('discover').catch(() => null);
      if (browser?.connected) {
        dialog.close();
        status.textContent = '';
        window.dispatchEvent(new Event('browser-daemon-refresh-extensions'));
        return;
      }
    }
  }
  // Asks the extension for its origin, has the daemon issue a code for it, and hands the
  // code over. The extension approves at once when its own popup started this.
  async function pair() {
    const browser = await extension('discover');
    if (browser.connected) {
      status.textContent = 'Already paired: pick tabs to share in the extension.';
      void identify();
      return true;
    }
    const response = await fetch('/api/extension/pair', {
      method: 'POST', credentials: 'same-origin', cache: 'no-store',
      headers: {'Content-Type': 'application/json'}, body: JSON.stringify({origin: browser.origin, remember: true}),
    });
    if (!response.ok) throw Object.assign(new Error(`Pairing failed (${response.status}). Sign in to this dashboard and try again.`), {signIn: [401, 403].includes(response.status)});
    const pair = await response.json();
    const opened = await extension('approve', {code: pair.code, remember: pair.remember, control: true});
    status.textContent = opened.where === 'connected' ? 'Connected: pick tabs to share in the extension.'
      : opened.where === 'popup' ? 'Answer in the extension popup (top right of this window).'
      : 'Answer in the extension tab that just opened.';
    void closeWhenConnected();
    return true;
  }
  button.addEventListener('click', async () => {
    button.disabled = true;
    status.textContent = 'Looking for the extension…';
    try { await pair(); }
    catch (error) { status.textContent = error.message; }
    finally { button.disabled = false; }
  });
  // The extension's Connect button opens this page with #nodes/add/connect: pair on
  // our own, waiting for a sign-in if the dashboard asks for one.
  async function autoPair() {
    button.disabled = true;
    try {
      for (let i = 0; i < 90; i++) {
        status.textContent = i ? 'Sign in to this dashboard to finish connecting…' : 'Connecting this browser…';
        try { if (await pair()) return; }
        catch (error) {
          if (!error.signIn) { status.textContent = error.message; return; }
        }
        await new Promise(resolve => setTimeout(resolve, 2000));
      }
    } finally { button.disabled = false; }
  }
  if (location.hash === '#nodes/add/connect') {
    history.replaceState(null, '', '#nodes/add');
    void autoPair();
  }
})();
