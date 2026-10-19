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
        reject(new Error('Extension not available on this dashboard. Open Browser Daemon Node, enter this daemon URL and click Enable dashboard pairing once, then try again.'));
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
    } catch { status.textContent = 'Browser status is unavailable. Retrying…'; }
    finally { refreshing = false; }
  }
  window.addEventListener('browser-daemon-refresh-extensions', refreshBrowsers);
  void refreshBrowsers();
  setInterval(refreshBrowsers, 5000);
  button.addEventListener('click', async () => {
    button.disabled = true;
    status.textContent = 'Looking for the browser extension…';
    try {
      const browser = await extension('discover');
      if (browser.connected) {
        status.textContent = 'This browser is already connected and listed in Nodes. Choose shared tabs in the extension.';
        return;
      }
      const response = await fetch('/api/extension/pair', {
        method: 'POST', credentials: 'same-origin', cache: 'no-store',
        headers: {'Content-Type': 'application/json'}, body: JSON.stringify({origin: browser.origin, remember: document.getElementById('rememberBrowser')?.checked === true}),
      });
      if (!response.ok) throw new Error(`Pairing request failed (${response.status}). Sign in to this HTTPS dashboard and try again.`);
      const pair = await response.json();
      await extension('approve', {code: pair.code, remember: pair.remember});
      status.textContent = 'Approve the connection in the extension tab that just opened, then select the tabs to share.';
    } catch (error) { status.textContent = error.message; }
    finally { button.disabled = false; }
  });
})();
