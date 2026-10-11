// Browser Daemon Service Worker for PWA App Shell
const CACHE_NAME = 'browser-daemon-shell-v1';
const PRECACHE_ASSETS = [
  '/',
  '/manifest.webmanifest',
  '/favicon.svg',
  '/favicon.ico',
  '/icon-192.png',
  '/icon-512.png'
];

self.addEventListener('install', (event) => {
  self.skipWaiting();
  event.waitUntil(
    caches.open(CACHE_NAME).then((cache) => {
      return cache.addAll(PRECACHE_ASSETS).catch((err) => {
        console.warn('[SW] Precache asset caching failed:', err);
      });
    })
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys().then((keys) => {
      return Promise.all(
        keys.filter((k) => k !== CACHE_NAME).map((k) => caches.delete(k))
      );
    }).then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;

  const url = new URL(req.url);

  // Bypass WebSockets, CDP endpoints, API endpoints, screencast streams, and live media
  if (
    url.protocol === 'ws:' ||
    url.protocol === 'wss:' ||
    url.pathname.startsWith('/api/') ||
    url.pathname.startsWith('/json') ||
    url.pathname.startsWith('/devtools') ||
    url.pathname.startsWith('/chromium') ||
    url.pathname.startsWith('/ws') ||
    url.pathname.includes('/frames') ||
    url.pathname.includes('/stream')
  ) {
    return;
  }

  // Network-first with cache fallback for dashboard HTML and shell assets
  event.respondWith(
    fetch(req)
      .then((networkResponse) => {
        if (networkResponse && networkResponse.status === 200) {
          const resClone = networkResponse.clone();
          caches.open(CACHE_NAME).then((cache) => cache.put(req, resClone));
        }
        return networkResponse;
      })
      .catch(() => {
        return caches.match(req).then((cachedResponse) => {
          if (cachedResponse) return cachedResponse;
          if (req.headers.get('accept')?.includes('text/html')) {
            return caches.match('/');
          }
          return new Response('Offline', { status: 503, statusText: 'Offline' });
        });
      })
  );
});
