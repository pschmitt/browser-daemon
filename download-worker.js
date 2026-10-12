// Browser Daemon Recording Background Downloader Worker
const REC_DB_NAME = 'BrowserDaemonRecordings';
const REC_DB_VERSION = 1;
const REC_STORE_NAME = 'recordings';

function openDb() {
  return new Promise((resolve, reject) => {
    if (!self.indexedDB) {
      reject(new Error('IndexedDB not supported in worker'));
      return;
    }
    const req = self.indexedDB.open(REC_DB_NAME, REC_DB_VERSION);
    req.onupgradeneeded = (e) => {
      const db = e.target.result;
      if (!db.objectStoreNames.contains(REC_STORE_NAME)) {
        db.createObjectStore(REC_STORE_NAME, { keyPath: 'id' });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function saveFramesToDb(id, frames) {
  return openDb().then((db) => {
    return new Promise((resolve, reject) => {
      const tx = db.transaction(REC_STORE_NAME, 'readwrite');
      const store = tx.objectStore(REC_STORE_NAME);
      store.put({ id, frames, cachedAt: Date.now() });
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
  });
}

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
    if (activeTasks.has(id)) {
      // Already downloading in background
      return;
    }

    const controller = new AbortController();
    activeTasks.set(id, { controller, startTime: Date.now() });

    try {
      await downloadAndSave(id, url, headers, controller.signal, expectedBytes || 0);
      activeTasks.delete(id);
    } catch (err) {
      activeTasks.delete(id);
      if (err.name !== 'AbortError') {
        self.postMessage({ type: 'ERROR', id, error: String(err.message || err) });
      }
    }
  }
});

// Streams frames.jsonl and parses it line by line: a single string holding a
// multi-hundred-MB recording exceeds the engine's string limit. Interrupted
// transfers resume where they stopped with a Range request.
async function downloadAndSave(id, url, headers, signal, expectedBytes = 0, maxRetries = 6) {
  const frames = [];
  let receivedBytes = 0;
  let totalBytes = expectedBytes || 0;
  let carry = '';
  let attempt = 0;
  let lastProgressPost = 0;

  const takeLines = (chunk, final) => {
    carry += chunk;
    const parts = carry.split('\n');
    carry = final ? '' : parts.pop();
    for (const line of parts) {
      if (!line.trim()) continue;
      try { frames.push(JSON.parse(line)); } catch (_) {}
    }
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
        // The server ignored the range: start over.
        frames.length = 0;
        receivedBytes = 0;
        carry = '';
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
          self.postMessage({ type: 'PROGRESS', id, receivedBytes, totalBytes, pct, frameCount: frames.length });
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

  if (frames.length === 0) throw new Error('Recording contains no frames');

  let cached = true;
  try {
    await saveFramesToDb(id, frames);
  } catch (err) {
    cached = false;
    console.warn('Could not cache recording', id, err);
  }
  self.postMessage({ type: 'SUCCESS', id, frameCount: frames.length, sizeBytes: receivedBytes, cached, frames });
}
