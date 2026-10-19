/* Shared with the background worker and the dependency-free security checks. */
globalThis.NodePolicy = (() => {
  const methods = new Set([
    'Runtime.evaluate', 'Runtime.callFunctionOn', 'Runtime.getProperties',
    'Runtime.releaseObject', 'Runtime.releaseObjectGroup',
    'DOM.getDocument', 'DOM.querySelector', 'DOM.querySelectorAll', 'DOM.describeNode',
    'DOM.resolveNode', 'DOM.getBoxModel', 'DOM.focus', 'DOM.scrollIntoViewIfNeeded',
    'Input.dispatchMouseEvent', 'Input.dispatchKeyEvent', 'Input.insertText',
    'Input.dispatchTouchEvent', 'Input.synthesizeScrollGesture',
    'Accessibility.getFullAXTree', 'Accessibility.getPartialAXTree',
    'Page.getFrameTree', 'Page.getLayoutMetrics', 'Page.captureScreenshot',
  ]);
  function webURL(value) {
    try {
      const u = new URL(value);
      return ['http:', 'https:'].includes(u.protocol) && !u.username && !u.password;
    } catch { return false; }
  }
  function serverURL(value) {
    const u = new URL(value);
    if (u.hostname.includes('*') || u.username || u.password || u.search || u.hash || !['', '/'].includes(u.pathname)) {
      throw new Error('Use a daemon base URL without credentials, path, query or fragment.');
    }
    if (u.protocol !== 'https:' && !(u.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(u.hostname))) {
      throw new Error('Use HTTPS (HTTP is allowed only on loopback).');
    }
    return u.origin;
  }
  function daemonOrigins(value) {
    if (!Array.isArray(value) || value.length > 128 || value.some(v => typeof v !== 'string')) throw new Error('Daemon allowlist must be a list of exact origins.');
    return [...new Set(value.map(v => serverURL(v.trim())))];
  }
  function approvedServer(value, origins) {
    const server = serverURL(value);
    if (!daemonOrigins(origins).includes(server)) throw new Error('Daemon URL is not on the allowlist. Open Allowed daemon URLs in extension setup.');
    return server;
  }
  function allowed(command, control) {
    if (!command || !Number.isSafeInteger(command.tab) || !command.params ||
        typeof command.params !== 'object' || Array.isArray(command.params) ||
        JSON.stringify(command.params).length > 128 * 1024) return false;
    if (['screenshot', 'snapshot'].includes(command.action)) return true;
    if (!control) return false;
    if (command.action === 'navigate') return webURL(command.params.url);
    if (command.action === 'cdp') return methods.has(command.params.method);
    return ['click', 'type', 'key', 'scroll', 'reload', 'back', 'forward', 'focus', 'close', 'evaluate'].includes(command.action);
  }
  return {webURL, serverURL, daemonOrigins, approvedServer, allowed};
})();
