/* Fetch and validate shared tab favicons. */
const faviconCache = new Map();
const MAX_FAVICON_BYTES = 128 * 1024;

function faviconURL(value) {
  if (typeof value !== 'string') return '';
  if (value.length <= 176 * 1024 && /^data:image\/(?:png|jpeg|gif|x-icon|webp);base64,[A-Za-z0-9+/]+={0,2}$/.test(value)) return value;
  const normalized = value.startsWith('moz-anno:favicon:') ? value.slice('moz-anno:favicon:'.length) : value;
  return /^https?:\/\//i.test(normalized) ? normalized.slice(0, 2048) : '';
}

function faviconDataURL(bytes) {
  const starts = (...values) => values.every((value, index) => bytes[index] === value);
  let mime = '';
  if (starts(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)) mime = 'image/png';
  else if (starts(0xff, 0xd8, 0xff)) mime = 'image/jpeg';
  else if (starts(0x47, 0x49, 0x46, 0x38)) mime = 'image/gif';
  else if (starts(0x00, 0x00, 0x01, 0x00)) mime = 'image/x-icon';
  else if (bytes.length >= 12 && String.fromCharCode(...bytes.subarray(0, 4)) === 'RIFF' &&
      String.fromCharCode(...bytes.subarray(8, 12)) === 'WEBP') mime = 'image/webp';
  if (!mime) return '';
  let binary = '';
  for (let i = 0; i < bytes.length; i += 8192) binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
  return `data:${mime};base64,${btoa(binary)}`;
}

async function tabFavicon(value) {
  const url = faviconURL(value);
  if (!url) return '';
  if (url.startsWith('data:')) return url;
  if (faviconCache.has(url)) return faviconCache.get(url);
  const pending = (async () => {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 4000);
    try {
      const response = await fetch(url, {credentials: 'omit', cache: 'force-cache', redirect: 'follow', signal: controller.signal});
      if (!response.ok || !response.body) return url;
      const length = Number(response.headers.get('content-length'));
      if (Number.isFinite(length) && length > MAX_FAVICON_BYTES) return url;
      const reader = response.body.getReader();
      const chunks = [];
      let size = 0;
      while (true) {
        const {done, value: chunk} = await reader.read();
        if (done) break;
        size += chunk.length;
        if (size > MAX_FAVICON_BYTES) { await reader.cancel(); return url; }
        chunks.push(chunk);
      }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
      return faviconDataURL(bytes) || url;
    } catch { return url; }
    finally { clearTimeout(timeout); }
  })();
  faviconCache.set(url, pending);
  if (faviconCache.size > 512) faviconCache.delete(faviconCache.keys().next().value);
  const result = await pending;
  faviconCache.set(url, result);
  return result;
}

