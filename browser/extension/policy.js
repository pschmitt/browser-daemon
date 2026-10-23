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
    'Page.startAudio', 'Page.stopAudio', 'Page.enable', 'Page.bringToFront', 'Page.startScreencast', 'Page.stopScreencast', 'Page.screencastFrameAck',
    'Page.captureScreenshot', 'Page.getLayoutMetrics', 'Page.getNavigationHistory', 'Page.getFrameTree',
    'Accessibility.getFullAXTree', 'Accessibility.getPartialAXTree',
  ]);
  const steering = new Set(['Page.startFiles', 'Page.stopFiles', 'Page.fileTarget', 'Page.beginFile', 'Page.fileChunk', 'Page.cancelFile', 'Page.applyFiles', 'Page.listDownloads', 'Page.readDownload', 'Page.removeDownload', 'Page.reload', 'Page.navigateToHistoryEntry', 'Page.handleJavaScriptDialog']);
  function webURL(value) {
    try {
      const u = new URL(value);
      return ['http:', 'https:'].includes(u.protocol) && !u.username && !u.password;
    } catch { return false; }
  }
  // A web page, or the blank page a CDP client opens before navigating.
  const pageURL = value => value === 'about:blank' || webURL(value);
  // URL normalizes unusual IPv4 spellings before we inspect the address.
  function localHost(host) {
    host = host.toLowerCase().replace(/\.$/, '');
    return host === 'localhost' || host.endsWith('.localhost') || /^127\./.test(host) ||
      host === '0.0.0.0' || host === '[::]' || host === '[::1]' ||
      /^\[::ffff:(?:127\.|7f[0-9a-f]{2}:)/.test(host);
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
    if (['screenshot', 'snapshot', 'publicAudio'].includes(command.action)) return true;
    if (!control) return false;
    if (command.action === 'navigate' || command.action === 'open') return pageURL(command.params.url);
    if (command.action === 'move') return Number.isSafeInteger(command.params.index) && command.params.index >= 0;
    if (command.action === 'pointerMove') return Number.isFinite(command.params.x) && Number.isFinite(command.params.y);
    if (command.action === 'pin') return typeof command.params.pinned === 'boolean';
    if (command.action === 'cdp') return methods.has(command.params.method);
    if (command.action === 'suggest') return typeof command.params.query === 'string' && command.params.query.length <= 200;
    return ['publicInput', 'click', 'type', 'key', 'scroll', 'reload', 'back', 'forward', 'focus', 'close', 'evaluate', 'frames', 'fill'].includes(command.action);
  }
  function relayAllowed(method, params, control) {
    if (typeof method !== 'string' || !params || typeof params !== 'object' || Array.isArray(params) ||
        JSON.stringify(params).length > 128 * 1024) return false;
    if (viewing.has(method)) return true;
    if (!control) return false;
    if (method === 'Page.reload' && params.scriptToEvaluateOnLoad !== undefined) return false;
    if (method === 'Page.navigate') return pageURL(params.url);
    return steering.has(method) || methods.has(method);
  }
  // CDP clients (Playwright's connectOverCDP, via the daemon) in a tab shared with
  // control: the page's own domains, never the profile's. No cookies or cache of
  // other sites, no requests with the browser's credentials outside the page, no
  // files from the owner's disk, no downloads to it. Same as automation_allowed.
  const automationMethods = new Set([
    'Page.enable',
    'Page.disable',
    'Page.getFrameTree',
    'Page.getLayoutMetrics',
    'Page.getNavigationHistory',
    'Page.navigateToHistoryEntry',
    'Page.navigate',
    'Page.reload',
    'Page.stopLoading',
    'Page.bringToFront',
    'Page.captureScreenshot',
    'Page.captureSnapshot',
    'Page.printToPDF',
    'Page.handleJavaScriptDialog',
    'Page.addScriptToEvaluateOnNewDocument',
    'Page.removeScriptToEvaluateOnNewDocument',
    'Page.createIsolatedWorld',
    'Page.setLifecycleEventsEnabled',
    'Page.setInterceptFileChooserDialog',
    'Page.setBypassCSP',
    'Page.startScreencast',
    'Page.stopScreencast',
    'Page.screencastFrameAck',
    'Runtime.enable',
    'Runtime.disable',
    'Runtime.evaluate',
    'Runtime.callFunctionOn',
    'Runtime.awaitPromise',
    'Runtime.getProperties',
    'Runtime.releaseObject',
    'Runtime.releaseObjectGroup',
    'Runtime.addBinding',
    'Runtime.removeBinding',
    'Runtime.runIfWaitingForDebugger',
    'Runtime.discardConsoleEntries',
    'DOM.enable',
    'DOM.disable',
    'DOM.getDocument',
    'DOM.getFlattenedDocument',
    'DOM.querySelector',
    'DOM.querySelectorAll',
    'DOM.describeNode',
    'DOM.resolveNode',
    'DOM.requestNode',
    'DOM.getBoxModel',
    'DOM.getContentQuads',
    'DOM.getNodeForLocation',
    'DOM.getOuterHTML',
    'DOM.focus',
    'DOM.scrollIntoViewIfNeeded',
    'DOM.setNodeValue',
    'DOM.setAttributeValue',
    'DOM.removeAttribute',
    'DOM.collectClassNamesFromSubtree',
    'Input.dispatchMouseEvent',
    'Input.dispatchKeyEvent',
    'Input.insertText',
    'Input.dispatchTouchEvent',
    'Input.synthesizeScrollGesture',
    'Input.setIgnoreInputEvents',
    'Emulation.setDeviceMetricsOverride',
    'Emulation.clearDeviceMetricsOverride',
    'Emulation.setEmulatedMedia',
    'Emulation.setTouchEmulationEnabled',
    'Emulation.setFocusEmulationEnabled',
    'Emulation.setGeolocationOverride',
    'Emulation.clearGeolocationOverride',
    'Emulation.setUserAgentOverride',
    'Emulation.setLocaleOverride',
    'Emulation.setTimezoneOverride',
    'Emulation.setScriptExecutionDisabled',
    'Emulation.setCPUThrottlingRate',
    'Emulation.setDefaultBackgroundColorOverride',
    'Emulation.setEmulatedVisionDeficiency',
    'Emulation.setAutomationOverride',
    'Emulation.setIdleOverride',
    'Emulation.clearIdleOverride',
    'Network.enable',
    'Network.disable',
    'Network.getResponseBody',
    'Network.getRequestPostData',
    'Network.setExtraHTTPHeaders',
    'Network.setCacheDisabled',
    'Network.setBlockedURLs',
    'Network.setBypassServiceWorker',
    'Network.emulateNetworkConditions',
    'Network.setUserAgentOverride',
    'Fetch.enable',
    'Fetch.disable',
    'Fetch.failRequest',
    'Fetch.fulfillRequest',
    'Fetch.continueRequest',
    'Fetch.continueResponse',
    'Fetch.continueWithAuth',
    'Fetch.getResponseBody',
    'Fetch.takeResponseBodyAsStream',
    'Log.enable',
    'Log.disable',
    'Log.clear',
    'Accessibility.enable',
    'Accessibility.disable',
    'Accessibility.getFullAXTree',
    'Accessibility.getPartialAXTree',
    'CSS.enable',
    'CSS.disable',
    'CSS.getComputedStyleForNode',
    'CSS.getMatchedStylesForNode',
    'CSS.getStyleSheetText',
    'CSS.setStyleSheetText',
    'CSS.getInlineStylesForNode',
    'Overlay.enable',
    'Overlay.disable',
    'Overlay.hideHighlight',
    'Overlay.highlightNode',
    'Overlay.highlightRect',
    'Overlay.setShowViewportSizeOnResize',
    'Performance.enable',
    'Performance.disable',
    'Performance.getMetrics',
    'DOMSnapshot.captureSnapshot',
    'IO.read',
    'IO.close',
  ]);
  function automationAllowed(method, params) {
    if (typeof method !== 'string' || !params || typeof params !== 'object' || Array.isArray(params) ||
        JSON.stringify(params).length > 8 * 1024 * 1024 || !automationMethods.has(method)) return false;
    if (method === 'Page.navigate') return pageURL(params.url);
    // Playwright creates its utility world with an empty script. Executable
    // initialization scripts would survive a later navigation to a private page.
    if (method === 'Page.addScriptToEvaluateOnNewDocument') return params.source === '' &&
      typeof params.worldName === 'string' && params.worldName.length > 0;
    if (method === 'Page.reload' && params.scriptToEvaluateOnLoad !== undefined) return false;
    // Interception must not redirect requests to local services or daemon pages.
    // The background applies the current daemon allowlist to these URLs too.
    if (method === 'Fetch.continueRequest' && params.url !== undefined) return pageURL(params.url);
    return true;
  }
  // Events a view-only viewer may see: the page's frames and navigation state.
  const viewEvent = method => method.startsWith('Page.') || method === 'Inspector.detached';
  return {webURL, pageURL, localHost, serverURL, daemonOrigins, approvedServer, allowed, relayAllowed, automationAllowed, viewEvent};
})();
