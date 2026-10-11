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

async function downloadAndSave(id, url, headers, signal, expectedBytes = 0, maxRetries = 3) {
  let attempt = 0;
  let lastError = null;

  while (attempt < maxRetries) {
    if (signal.aborted) throw new DOMException('Aborted', 'AbortError');
    attempt++;

    try {
      const res = await fetch(url, { headers, signal });
      if (!res.ok) {
        throw new Error(`HTTP ${res.status}: ${res.statusText}`);
      }

      const headerContentLength = +(res.headers.get('Content-Length') || 0);
      const contentLength = headerContentLength > 0 ? headerContentLength : (expectedBytes || 0);
      let text = '';
      let receivedBytes = 0;
      let lastProgressPost = 0;

      if (res.body && typeof res.body.getReader === 'function') {
        const reader = res.body.getReader();
        const decoder = new TextDecoder('utf-8');

        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          receivedBytes += value.length;
          text += decoder.decode(value, { stream: true });

          const now = Date.now();
          if (now - lastProgressPost > 100 || receivedBytes === contentLength) {
            lastProgressPost = now;
            const pct = contentLength > 0 ? Math.min(99, Math.round((receivedBytes / contentLength) * 100)) : 0;
            self.postMessage({
              type: 'PROGRESS',
              id,
              receivedBytes,
              totalBytes: contentLength,
              pct
            });
          }
        }
        text += decoder.decode();
      } else {
        text = await res.text();
        receivedBytes = text.length;
      }

      self.postMessage({
        type: 'PROGRESS',
        id,
        receivedBytes,
        totalBytes: contentLength || receivedBytes,
        pct: 99,
        parsing: true
      });

      // Parse JSON lines in worker thread (off the main UI thread)
      const lines = text.trim().split('\n');
      const frames = [];
      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          frames.push(JSON.parse(line));
        } catch (_) {}
      }

      if (frames.length === 0) {
        throw new Error('Recording contains no frames');
      }

      // Save directly to IndexedDB
      await saveFramesToDb(id, frames);

      self.postMessage({
        type: 'SUCCESS',
        id,
        frameCount: frames.length,
        sizeBytes: receivedBytes
      });

      return;
    } catch (err) {
      lastError = err;
      if (err.name === 'AbortError') throw err;
      // If network was interrupted or backgrounded, retry with backoff
      if (attempt < maxRetries) {
        self.postMessage({
          type: 'RETRY',
          id,
          attempt,
          maxRetries
        });
        await new Promise((r) => setTimeout(r, 1500));
      }
    }
  }

  throw lastError || new Error('Download failed after retries');
}
