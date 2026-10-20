/* Tab-scoped file relay. Buffers contain only uploads supplied by a viewer or
 * new downloads initiated in a controlling view; never files from the host. */
const FILE_LIMIT = 1024 * 1024 * 1024;
const FILE_CACHE_LIMIT = 2 * 1024 * 1024 * 1024;
const FILE_CHUNK = 65536;
const fileTabs = new Map();
const fileWaiters = new Map();
const fileMethods = new Set(['Page.startFiles', 'Page.stopFiles', 'Page.fileTarget', 'Page.beginFile', 'Page.fileChunk', 'Page.cancelFile', 'Page.applyFiles', 'Page.listDownloads', 'Page.readDownload', 'Page.removeDownload']);
const fileEncode = bytes => { let raw = ''; for (let i = 0; i < bytes.length; i += 8192) raw += String.fromCharCode(...bytes.subarray(i, i + 8192)); return btoa(raw); };
const fileName = name => String(name || 'download').replace(/[\\/\x00-\x1f\x7f]/g, '_').slice(0, 200) || 'download';
function fileBytes() {
  let bytes = 0;
  for (const s of fileTabs.values()) {
    for (const u of s.uploads.values()) bytes += u.size;
    for (const d of s.downloads.values()) bytes += d.received_bytes;
  }
  return bytes;
}
function fileEvent(tab, method, params) {
  const s = fileTabs.get(tab);
  if (!s || !sharing.files || shared.get(tab)?.paused !== false || !shared.get(tab)?.control) return;
  for (const channel of s.channels) if (channels.get(channel) === tab) sendEvent(tab, method, params, channel);
}
const publicFile = d => ({id: d.id, name: d.name, url: d.url, state: d.state, total_bytes: d.total_bytes,
  received_bytes: d.received_bytes, started_at_secs: d.started_at_secs, finished_at_secs: d.finished_at_secs});
async function filePage(tab, frame, p) {
  const s = fileTabs.get(tab);
  if (!s || (p.op !== 'stop' && (!sharing.files || !shared.get(tab)?.control || shared.get(tab)?.paused !== false))) throw new ShimError('File sharing ended.');
  const result = await api.scripting.executeScript({target: {tabId: tab, ...(frame === null ? {allFrames: true} : {frameIds: [frame]})},
    func: filePageBridge, args: [{...p, cap: s.cap}]});
  if (p.op !== 'stop' && (fileTabs.get(tab) !== s || !sharing.files || !shared.get(tab)?.control)) throw new ShimError('File sharing ended.');
  return result;
}
async function installFilePages(tab) {
  try { await filePage(tab, null, {op: 'init'}); } catch { /* A navigation can replace a frame between renewals. */ }
}
async function startFileChannel(tab, channel) {
  if (!sharing.files) throw new ShimError('Enable File transfer in the browser extension first.');
  let s = fileTabs.get(tab);
  if (!s) {
    s = {cap: crypto.randomUUID(), channels: new Set(), uploads: new Map(), targets: new Map(), downloads: new Map(),
      until: 0, inputChannel: null, requests: new Map(), network: false, automationFetch: false};
    fileTabs.set(tab, s);
    s.timer = setInterval(() => {
      if (!registration || !sharing.files || !shared.get(tab)?.control || shared.get(tab)?.paused !== false) { stopFileTab(tab); return; }
      for (const [id, r] of s.requests) if (r.until < Date.now()) s.requests.delete(id);
      for (const [id, d] of s.downloads) if (Date.now() - d.touched > 15 * 60 * 1000) { s.downloads.delete(id); fileEvent(tab, 'Page.downloadProgress', {id, removed: true}); }
      for (const [id, u] of s.uploads) if (Date.now() - u.touched > 5 * 60 * 1000) void cancelFile(tab, id);
      for (const [id, t] of s.targets) if (Date.now() - t.touched > 30 * 60 * 1000) s.targets.delete(id);
      if (s.channels.size) void installFilePages(tab);
    }, 5000);
  }
  s.channels.add(channel);
  try {
    await filePage(tab, null, {op: 'init'});
    if (chromium && !s.network && !s.automationFetch) {
      s.network = true;
      await cdp(tab, 'Fetch.enable', {patterns: ['Request', 'Response'].flatMap(requestStage => ['Document', 'Other'].map(resourceType => ({urlPattern: '*', requestStage, resourceType})))});
    }
  } catch {
    s.channels.delete(channel);
    if (s.network) { s.network = false; void cdp(tab, 'Fetch.disable').catch(() => {}); }
    throw new ShimError('File transfer needs site access. Enable it again in the extension.');
  }
  return {maxBytes: FILE_LIMIT, chunkBytes: FILE_CHUNK};
}
function stopFileChannel(channel) {
  for (const [tab, s] of fileTabs) {
    if (!s.channels.delete(channel)) continue;
    for (const [id, u] of s.uploads) if (u.channel === channel) void cancelFile(tab, id);
    if (s.inputChannel === channel) { s.until = 0; s.inputChannel = null; }
    if (!s.channels.size) {
      void filePage(tab, null, {op: 'stop'}).catch(() => {});
      if (chromium && s.network) void cdp(tab, 'Fetch.disable').catch(() => {});
      s.network = false;
    }
  }
}
function stopFileTab(tab) {
  const s = fileTabs.get(tab);
  if (!s) return;
  for (const channel of s.channels) sendEvent(tab, 'Page.filesStopped', {}, channel);
  // Inject cleanup before removing the capability; a vanished page has its lease.
  void filePage(tab, null, {op: 'stop'}).catch(() => {});
  fileTabs.delete(tab); clearInterval(s.timer); clearTimeout(s.interceptTimer);
  if (chromium) {
    if (s.network) void cdp(tab, 'Fetch.disable').catch(() => {});
    void cdp(tab, 'Page.setInterceptFileChooserDialog', {enabled: false}).catch(() => {});
  }
  for (const [id, w] of fileWaiters) if (w.tab === tab) { clearTimeout(w.timer); w.reject(new ShimError('File sharing ended.')); fileWaiters.delete(id); }
}
async function armFileInput(tab, channel, method, params) {
  const s = fileTabs.get(tab);
  if (!s?.channels.has(channel)) return;
  if (!(method === 'Input.dispatchMouseEvent' && params.type === 'mousePressed' ||
        method === 'Input.dispatchTouchEvent' && params.type === 'touchStart' ||
        method === 'Input.dispatchKeyEvent' && ['keyDown', 'rawKeyDown'].includes(params.type))) return;
  s.until = Date.now() + 5000; s.inputChannel = channel;
  await filePage(tab, null, {op: 'arm'});
  if (chromium) {
    await cdp(tab, 'Page.setInterceptFileChooserDialog', {enabled: true});
    clearTimeout(s.interceptTimer);
    s.interceptTimer = setTimeout(() => void cdp(tab, 'Page.setInterceptFileChooserDialog', {enabled: false}).catch(() => {}), 750);
  }
}
function waitFileTarget(tab, channel, request, chooser = false) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { fileWaiters.delete(request); reject(new ShimError('The file target changed or cannot be reached.')); }, 5000);
    fileWaiters.set(request, {tab, channel, resolve, reject, timer, chooser});
  });
}
async function cancelFile(tab, id) {
  const s = fileTabs.get(tab), u = s?.uploads.get(id);
  if (!u) return;
  s.uploads.delete(id);
  await filePage(tab, u.frame, {op: 'cancel', id}).catch(() => {});
}
async function fileCommand(tab, channel, method, p) {
  if (method === 'Page.startFiles') return startFileChannel(tab, channel);
  const s = fileTabs.get(tab);
  if (!s?.channels.has(channel) || !sharing.files || !shared.get(tab)?.control) throw new ShimError('File transfer is not enabled for this view.');
  if (method === 'Page.stopFiles') { stopFileChannel(channel); return {}; }
  if (method === 'Page.fileTarget') {
    if (fileWaiters.size >= 64) throw new ShimError('Too many pending file targets.');
    if (![p.x, p.y].every(Number.isFinite)) throw new ShimError('Invalid drop position.');
    const request = crypto.randomUUID(), answer = waitFileTarget(tab, channel, request);
    void filePage(tab, 0, {op: 'locate', request, x: p.x, y: p.y}).catch(() => {});
    return answer;
  }
  if (method === 'Page.beginFile') {
    const target = s.targets.get(p.target);
    if (!target || target.channel !== channel || !Number.isSafeInteger(p.size) || p.size < 0 || p.size > FILE_LIMIT ||
        s.uploads.size >= 32 || fileBytes() + p.size > FILE_CACHE_LIMIT) throw new ShimError('Invalid file, expired target or file transfer limit reached (1 GiB per file).');
    const id = crypto.randomUUID();
    const u = {channel, target: p.target, frame: target.frame, size: p.size, received: 0, touched: Date.now(), serial: Promise.resolve()};
    s.uploads.set(id, u); target.touched = Date.now();
    try { await filePage(tab, u.frame, {op: 'begin', target: p.target, id, name: fileName(p.name), size: p.size,
      type: typeof p.type === 'string' ? p.type.slice(0, 128) : '', lastModified: Number.isFinite(p.lastModified) ? p.lastModified : Date.now()}); }
    catch (error) { s.uploads.delete(id); throw error; }
    return {id};
  }
  if (method === 'Page.fileChunk') {
    const u = s.uploads.get(p.id);
    if (!u || u.channel !== channel || p.offset !== u.received || typeof p.data !== 'string' ||
        p.data.length > 87384 || !/^[A-Za-z0-9+/]*={0,2}$/.test(p.data)) throw new ShimError('Invalid file chunk.');
    const bytes = atob(p.data).length;
    if (bytes > FILE_CHUNK || u.received + bytes > u.size) throw new ShimError('File exceeds its declared size.');
    u.received += bytes; u.touched = Date.now();
    u.serial = u.serial.then(() => filePage(tab, u.frame, {op: 'chunk', id: p.id, offset: p.offset, data: p.data}));
    await u.serial;
    return {received: u.received};
  }
  if (method === 'Page.cancelFile') {
    if (s.uploads.get(p.id)?.channel === channel) await cancelFile(tab, p.id);
    return {};
  }
  if (method === 'Page.applyFiles') {
    const target = s.targets.get(p.target);
    if (!target || target.channel !== channel || !Array.isArray(p.files) || !p.files.length || p.files.length > 32) throw new ShimError('Invalid file target.');
    for (const id of p.files) {
      const u = s.uploads.get(id);
      if (!u || u.channel !== channel || u.target !== p.target || u.received !== u.size) throw new ShimError('Upload is incomplete.');
      await u.serial;
    }
    await filePage(tab, target.frame, {op: 'apply', target: p.target, files: p.files});
    for (const id of p.files) s.uploads.delete(id);
    s.targets.delete(p.target);
    return {count: p.files.length};
  }
  if (method === 'Page.listDownloads') return {downloads: [...s.downloads.values()].map(publicFile)};
  const d = s.downloads.get(p.id);
  if (!d) throw new ShimError('Download expired or was removed.');
  d.touched = Date.now();
  if (method === 'Page.removeDownload') { s.downloads.delete(p.id); fileEvent(tab, 'Page.downloadProgress', {id: p.id, removed: true}); return {}; }
  if (method === 'Page.readDownload') {
    if (d.state !== 'completed' || !Number.isSafeInteger(p.offset) || p.offset < 0 || p.offset > d.received_bytes) throw new ShimError('Download is not ready.');
    const part = d.parts[Math.floor(p.offset / FILE_CHUNK)];
    const data = part?.subarray(p.offset % FILE_CHUNK) || new Uint8Array();
    return {data: fileEncode(data), eof: p.offset + data.length >= d.received_bytes};
  }
  throw new ShimError('Unknown file operation.');
}
function beginFileDownload(tab, id, name, url, size, frame = null, inputChannel = null) {
  const s = fileTabs.get(tab);
  if (!s?.channels.size || (!inputChannel && s.until < Date.now()) || !s.channels.has(inputChannel || s.inputChannel) || s.downloads.size >= 64 || size > FILE_LIMIT) return null;
  if (s.downloads.has(id)) return null;
  size = Number.isSafeInteger(size) && size >= 0 ? size : null;
  const d = {id, name: fileName(name), url: safeURL(url) ? url : '', total_bytes: size || null, received_bytes: 0,
    state: 'in_progress', started_at_secs: Math.floor(Date.now() / 1000), parts: [], touched: Date.now(), frame};
  s.downloads.set(id, d); fileEvent(tab, 'Page.downloadProgress', publicFile(d));
  return d;
}
function appendFileDownload(tab, id, bytes) {
  const s = fileTabs.get(tab), d = s?.downloads.get(id);
  if (!s?.channels.size || !sharing.files || !shared.get(tab)?.control || shared.get(tab)?.paused !== false || !d || d.state !== 'in_progress' || d.received_bytes + bytes.length > FILE_LIMIT || fileBytes() + bytes.length > FILE_CACHE_LIMIT) return false;
  // Pack every part to FILE_CHUNK, so offset reads never depend on network frames.
  let offset = 0;
  while (offset < bytes.length) {
    let last = d.parts.at(-1);
    const used = d.received_bytes % FILE_CHUNK;
    if (!last || !used) { last = new Uint8Array(FILE_CHUNK); d.parts.push(last); }
    const count = Math.min(FILE_CHUNK - used, bytes.length - offset);
    last.set(bytes.subarray(offset, offset + count), used); offset += count; d.received_bytes += count;
  }
  d.touched = Date.now();
  if (!d.told || Date.now() - d.told > 400) { fileEvent(tab, 'Page.downloadProgress', publicFile(d)); d.told = Date.now(); }
  return true;
}
function endFileDownload(tab, id, ok) {
  const d = fileTabs.get(tab)?.downloads.get(id);
  if (!d) return;
  d.state = ok ? 'completed' : 'canceled'; d.finished_at_secs = Math.floor(Date.now() / 1000);
  if (!ok) { d.parts = []; d.received_bytes = 0; }
  else if (d.parts.length && d.received_bytes % FILE_CHUNK) d.parts[d.parts.length - 1] = d.parts.at(-1).slice(0, d.received_bytes % FILE_CHUNK);
  fileEvent(tab, 'Page.downloadProgress', publicFile(d));
}
async function fileBridgeMessage(m, sender) {
  const tab = sender.tab?.id, s = fileTabs.get(tab);
  if (sender.id !== api.runtime.id || !s || m.cap !== s.cap || !sharing.files || !shared.get(tab)?.control ||
      shared.get(tab)?.paused !== false || !safeTab(sender.tab) || !safeURL(sender.url)) return {ok: false};
  if (m.event === 'chooser' || m.event === 'target') {
    if (typeof m.request !== 'string' || !/^[a-f0-9-]{36}$/.test(m.request) || s.targets.size >= 64) return {ok: false};
    const waiter = fileWaiters.get(m.request);
    const channel = waiter?.tab === tab ? waiter.channel : m.event === 'chooser' && Date.now() < s.until ? s.inputChannel : null;
    if (!channel || !s.channels.has(channel)) return {ok: false};
    s.targets.set(m.request, {frame: sender.frameId, channel, touched: Date.now()});
    if (waiter) { clearTimeout(waiter.timer); fileWaiters.delete(m.request); waiter.resolve({target: m.request}); }
    if (m.event === 'chooser') sendEvent(tab, 'Page.fileChooserOpened', {fileRequest: m.request, mode: m.multiple ? 'selectMultiple' : 'selectSingle', accept: String(m.accept || '').slice(0, 512)}, channel);
    return {ok: true};
  }
  if (typeof m.id !== 'string' || !/^[a-f0-9-]{36}$/.test(m.id)) return {ok: false};
  if (m.event === 'downloadBegin') return {ok: !!beginFileDownload(tab, m.id, m.name, m.url, m.size, sender.frameId)};
  const d = s.downloads.get(m.id);
  if (!d || d.frame !== sender.frameId) return {ok: false};
  if (m.event === 'downloadChunk') {
    if (typeof m.data !== 'string' || m.data.length > 87384 || !/^[A-Za-z0-9+/]*={0,2}$/.test(m.data)) return {ok: false};
    const ok = appendFileDownload(tab, m.id, Uint8Array.from(atob(m.data), c => c.charCodeAt(0)));
    if (!ok) endFileDownload(tab, m.id, false);
    return {ok};
  }
  if (m.event === 'downloadEnd' || m.event === 'downloadCancel') { endFileDownload(tab, m.id, m.event === 'downloadEnd'); return {ok: true}; }
  return {ok: false};
}
async function interceptFileChooser(tab, p) {
  const s = fileTabs.get(tab);
  if (!s || Date.now() > s.until || !s.inputChannel || !p.backendNodeId) return;
  const request = crypto.randomUUID();
  const input = await cdp(tab, 'DOM.resolveNode', {backendNodeId: p.backendNodeId});
  const objectId = input.object?.objectId;
  if (!objectId) return;
  try {
    await cdp(tab, 'Runtime.callFunctionOn', {objectId, functionDeclaration: 'function(id) { this.setAttribute("data-bd-file", id); }', arguments: [{value: request}]});
    await filePage(tab, null, {op: 'mark', request});
  } finally { await cdp(tab, 'Runtime.releaseObject', {objectId}).catch(() => {}); }
}
function attachmentName(headers) {
  const disposition = headers.find(h => h.name.toLowerCase() === 'content-disposition')?.value || '';
  if (!/^\s*attachment\b/i.test(disposition)) return null;
  const encoded = /filename\*=UTF-8''([^;]+)/i.exec(disposition)?.[1];
  if (encoded) { try { return decodeURIComponent(encoded); } catch { /* fall back */ } }
  return /filename="([^"]*)"/i.exec(disposition)?.[1] || /filename=([^;]+)/i.exec(disposition)?.[1]?.trim() || 'download';
}
async function interceptFileResponse(tab, p) {
  const s = fileTabs.get(tab);
  if (!s?.network || s.automationFetch) return false;
  const resume = () => cdp(tab, 'Fetch.continueRequest', {requestId: p.requestId}).catch(() => {});
  const key = p.networkId || p.requestId;
  if (!p.responseStatusCode && !p.responseErrorReason) {
    if (Date.now() < s.until && s.channels.has(s.inputChannel)) { if (s.requests.size >= 1024) s.requests.delete(s.requests.keys().next().value); s.requests.set(key, {channel: s.inputChannel, until: Date.now() + 5 * 60 * 1000}); }
    await resume(); return true;
  }
  const initiator = s.requests.get(key);
  const name = attachmentName(p.responseHeaders || []);
  if (!name || !safeURL(p.request?.url) || ['Fetch', 'XHR'].includes(p.resourceType) || !s.channels.size || !initiator || initiator.until < Date.now() || !s.channels.has(initiator.channel)) { await resume(); return true; }
  const size = Number(p.responseHeaders.find(h => h.name.toLowerCase() === 'content-length')?.value) || null;
  const id = crypto.randomUUID(), d = beginFileDownload(tab, id, name, p.request.url, size, null, initiator.channel);
  if (!d) { await resume(); return true; }
  let handle, ok = false;
  try {
    ({stream: handle} = await cdp(tab, 'Fetch.takeResponseBodyAsStream', {requestId: p.requestId}));
    while (fileTabs.get(tab) === s && s.channels.size) {
      const chunk = await cdp(tab, 'IO.read', {handle, size: FILE_CHUNK});
      const bytes = chunk.base64Encoded ? Uint8Array.from(atob(chunk.data), c => c.charCodeAt(0)) : new TextEncoder().encode(chunk.data);
      if (!appendFileDownload(tab, id, bytes)) throw new Error('Download limit');
      if (chunk.eof) { ok = true; break; }
    }
  } catch { /* A canceled or oversized download remains visible as canceled. */ }
  finally {
    if (handle) await cdp(tab, 'IO.close', {handle}).catch(() => {});
    await cdp(tab, 'Fetch.failRequest', {requestId: p.requestId, errorReason: 'Aborted'}).catch(() => {});
    endFileDownload(tab, id, ok);
  }
  return true;
}
let firefoxFileFilterInstalled = false;
function installFirefoxFileFilter() {
  if (firefoxFileFilterInstalled || chromium || !api.webRequest?.filterResponseData) return;
  api.webRequest.onBeforeRequest.addListener(details => {
    const s = fileTabs.get(details.tabId);
    if (s?.channels.size && Date.now() < s.until && s.channels.has(s.inputChannel) && ['main_frame', 'sub_frame', 'other'].includes(details.type)) {
      if (s.requests.size >= 1024) s.requests.delete(s.requests.keys().next().value);
      s.requests.set(details.requestId, {channel: s.inputChannel, until: Date.now() + 5 * 60 * 1000});
    }
  }, {urls: ['http://*/*', 'https://*/*']});
  api.webRequest.onHeadersReceived.addListener(details => {
    const s = fileTabs.get(details.tabId), initiator = s?.requests.get(details.requestId), name = attachmentName(details.responseHeaders || []);
    if (!s?.channels.size || !sharing.files || !shared.get(details.tabId)?.control || shared.get(details.tabId)?.paused !== false ||
        !name || !initiator || initiator.until < Date.now() || !s.channels.has(initiator.channel) || !safeURL(details.url) || !['main_frame', 'sub_frame', 'other'].includes(details.type)) return;
    const id = crypto.randomUUID();
    const size = Number(details.responseHeaders.find(h => h.name.toLowerCase() === 'content-length')?.value) || null;
    if (!beginFileDownload(details.tabId, id, name, details.url, size, null, initiator.channel)) return;
    let filter;
    try { filter = api.webRequest.filterResponseData(details.requestId); } catch { endFileDownload(details.tabId, id, false); return; }
    let ok = true;
    filter.ondata = event => { filter.write(event.data); if (ok && !(ok = appendFileDownload(details.tabId, id, new Uint8Array(event.data)))) endFileDownload(details.tabId, id, false); };
    filter.onstop = () => { filter.close(); endFileDownload(details.tabId, id, ok); };
    filter.onerror = () => endFileDownload(details.tabId, id, false);
  }, {urls: ['http://*/*', 'https://*/*']}, ['blocking', 'responseHeaders']);
  firefoxFileFilterInstalled = true;
}

async function resumeFileNetwork(tab) {
  const s = fileTabs.get(tab);
  if (!chromium || !s?.channels.size) return;
  s.automationFetch = false;
  s.network = true;
  try { await cdp(tab, 'Fetch.enable', {patterns: ['Request', 'Response'].flatMap(requestStage => ['Document', 'Other'].map(resourceType => ({urlPattern: '*', requestStage, resourceType})))}); }
  catch (error) { s.network = false; throw error; }
}
