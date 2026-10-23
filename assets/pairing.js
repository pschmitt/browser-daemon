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
  let refreshing = false;
  async function refreshBrowsers() {
    if (refreshing || document.hidden) return;
    refreshing = true;
    try {
      const headers = typeof authHeaders === 'function' ? authHeaders() : {};
      const mesh = await fetch('/api/mesh/overview', {headers, cache: 'no-store', signal: AbortSignal.timeout(8000)}).then(r => { if (!r.ok) throw Error(); return r.json(); });
      const bases = ['/api', ...mesh.peers.filter(p => p.status === 'online').map(p => `/api/peers/${encodeURIComponent(p.name)}`)];
      const reports = await Promise.allSettled(bases.map(async base => {
        const response = await fetch(`${base}/extension/nodes`, {headers, cache: 'no-store', signal: AbortSignal.timeout(8000)});
        if (!response.ok) throw new Error();
        return (await response.json()).nodes.map(node => ({...node, daemon: base === '/api' ? '' : decodeURIComponent(base.slice('/api/peers/'.length))}));
      }));
      window.dispatchEvent(new CustomEvent('browser-daemon-extension-nodes', {detail: reports.flatMap(r => r.status === 'fulfilled' ? r.value : [])}));
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
  button.addEventListener('click', async () => {
    button.disabled = true;
    status.textContent = 'Looking for the extension…';
    try {
      const browser = await extension('discover');
      if (browser.connected) {
        status.textContent = 'Already paired: pick tabs to share in the extension.';
        void identify();
        return;
      }
      const response = await fetch('/api/extension/pair', {
        method: 'POST', credentials: 'same-origin', cache: 'no-store',
        headers: {'Content-Type': 'application/json'}, body: JSON.stringify({origin: browser.origin, remember: true}),
      });
      if (!response.ok) throw new Error(`Pairing failed (${response.status}). Sign in to this dashboard and try again.`);
      const pair = await response.json();
      const opened = await extension('approve', {code: pair.code, remember: pair.remember, control: true});
      status.textContent = opened.where === 'popup' ? 'Answer in the extension popup (top right of this window).'
        : 'Answer in the extension tab that just opened.';
      void closeWhenConnected();
    } catch (error) { status.textContent = error.message; }
    finally { button.disabled = false; }
  });
})();
