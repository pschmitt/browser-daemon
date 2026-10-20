/* Isolated page world: only viewer-provided bytes become Files. No host paths. */
function filePageBridge(p) {
  const api = globalThis.browser || chrome;
  let s = globalThis.__bdFileState;
  if (p.op === 'init') {
    if (s?.cap === p.cap) { s.until = Date.now() + 20000; return; }
    s?.stop();
    s = {cap: p.cap, until: Date.now() + 20000, armed: 0, uploads: new Map(), targets: new Map(), controllers: new Set(), stopped: false};
    globalThis.__bdFileState = s;
    const encode = bytes => { let raw = ''; for (let i = 0; i < bytes.length; i += 8192) raw += String.fromCharCode(...bytes.subarray(i, i + 8192)); return btoa(raw); };
    const emit = message => api.runtime.sendMessage({type: 'fileBridge', cap: s.cap, ...message});
    s.remember = (id, element, x = null, y = null) => {
      if (s.targets.size >= 64) s.targets.delete(s.targets.keys().next().value);
      s.targets.set(id, {element, x, y});
    };
    const find = (x, y) => {
      let element = document.elementFromPoint(x, y);
      for (let depth = 0; depth < 64 && element?.shadowRoot; depth++) {
        const next = element.shadowRoot.elementFromPoint(x, y);
        if (!next || next === element) break;
        element = next;
      }
      return element;
    };
    s.locate = (nonce, x, y) => {
      const element = find(x, y);
      if (!element) return;
      if (element instanceof HTMLIFrameElement) {
        const r = element.getBoundingClientRect();
        element.contentWindow?.postMessage({bdFileTarget: nonce, x: (x - r.left) * element.offsetWidth / r.width - element.clientLeft, y: (y - r.top) * element.offsetHeight / r.height - element.clientTop}, '*');
        return;
      }
      s.remember(nonce, element, x, y);
      void emit({event: 'target', request: nonce}).catch(() => {});
    };
    const locate = e => {
      if (e.source !== parent || typeof e.data?.bdFileTarget !== 'string' || !/^[a-f0-9-]{36}$/.test(e.data.bdFileTarget) ||
          !Number.isFinite(e.data.x) || !Number.isFinite(e.data.y)) return;
      s.locate(e.data.bdFileTarget, e.data.x, e.data.y);
    };
    const click = e => {
      if (Date.now() > s.armed || s.stopped) return;
      const input = e.composedPath().find(n => n instanceof HTMLInputElement && n.type === 'file');
      if (input) {
        e.preventDefault();
        const request = crypto.randomUUID(); s.remember(request, input);
        void emit({event: 'chooser', request, multiple: input.multiple, accept: input.accept}).catch(() => {});
        return;
      }
      const anchor = e.composedPath().find(n => n instanceof HTMLAnchorElement && n.hasAttribute('download'));
      if (!anchor) return;
      const url = new URL(anchor.href, location.href);
      if (!['blob:', 'data:'].includes(url.protocol) && url.origin !== location.origin) return;
      e.preventDefault();
      const id = crypto.randomUUID();
      void (async () => {
        let reader;
        const controller = new AbortController(); s.controllers.add(controller);
        try {
          // Reserve the download during the viewer gesture, before headers arrive.
          const start = await emit({event: 'downloadBegin', id, name: anchor.download || 'download', url: location.href, size: null});
          if (!start?.ok) throw new Error('Sharing ended');
          const response = await fetch(url.href, {credentials: 'same-origin', signal: controller.signal});
          if (!response.ok) throw new Error('Download failed');
          const size = Number(response.headers.get('content-length')) || null;
          if (size > 1073741824) throw new Error('Maximum file size is 1 GiB');
          reader = response.body.getReader();
          while (!s.stopped) {
            const {value, done} = await reader.read();
            if (done) break;
            for (let i = 0; i < value.length; i += 65536) {
              const data = encode(value.subarray(i, i + 65536));
              if (!(await emit({event: 'downloadChunk', id, data}))?.ok) throw new Error('Sharing ended');
            }
          }
          await emit({event: s.stopped ? 'downloadCancel' : 'downloadEnd', id});
        } catch { await emit({event: 'downloadCancel', id}).catch(() => {}); }
        finally { s.controllers.delete(controller); await reader?.cancel().catch(() => {}); }
      })();
    };
    const timer = setInterval(() => { if (Date.now() > s.until) s.stop(); }, 1000);
    s.stop = () => {
      s.stopped = true; clearInterval(timer);
      document.removeEventListener('click', click, true); removeEventListener('message', locate); removeEventListener('pagehide', s.stop);
      for (const controller of s.controllers) controller.abort();
      s.controllers.clear(); s.uploads.clear(); s.targets.clear();
      if (globalThis.__bdFileState === s) delete globalThis.__bdFileState;
    };
    document.addEventListener('click', click, true); addEventListener('message', locate); addEventListener('pagehide', s.stop, {once: true});
    return;
  }
  if (!s || s.stopped || s.cap !== p.cap) throw new Error('The shared page changed');
  s.until = Date.now() + 20000;
  if (p.op === 'stop') { s.stop(); return; }
  if (p.op === 'arm') { s.armed = Date.now() + 5000; return; }
  if (p.op === 'locate') { s.locate(p.request, p.x, p.y); return; }
  if (p.op === 'mark') {
    const roots = [document]; let input;
    for (let i = 0; i < roots.length && !input; i++) {
      input = roots[i].querySelector(`[data-bd-file="${p.request}"]`);
      if (!input) for (const node of roots[i].querySelectorAll('*')) if (node.shadowRoot && roots.length < 64) roots.push(node.shadowRoot);
    }
    if (input instanceof HTMLInputElement && input.type === 'file') {
      input.removeAttribute('data-bd-file'); s.remember(p.request, input);
      void api.runtime.sendMessage({type: 'fileBridge', cap: s.cap, event: 'chooser', request: p.request, multiple: input.multiple, accept: input.accept});
    }
    return;
  }
  if (p.op === 'begin') {
    if (s.uploads.size >= 32 || !s.targets.get(p.target)?.element?.isConnected) throw new Error('Upload target changed');
    s.uploads.set(p.id, {name: p.name, type: p.type, size: p.size, lastModified: p.lastModified, parts: [], received: 0});
    return;
  }
  const upload = s.uploads.get(p.id);
  if (p.op === 'chunk') {
    if (!upload || p.offset !== upload.received) throw new Error('Invalid upload offset');
    const data = Uint8Array.from(atob(p.data), c => c.charCodeAt(0));
    if (upload.received + data.length > upload.size) throw new Error('Upload exceeds its size');
    upload.parts.push(data); upload.received += data.length;
    return;
  }
  if (p.op === 'cancel') { s.uploads.delete(p.id); return; }
  if (p.op === 'apply') {
    const target = s.targets.get(p.target);
    if (!target?.element?.isConnected) throw new Error('Upload target changed');
    const transfer = new DataTransfer();
    for (const id of p.files) {
      const file = s.uploads.get(id);
      if (!file || file.received !== file.size) throw new Error('Upload is incomplete');
      transfer.items.add(new File(file.parts, file.name, {type: file.type, lastModified: file.lastModified}));
    }
    if (target.element instanceof HTMLInputElement && target.element.type === 'file') {
      if (!target.element.multiple && transfer.files.length > 1) throw new Error('This field accepts one file');
      target.element.files = transfer.files;
      target.element.dispatchEvent(new Event('input', {bubbles: true, composed: true}));
      target.element.dispatchEvent(new Event('change', {bubbles: true}));
    } else {
      const x = Number.isFinite(target.x) ? target.x : target.element.getBoundingClientRect().left + target.element.getBoundingClientRect().width / 2;
      const y = Number.isFinite(target.y) ? target.y : target.element.getBoundingClientRect().top + target.element.getBoundingClientRect().height / 2;
      for (const type of ['dragenter', 'dragover', 'drop']) target.element.dispatchEvent(new DragEvent(type, {
        bubbles: true, composed: true, cancelable: true, dataTransfer: transfer, clientX: x, clientY: y,
      }));
    }
    for (const id of p.files) s.uploads.delete(id);
    s.targets.delete(p.target);
    return {count: transfer.files.length};
  }
  throw new Error('Unknown file operation');
}
