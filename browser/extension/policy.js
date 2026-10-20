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
  // The live view's protocol: frames and page state for every viewer, and the
  // page's own input, navigation and dialogs only under a control grant.
  const viewing = new Set([
    'Page.enable', 'Page.bringToFront', 'Page.startScreencast', 'Page.stopScreencast', 'Page.screencastFrameAck',
    'Page.captureScreenshot', 'Page.getLayoutMetrics', 'Page.getNavigationHistory', 'Page.getFrameTree',
    'Accessibility.getFullAXTree', 'Accessibility.getPartialAXTree',
  ]);
  const steering = new Set(['Page.reload', 'Page.navigateToHistoryEntry', 'Page.handleJavaScriptDialog']);
  function webURL(value) {
    try {
      const u = new URL(value);
      return ['http:', 'https:'].includes(u.protocol) && !u.username && !u.password;
    } catch { return false; }
  }
  // A web page, or the blank page a CDP client opens before navigating.
  const pageURL = value => value === 'about:blank' || webURL(value);
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
    if (command.action === 'navigate' || command.action === 'open') return pageURL(command.params.url);
    if (command.action === 'cdp') return methods.has(command.params.method);
    return ['click', 'type', 'key', 'scroll', 'reload', 'back', 'forward', 'focus', 'close', 'evaluate', 'frames', 'fill'].includes(command.action);
  }
  function relayAllowed(method, params, control) {
    if (typeof method !== 'string' || !params || typeof params !== 'object' || Array.isArray(params) ||
        JSON.stringify(params).length > 128 * 1024) return false;
    if (viewing.has(method)) return true;
    if (!control) return false;
    if (method === 'Page.navigate') return pageURL(params.url);
    return steering.has(method) || methods.has(method);
  }
  // CDP clients (Playwright's connectOverCDP, via the daemon) in a tab shared with
  // control: the page's own domains, never the profile's. No cookies or cache of
  // other sites, no requests with the browser's credentials outside the page, no
  // files from the owner's disk, no downloads to it. Same as automation_allowed.
  const automationDenied = new Set([
    'Page.setDownloadBehavior', 'Page.crash', 'Network.getAllCookies', 'Network.getCookies',
    'Network.setCookie', 'Network.setCookies', 'Network.deleteCookies', 'Network.clearBrowserCookies',
    'Network.clearBrowserCache', 'Network.loadNetworkResource', 'DOM.setFileInputFiles',
  ]);
  const automationDomains = new Set(['Page', 'Runtime', 'DOM', 'Input', 'Emulation', 'Network', 'Fetch', 'Log',
    'Accessibility', 'CSS', 'Overlay', 'Performance', 'DOMSnapshot']);
  function automationAllowed(method, params) {
    if (typeof method !== 'string' || !params || typeof params !== 'object' || Array.isArray(params) ||
        JSON.stringify(params).length > 8 * 1024 * 1024 || automationDenied.has(method)) return false;
    if (method === 'Page.navigate') return pageURL(params.url);
    if (method === 'IO.read' || method === 'IO.close') return true;
    return automationDomains.has(method.split('.')[0]);
  }
  // Events a view-only viewer may see: the page's frames and navigation state.
  const viewEvent = method => method.startsWith('Page.') || method === 'Inspector.detached';
  return {webURL, pageURL, serverURL, daemonOrigins, approvedServer, allowed, relayAllowed, automationAllowed, viewEvent};
})();
