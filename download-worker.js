// Browser Daemon recording downloader.
//
// Streams frames.jsonl, hands the page the frames' index (time, address, title
// and which chunk holds the picture, never the picture itself) so playback can
// start right away, and caches the full frames in IndexedDB chunk by chunk. The
// page reads pictures on demand: from the cached chunk, or with a Range request
// for the chunk's bytes. A recording only counts as cached once its
// "recordings" entry (written last, with the index) exists.
const REC_DB_NAME = 'BrowserDaemonRecordings';
const REC_DB_VERSION = 2;
const REC_STORE_NAME = 'recordings';
const CHUNK_STORE_NAME = 'chunks';
const BATCH_FRAMES = 40;
const BATCH_MS = 250;

let dbPromise = null;
function openDb() {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise((resolve, reject) => {
    if (!self.indexedDB) {
      reject(new Error('IndexedDB not supported in worker'));
      return;
    }
    const req = self.indexedDB.open(REC_DB_NAME, REC_DB_VERSION);
    req.onupgradeneeded = (e) => {
      const db = e.target.result;
      if (!db.objectStoreNames.contains(REC_STORE_NAME)) db.createObjectStore(REC_STORE_NAME, { keyPath: 'id' });
      if (!db.objectStoreNames.contains(CHUNK_STORE_NAME)) db.createObjectStore(CHUNK_STORE_NAME, { keyPath: ['id', 'seq'] });
    };
    req.onsuccess = () => {
      const db = req.result;
      db.onversionchange = () => { db.close(); dbPromise = null; };
      resolve(db);
    };
    req.onerror = () => { dbPromise = null; reject(req.error); };
  });
  return dbPromise;
}

function tx(storeNames, mode, fn) {
  return openDb().then((db) => new Promise((resolve, reject) => {
    const t = db.transaction(storeNames, mode);
    fn(t);
    t.oncomplete = () => resolve();
    t.onerror = () => reject(t.error);
    t.onabort = () => reject(t.error || new Error('Transaction aborted'));
  }));
}

const putChunk = (id, seq, frames) => tx(CHUNK_STORE_NAME, 'readwrite', (t) => t.objectStore(CHUNK_STORE_NAME).put({ id, seq, frames }));
// Spans the chunk store too, so it is ordered after the pending chunk writes.
const putMeta = (id, chunks, frameCount, index, chunkInfo) => tx([CHUNK_STORE_NAME, REC_STORE_NAME], 'readwrite', (t) => t.objectStore(REC_STORE_NAME).put({ id, chunks, frameCount, index, chunkInfo, cachedAt: Date.now() }));
const dropChunks = (id) => tx(CHUNK_STORE_NAME, 'readwrite', (t) => t.objectStore(CHUNK_STORE_NAME).delete(IDBKeyRange.bound([id, 0], [id, Infinity])));

const activeTasks = new Map();

self.addEventListener('message', async (e) => {
  const { action, id, url, headers, expectedBytes } = e.data || {};

  if (action === 'CANCEL') {
    const task = activeTasks.get(id);
    if (task && task.controller) {
      task.controller.abort();
      activeTasks.delete(id);
    }
    return;
  }

  if (action === 'DOWNLOAD') {
    if (activeTasks.has(id)) return;
    const controller = new AbortController();
    activeTasks.set(id, { controller, startTime: Date.now() });
    try {
      await download(id, url, headers, controller.signal, expectedBytes || 0);
    } catch (err) {
      if (err.name !== 'AbortError') self.postMessage({ type: 'ERROR', id, error: String(err.message || err) });
      dropChunks(id).catch(() => {});
    } finally {
      activeTasks.delete(id);
    }
  }
});

// Parses frames.jsonl line by line (one string cannot hold a recording of
// several hundred MB) and resumes interrupted transfers with a Range request.
async function download(id, url, headers, signal, expectedBytes = 0, maxRetries = 6) {
  let batch = [];
  let lastFlush = Date.now();
  let frameCount = 0;
  let seq = 0;
  let cacheOk = true;
  let receivedBytes = 0;
  let totalBytes = expectedBytes || 0;
  let carry = '';
  // Bytes of frames.jsonl before the line being parsed: where its chunk starts.
  let pos = 0;
  const encoder = new TextEncoder();
  const byteLength = (str) => (/^[\x00-\x7f]*$/.test(str) ? str.length : encoder.encode(str).length);
  // The page's light copy of the frames (no pictures), and where each chunk lies.
  const index = [];
  const chunkInfo = [];
  let chunkStart = -1;
  let chunkEnd = 0;
  let attempt = 0;
  let lastProgressPost = 0;

  await dropChunks(id).catch(() => { cacheOk = false; });

  const flush = (force) => {
    if (!batch.length || (!force && batch.length < BATCH_FRAMES && Date.now() - lastFlush < BATCH_MS)) return;
    const frames = batch;
    batch = [];
    lastFlush = Date.now();
    const chunk = { first: frameCount - frames.length, count: frames.length, off: chunkStart, end: chunkEnd };
    const light = frames.map((f) => ({ t: f.t, url: f.url, title: f.title, c: seq }));
    chunkStart = -1;
    chunkInfo.push(chunk);
    for (const f of light) index.push(f);
    self.postMessage({ type: 'FRAMES', id, frames: light, chunk, offset: chunk.first });
    const thisSeq = seq++;
    if (cacheOk) {
      putChunk(id, thisSeq, frames).catch((err) => {
        cacheOk = false;
        console.warn('Could not cache recording', id, err);
      });
    }
  };

  const takeLines = (chunk, final) => {
    carry += chunk;
    const parts = carry.split('\n');
    carry = final ? '' : parts.pop();
    for (const line of parts) {
      const start = pos;
      const len = byteLength(line);
      pos += len + 1;   // and the newline
      if (!line.trim()) continue;
      try {
        batch.push(JSON.parse(line));
        frameCount++;
        if (chunkStart < 0) chunkStart = start;
        chunkEnd = start + len;
      } catch (_) {}
    }
    flush(final);
  };

  while (true) {
    if (signal.aborted) throw new DOMException('Aborted', 'AbortError');
    const decoder = new TextDecoder('utf-8');
    try {
      const reqHeaders = { ...(headers || {}) };
      if (receivedBytes > 0) reqHeaders.Range = `bytes=${receivedBytes}-`;
      const res = await fetch(url, { headers: reqHeaders, signal, cache: 'no-store' });
      if (res.status === 416 && receivedBytes > 0) break;
      if (!res.ok) {
        const err = new Error(`HTTP ${res.status}${res.statusText ? ': ' + res.statusText : ''}`);
        err.fatal = res.status >= 400 && res.status < 500;
        throw err;
      }
      if (receivedBytes > 0 && res.status !== 206) {
        // The server ignored the range: frames already sent are sent again.
        throw Object.assign(new Error('Server cannot resume this download'), { fatal: true });
      }
      const len = +(res.headers.get('Content-Length') || 0);
      if (len > 0) totalBytes = receivedBytes + len;

      const reader = res.body.getReader();
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        receivedBytes += value.length;
        takeLines(decoder.decode(value, { stream: true }), false);
        attempt = 0;
        const now = Date.now();
        if (now - lastProgressPost > 150) {
          lastProgressPost = now;
          const pct = totalBytes > 0 ? Math.min(99, Math.round((receivedBytes / totalBytes) * 100)) : 0;
          self.postMessage({ type: 'PROGRESS', id, receivedBytes, totalBytes, pct, frameCount });
        }
      }
      if (totalBytes > 0 && receivedBytes < totalBytes) throw new Error('Connection closed early');
      takeLines(decoder.decode(), false);
      break;
    } catch (err) {
      if (err.name === 'AbortError' || err.fatal) throw err;
      attempt++;
      if (attempt > maxRetries) throw err;
      self.postMessage({ type: 'RETRY', id, attempt, maxRetries, receivedBytes, totalBytes });
      await new Promise((r) => setTimeout(r, Math.min(15000, 1000 * 2 ** (attempt - 1))));
    }
  }
  takeLines('', true);

  if (frameCount === 0) throw new Error('Recording contains no frames');

  let cached = cacheOk;
  if (cached) {
    try {
      await putMeta(id, seq, frameCount, index, chunkInfo);
    } catch (err) {
      cached = false;
    }
  }
  if (!cached) dropChunks(id).catch(() => {});
  self.postMessage({ type: 'SUCCESS', id, frameCount, sizeBytes: receivedBytes, cached });
}
