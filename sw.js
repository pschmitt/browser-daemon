// Browser Daemon service worker: keeps the static assets (icons, manifest, shared code)
// for the installed app. Pages and API answers are never cached or touched:
// they sit behind the sign-in cookie and carry live data.
const CACHE_NAME = 'browser-daemon-static-v7';
const STATIC_PATHS = new Set([
  '/manifest.webmanifest',
  '/favicon.svg',
  '/favicon.ico',
  '/icon-192.png',
  '/icon-512.png',
  '/icon-maskable-512.png',
  '/assets/common.js',
  '/assets/common.css'
]);

self.addEventListener('install', (event) => {
  self.skipWaiting();
  event.waitUntil(
    caches.open(CACHE_NAME)
      .then((cache) => cache.addAll([...STATIC_PATHS]))
      .catch((err) => console.warn('[SW] Precache failed:', err))
  );
});

// Every cache of an older version goes (v3 and before held the dashboard HTML).
self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE_NAME).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  // Only our own static assets; everything else goes to the network untouched.
  if (url.origin !== self.location.origin || !STATIC_PATHS.has(url.pathname)) return;

  // The query string is ignored, in the cache key as well as in the lookup.
  const key = new Request(url.origin + url.pathname);
  event.respondWith(
    fetch(req)
      .then((res) => {
        if (res && res.status === 200 && res.type === 'basic') {
          const copy = res.clone();
          caches.open(CACHE_NAME).then((cache) => cache.put(key, copy));
        }
        return res;
      })
      .catch(() => caches.match(key).then((hit) => hit || new Response('Offline', { status: 503, statusText: 'Offline' })))
  );
});
