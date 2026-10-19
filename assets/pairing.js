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
  const summary = document.getElementById('temporaryBrowserSummary');
  let refreshing = false;
  async function refreshBrowsers() {
    if (!summary || refreshing || document.hidden) return;
    refreshing = true;
    try {
      const mesh = await fetch('/api/mesh/overview', {cache: 'no-store', signal: AbortSignal.timeout(8000)}).then(r => r.json());
      const bases = ['/api', ...mesh.peers.filter(p => p.status === 'online').map(p => `/api/peers/${encodeURIComponent(p.name)}`)];
      const reports = await Promise.allSettled(bases.map(async base => {
        const response = await fetch(`${base}/extension/nodes`, {cache: 'no-store', signal: AbortSignal.timeout(8000)});
        if (!response.ok) throw new Error();
        return (await response.json()).nodes.map(node => ({...node, daemon: base === '/api' ? '' : decodeURIComponent(base.slice('/api/peers/'.length))}));
      }));
      const nodes = reports.flatMap(r => r.status === 'fulfilled' ? r.value : []);
      window.dispatchEvent(new CustomEvent('browser-daemon-extension-nodes', {detail: nodes}));
      summary.replaceChildren();
      const sessions = document.getElementById('sharedBrowserSessions');
      if (sessions) sessions.replaceChildren();
      if (!nodes.length) {
        const empty = document.createElement('p');
        empty.textContent = 'No browser extensions connected. Use Pair browser extension to connect this browser.';
        summary.append(empty);
      }
      const viewURL = (node, tab) => `/extension/?browser=${encodeURIComponent(node.id)}&daemon=${encodeURIComponent(node.daemon)}${tab ? `&tab=${tab.id}` : ''}`;
      for (const node of nodes) {
        const card = document.createElement('article'); card.className = 'node-card';
        const heading = document.createElement('h3'); heading.textContent = node.name;
        const detail = document.createElement('p');
        detail.textContent = `${node.browser} browser extension · ${node.tabs.length} shared tab(s) · ${node.online === false ? 'offline' : 'connected'}`;
        const link = document.createElement('a'); link.className = 'btn'; link.href = viewURL(node);
        link.textContent = node.tabs.length ? 'Open shared tabs' : 'No shared tabs — choose tabs in the extension';
        card.append(heading, detail, link); summary.append(card);
        for (const tab of node.tabs) {
          if (!sessions) break;
          if (typeof filter === 'string' && filter !== 'all' && (node.daemon || mesh.local_node.name) !== filter) continue;
          if (typeof agentsOnly !== 'undefined' && agentsOnly) continue;
          if (typeof sessQuery === 'string' && sessQuery && !`${node.name} ${tab.title || ''} ${tab.url}`.toLowerCase().includes(sessQuery.toLowerCase())) continue;
          const shared = document.createElement('article'); shared.className = 'node-card';
          const title = document.createElement('h3'); title.textContent = tab.title || 'Shared tab';
          const owner = document.createElement('p'); owner.textContent = `${node.name} · ${tab.control ? 'Remote control enabled' : 'View only'}`;
          const open = document.createElement('a'); open.className = 'btn primary'; open.href = viewURL(node, tab); open.textContent = 'Open shared tab';
          shared.append(title, owner, open); sessions.append(shared);
        }
      }
    } catch { summary.textContent = 'Could not load temporary browsers. Open Temporary browsers for connection details.'; }
    finally { refreshing = false; }
  }
  void refreshBrowsers();
  setInterval(refreshBrowsers, 5000);
  button.addEventListener('click', async () => {
    button.disabled = true;
    status.textContent = 'Looking for the browser extension…';
    try {
      const browser = await extension('discover');
      if (browser.connected) {
        status.textContent = 'This browser is connected. Open Temporary browsers to see it; choose tabs in the extension to share them.';
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
