/* The handoff view uses the same driver contract for managed and shared browsers. */
globalThis.ExtensionDriver = class ExtensionDriver {
  constructor(node, page, daemon, handlers, options = {}) {
    this.node = node; this.page = String(page); this.h = handlers; this.options = options;
    this.base = daemon ? `/api/peers/${encodeURIComponent(daemon)}` : '/api';
    this.open = true; this.queue = Promise.resolve(); this.ready = Promise.resolve();
  }
  isOpen() { return this.open; }
  close() { this.open = false; clearTimeout(this.timer); }
  async command(action, params = {}) {
    if (!this.open) throw new Error('View closed');
    const response = await fetch(`${this.base}/extension/nodes/${encodeURIComponent(this.node.id)}/command`, {
      method: 'POST', headers: {...this.options.headers?.(), 'Content-Type': 'application/json'},
      body: JSON.stringify({tab: Number(this.page), action, params}), signal: AbortSignal.timeout(15000),
    });
    if (!response.ok) throw new Error(response.status === 403 ? 'Sharing or control permission ended. Approve this tab again in the browser extension.' : 'The browser is unavailable or stopped sharing this tab.');
    const body = await response.json();
    if (body.error) throw new Error('The browser could not complete that action.');
    return body.result;
  }
  get tab() { return this.node.tabs.find(t => String(t.id) === this.page); }
  get control() { return this.tab?.control === true; }
  send(action, params) {
    if (!this.control) return;
    this.lastInput = performance.now();
    this.queue = this.queue.then(() => this.open && this.command(action, params)).catch(error => this.options.error?.(error.message));
  }
  async start() { await this.frame(); this.loop(); }
  async frame() {
    const r = await this.command('screenshot');
    if (!this.open) return;
    if (!['jpeg', 'png'].includes(r?.format) || typeof r.data !== 'string') throw new Error('Invalid browser screenshot');
    let width = r.width, height = r.height;
    if (!(width > 0 && height > 0)) {
      const img = new Image(); img.src = `data:image/${r.format};base64,${r.data}`; await img.decode();
      width = img.naturalWidth; height = img.naturalHeight;
    }
    await this.h.frame(r.data, {deviceWidth: width, deviceHeight: height, pageScaleFactor: 1, offsetTop: 0}, `image/${r.format}`);
  }
  loop() {
    this.timer = setTimeout(async () => {
      if (!this.open) return;
      try { if (!document.hidden) await this.frame(); }
      catch (error) { this.close(); this.h.gone(error.message); return; }
      this.loop();
    }, performance.now() - (this.lastInput || 0) < 1500 ? 150 : 700);
  }
  async applyMetrics() {} // A shared browser keeps its owner's window size.
  async setVideo() {}
  async historyState() { return this.control ? null : {back: false, fwd: false}; }
  async evaluate(expression) {
    if (!this.control) throw new Error('View only');
    if (this.node.browser === 'chromium') {
      const r = await this.command('cdp', {method: 'Runtime.evaluate', params: {expression, returnByValue: true}});
      return r?.result?.value;
    }
    return this.command('evaluate', {script: `return (${expression});`, args: []});
  }
  insertText(text) { this.send('type', {text}); }
  pressKey(key, mods = {}) {
    if (this.node.browser !== 'chromium') { this.send('key', {key, modifiers: mods}); return; }
    const modifiers = (mods.alt ? 1 : 0) | (mods.ctrl ? 2 : 0) | (mods.meta ? 4 : 0) | (mods.shift ? 8 : 0);
    const codes = {Enter: 13, Tab: 9, Escape: 27, Backspace: 8, Delete: 46, ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Home: 36, End: 35, PageUp: 33, PageDown: 34};
    const base = {key, modifiers, windowsVirtualKeyCode: codes[key] || key.toUpperCase().charCodeAt(0)};
    const text = !mods.ctrl && !mods.alt && !mods.meta && (key === 'Enter' ? '\r' : key === 'Tab' ? '\t' : '');
    this.send('cdp', {method: 'Input.dispatchKeyEvent', params: {...base, type: text ? 'keyDown' : 'rawKeyDown', ...(text ? {text} : {})}});
    this.send('cdp', {method: 'Input.dispatchKeyEvent', params: {...base, type: 'keyUp'}});
  }
  shortcut(key, shift) { this.pressKey(key, {ctrl: true, shift}); }
  mouse(type, x, y, button = 0, clicks = 1, down = false) {
    if (this.node.browser !== 'chromium') {
      if (type === 'mousePressed') this.pointer = {x, y, button, path: []};
      else if (type === 'mouseMoved' && down && this.pointer) {
        this.pointer.path.push({x, y});
        if (this.pointer.path.length > 127) this.pointer.path.splice(1, 1);
      } else if (type === 'mouseReleased' && this.pointer) {
        const pointer = this.pointer; this.pointer = null;
        this.send('click', {...pointer, path: [...pointer.path, {x, y}]});
      }
      return;
    }
    this.send('cdp', {method: 'Input.dispatchMouseEvent', params: {type, x, y, button: type === 'mouseMoved' && !down ? 'none' : ['left', 'middle', 'right'][button], buttons: type === 'mouseReleased' || !down && type === 'mouseMoved' ? 0 : [1, 4, 2][button], clickCount: clicks}});
  }
  click(x, y) { this.send('click', {x, y}); }
  mouseDown(x, y, clicks, button) { this.mouse('mousePressed', x, y, button, clicks, true); }
  mouseMove(x, y, down, button) { this.mouse('mouseMoved', x, y, button, 0, down); }
  mouseUp(x, y, clicks, button) { this.mouse('mouseReleased', x, y, button, clicks); }
  wheel(x, y, dx, dy) { this.send('scroll', {x: dx, y: dy}); }
  touch(type, x, y) {
    if (this.node.browser === 'chromium') this.send('cdp', {method: 'Input.dispatchTouchEvent', params: {type, touchPoints: x === undefined ? [] : [{x, y}]}});
    else if (type === 'touchStart') { this.touchStart = {x, y}; this.touchMoved = false; }
    else if (type === 'touchMove' && this.touchStart) { this.touchMoved = true; this.wheel(x, y, this.touchStart.x - x, this.touchStart.y - y); this.touchStart = {x, y}; }
    else if (type === 'touchEnd' && this.touchStart) { if (!this.touchMoved) this.click(this.touchStart.x, this.touchStart.y); this.touchStart = null; }
  }
  async navigate(url) { if (this.control) return this.command('navigate', {url}); }
  reload() { this.send('reload'); }
  async history(delta) { if (this.control) return this.command(delta < 0 ? 'back' : 'forward'); }
  async dropFiles() { throw new Error('File upload is not available for shared browsers.'); }
};

// Keep old bookmarks working; all browsing and control now use the regular view.
if (location.pathname.startsWith('/extension/')) {
  const query = new URLSearchParams(location.search);
  if (query.has('browser')) {
    const target = new URLSearchParams({browser: query.get('browser'), daemon: query.get('daemon') || ''});
    const page = query.get('tab') || query.get('page'); if (page) target.set('page', page);
    location.replace('/handoff/?' + target);
  } else location.replace('/#nodes');
}
