// Installed only on the configured daemon after the user grants site access.
// This bridge can request a pairing approval; it cannot share tabs or control them.
(() => {
  if (globalThis.__browserDaemonPairBridge) return;
  globalThis.__browserDaemonPairBridge = true;
  const api = globalThis.browser || chrome;
  window.addEventListener('message', async event => {
    const data = event.data;
    if (event.source !== window || event.origin !== location.origin ||
        data?.source !== 'browser-daemon-dashboard' || typeof data.id !== 'string' || data.id.length > 80 ||
        !['discover', 'approve'].includes(data.action)) return;
    try {
      const result = await api.runtime.sendMessage({type: 'dashboardPair', action: data.action, code: data.code, remember: data.remember});
      window.postMessage({source: 'browser-daemon-extension', id: data.id, ...result}, location.origin);
    } catch {
      window.postMessage({source: 'browser-daemon-extension', id: data.id, error: 'Open the extension to enable dashboard pairing again.'}, location.origin);
    }
  });
})();
