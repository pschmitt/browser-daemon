// Shared by the dashboard and the handoff view. Each page loads it (blocking) in its
// <head>, before its own script, so what is declared here is global there.
"use strict";

// Saved theme choice (light/dark; none = follow the system), applied before first paint.
(() => {
  let t = null;
  try { t = localStorage.getItem("dash.theme"); } catch { /* storage unavailable */ }
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
})();

// Signed in by the cookie the page came with (HttpOnly: no script ever sees a
// secret). A 401 means it expired: reload once to get a fresh one.
(() => {
  const plain = window.fetch.bind(window);
  window.fetch = async (...args) => {
    const res = await plain(...args);
    if (res.status === 401) {
      let again = false;
      try { again = sessionStorage.getItem("bd-401") === "1"; sessionStorage.setItem("bd-401", "1"); } catch { /* storage unavailable */ }
      if (!again) location.reload();
    } else {
      try { sessionStorage.removeItem("bd-401"); } catch { /* storage unavailable */ }
    }
    return res;
  };
})();
// The cookie authenticates same-origin requests; nothing to add.
function authHeaders(extra = {}) {
  return { ...extra };
}

const $ = (id) => document.getElementById(id);

// Firefox for Android, in an installed web app, cannot take a downloaded PDF: it hands
// it to its PDF viewer, which has no tab in a web app, and the app ends on about:blank.
// There a PDF goes to the share sheet (Files, Drive, a PDF app) instead, or, where files
// cannot be shared (Firefox for Android so far), is shown in this window by Firefox's
// PDF viewer: its menu shares it or opens it in Firefox, Back returns. Sharing needs a
// tap: the file is fetched ahead.
const FIREFOX_ANDROID = /Android/.test(navigator.userAgent) && /Firefox\//.test(navigator.userAgent);
const inAppWindow = () => matchMedia("(display-mode: standalone), (display-mode: fullscreen), (display-mode: minimal-ui)").matches;
function saveNeedsHelp(name) {
  return FIREFOX_ANDROID && inAppWindow() && /\.pdf$/i.test(String(name || ""));
}
const prefetchedFiles = new Map();
function prefetchFile(url, name) {
  // Only worth it where the share sheet takes files.
  if (!navigator.canShare?.({ files: [new File([""], name, { type: "application/pdf" })] })) return Promise.reject(new Error("no file sharing"));
  const key = new URL(url, location.href).href;
  if (!prefetchedFiles.has(key)) {
    const file = fetch(key, { headers: authHeaders() })
      .then((res) => (res.ok ? res.blob() : Promise.reject(new Error(res.statusText))))
      .then((blob) => new File([blob], name, { type: /\.pdf$/i.test(name) ? "application/pdf" : blob.type }));
    file.catch(() => prefetchedFiles.delete(key));
    prefetchedFiles.set(key, file);
  }
  return prefetchedFiles.get(key);
}
async function saveWithHelp(url, name) {
  const probe = new File([""], name, { type: "application/pdf" });
  if (navigator.canShare?.({ files: [probe] })) {
    const file = await prefetchFile(url, name).catch(() => null);
    try {
      if (file) {
        await navigator.share({ files: [file], title: name });
        return;
      }
    } catch (e) {
      if (e.name === "AbortError") return;
    }
  }
  const u = new URL(url, location.href);
  u.searchParams.set("inline", "1");
  location.href = u.href;
}
document.addEventListener("click", (e) => {
  const a = e.target.closest?.("a[download]");
  if (!a || !saveNeedsHelp(a.getAttribute("download"))) return;
  e.preventDefault();
  saveWithHelp(a.href, a.getAttribute("download"));
}, true);

function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
const esc = escapeHtml;

const SEARCH_ENGINES = {
  duckduckgo: { name: "DuckDuckGo", url: "https://duckduckgo.com/?q=" },
  google: { name: "Google", url: "https://www.google.com/search?q=" },
  startpage: { name: "Startpage", url: "https://www.startpage.com/do/search?q=" },
  brave: { name: "Brave Search", url: "https://search.brave.com/search?q=" },
  bing: { name: "Bing", url: "https://www.bing.com/search?q=" },
  kagi: { name: "Kagi", url: "https://kagi.com/search?q=" },
};
// The search engine of the URL bar: one choice for both pages (same origin, same key).
const SEARCH_KEY = "browserd.search";
function searchEngine() {
  let id = null;
  try { id = localStorage.getItem(SEARCH_KEY); } catch { /* storage unavailable */ }
  return { id: SEARCH_ENGINES[id] ? id : "duckduckgo", ...SEARCH_ENGINES[SEARCH_ENGINES[id] ? id : "duckduckgo"] };
}
function setSearchEngine(id) {
  try { localStorage.setItem(SEARCH_KEY, id); } catch { /* storage unavailable */ }
}

function formatUptime(secs) {
  if (!secs || secs < 0) return '0s';
  const d = Math.floor(secs / 86400), h = Math.floor((secs % 86400) / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${s}s`;
  return `${s}s`;
}

function hashOf(s) { let h = 0; for (const c of String(s)) h = (h * 31 + c.charCodeAt(0)) | 0; return Math.abs(h); }
// Names that differ in one character (rofl-13, rofl-14) land far apart on the colour wheel.
const hueOf = (name) => Math.round((hashOf(String(name || '').toLowerCase()) * 137.508) % 360);

// Site icons come from the node (/api/favicon): neither the sites nor an icon service
// ever see this browser. Local and single-label hosts are not on the public web:
// nothing to look up ('').
function faviconSrc(url) {
  let u;
  try { u = new URL(url, location.href); } catch { return ""; }
  const host = /^https?:$/.test(u.protocol) ? u.hostname : "";
  const lookable = host && host.includes(".") && !/^[\d.]+$/.test(host) && !host.includes(":");
  return lookable ? `/api/favicon?url=${encodeURIComponent(u.origin)}` : "";
}

// Which agent drives a session, by the name its browser MCP reports ("claude-code 2.1.296",
// "codex", ...): the agent's own mark instead of a generic robot.
const AGENT_SVG = {
  claude: '<path d="M12 3v18M3 12h18M5.64 5.64l12.72 12.72M18.36 5.64 5.64 18.36" fill="none" stroke="#d97757" stroke-width="2.4" stroke-linecap="round"/>',
  codex: '<g fill="none" stroke="currentColor" stroke-width="1.5"><rect x="7.2" y="2.8" width="9.6" height="18.4" rx="4.8"/><rect x="7.2" y="2.8" width="9.6" height="18.4" rx="4.8" transform="rotate(60 12 12)"/><rect x="7.2" y="2.8" width="9.6" height="18.4" rx="4.8" transform="rotate(120 12 12)"/></g>',
  gemini: '<defs><linearGradient id="ag-gem" x1="3" y1="21" x2="21" y2="3" gradientUnits="userSpaceOnUse"><stop stop-color="#4f8cf7"/><stop offset=".55" stop-color="#9b72f2"/><stop offset="1" stop-color="#f48fb1"/></linearGradient></defs><path d="M12 2c.7 5.6 4.4 9.3 10 10-5.6.7-9.3 4.4-10 10-.7-5.6-4.4-9.3-10-10 5.6-.7 9.3-4.4 10-10z" fill="url(#ag-gem)" stroke="none"/>',
  // The real Antigravity mark (Wikipedia's Google_Antigravity_Logo.svg, as in the
  // google-antigravity-usage HA integration), fitted from its 85x78 box.
  antigravity: '<g stroke="none" transform="translate(-2.365 -2.753) scale(0.2588)"><path d="M89.6992 93.695C94.3659 97.195 101.366 94.8617 94.9492 88.445C75.6992 69.7783 79.7825 18.445 55.8659 18.445C31.9492 18.445 36.0325 69.7783 16.7825 88.445C9.78251 95.445 17.3658 97.195 22.0325 93.695C40.1159 81.445 38.9492 59.8617 55.8659 59.8617C72.7825 59.8617 71.6159 81.445 89.6992 93.695Z" fill="#3186FF"/> <mask id="agy-m" maskUnits="userSpaceOnUse" x="13" y="18" width="85" height="78" style="mask-type: alpha;"><path d="M89.6992 93.695C94.3659 97.195 101.366 94.8617 94.9492 88.445C75.6992 69.7783 79.7825 18.445 55.8659 18.445C31.9492 18.445 36.0325 69.7783 16.7825 88.445C9.78251 95.445 17.3658 97.195 22.0325 93.695C40.1159 81.445 38.9492 59.8617 55.8659 59.8617C72.7825 59.8617 71.6159 81.445 89.6992 93.695Z" fill="black"/></mask> <g mask="url(#agy-m)"><g filter="url(#agy-f0)"><ellipse cx="22.7873" cy="26.8098" rx="22.7873" ry="26.8098" transform="matrix(-0.112784 0.99362 -0.99362 -0.112781 66.2473 -15.5344)" fill="#FFE432"/></g><g filter="url(#agy-f1)"><ellipse cx="96.491" cy="35.1231" rx="29.5007" ry="30.1492" transform="rotate(76.9243 96.491 35.1231)" fill="#FC413D"/></g><g filter="url(#agy-f2)"><ellipse cx="9.02988" cy="41.6647" rx="30.832" ry="39.9417" transform="rotate(74.1257 9.02988 41.6647)" fill="#00B95C"/></g><g filter="url(#agy-f3)"><ellipse cx="9.02988" cy="41.6647" rx="30.832" ry="39.9417" transform="rotate(74.1257 9.02988 41.6647)" fill="#00B95C"/></g><g filter="url(#agy-f4)"><ellipse cx="11.2212" cy="42.8915" rx="30.22" ry="33.2695" transform="rotate(45.6065 11.2212 42.8915)" fill="#00B95C"/></g><g filter="url(#agy-f5)"><ellipse cx="75.7546" cy="104.822" rx="29.0177" ry="27.943" transform="rotate(76.9243 75.7546 104.822)" fill="#3186FF"/></g><g filter="url(#agy-f6)"><ellipse cx="33.5661" cy="35.4043" rx="33.5661" ry="35.4043" transform="matrix(-0.409539 0.912293 -0.912294 -0.409537 101.25 -15.1674)" fill="#FBBC04"/></g><g filter="url(#agy-f7)"><path d="M2.56802 149.695C-15.8116 142.48 15.5987 83.1163 23.4093 63.2203C31.22 43.3244 52.4514 33.0447 70.831 40.26C89.2107 47.4753 110.996 87.2162 103.185 107.112C95.3742 127.008 20.9477 156.91 2.56802 149.695Z" fill="#3186FF"/></g><g filter="url(#agy-f8)"><path d="M113.934 75.8079C109.013 81.5509 96.1724 78.6224 85.253 69.2667C74.3335 59.911 69.4704 47.6711 74.391 41.928C79.3116 36.185 92.1525 39.1136 103.072 48.4692C113.991 57.8249 118.855 70.0648 113.934 75.8079Z" fill="#749BFF"/></g><g filter="url(#agy-f9)"><ellipse cx="92.611" cy="23.7962" rx="44.2411" ry="27.5016" transform="rotate(34.0763 92.611 23.7962)" fill="#FC413D"/></g><g filter="url(#agy-f10)"><ellipse cx="23.4949" cy="29.5887" rx="23.7071" ry="13.7869" transform="rotate(112.516 23.4949 29.5887)" fill="#FFEE48"/></g></g> <defs><filter id="agy-f0" x="2.49348" y="-26.5423" width="69.0899" height="61.2525" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="3.89034" result="b"/></filter><filter id="agy-f1" x="28.7524" y="-32.0333" width="135.477" height="134.313" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="18.8078" result="b"/></filter><filter id="agy-f2" x="-62.2884" y="-21.9253" width="142.637" height="127.18" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.9884" result="b"/></filter><filter id="agy-f3" x="-62.2884" y="-21.9253" width="142.637" height="127.18" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.9884" result="b"/></filter><filter id="agy-f4" x="-52.5697" y="-20.8346" width="127.582" height="127.452" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.9884" result="b"/></filter><filter id="agy-f5" x="17.3619" y="45.4646" width="116.786" height="118.715" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.1937" result="b"/></filter><filter id="agy-f6" x="-7.44765" y="-60.4737" width="125.303" height="122.858" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="13.7698" result="b"/></filter><filter id="agy-f7" x="-27.7086" y="13.3597" width="157.119" height="162.029" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="12.297" result="b"/></filter><filter id="agy-f8" x="50.4638" y="16.981" width="87.3973" height="83.7738" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="11.0036" result="b"/></filter><filter id="agy-f9" x="34.2604" y="-28.457" width="116.701" height="104.506" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="9.29385" result="b"/></filter><filter id="agy-f10" x="-15.1522" y="-15.9493" width="77.2941" height="91.076" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="11.5027" result="b"/></filter></defs></g>',
  opencode: '<rect x="4" y="3.5" width="16" height="17" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.8"/><rect x="8.5" y="8" width="7" height="8" rx="1" fill="currentColor" stroke="none"/>',
  cursor: '<path d="M5 3l15 8.2-6.3 1.8L10.5 20z" fill="currentColor" stroke="none"/>',
  copilot: '<g fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="7" width="18" height="12" rx="6"/><path d="M8 7c0-2.5 2-4 4-4s4 1.5 4 4"/></g><circle cx="9" cy="13" r="1.4" fill="currentColor" stroke="none"/><circle cx="15" cy="13" r="1.4" fill="currentColor" stroke="none"/>',
  hermes: '<circle cx="12" cy="12" r="9" fill="none" stroke="#c8a14a" stroke-width="1.8"/><path d="M8.5 7.5v9M15.5 7.5v9M8.5 12h7" fill="none" stroke="#c8a14a" stroke-width="1.9" stroke-linecap="round"/>',
  agent: '<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="8" width="16" height="12" rx="3"/><path d="M12 4v4M9.5 13v1M14.5 13v1M9.5 17h5M2 13v3M22 13v3"/></g>',
};
const AGENT_TITLE = { claude: 'Claude', codex: 'Codex', gemini: 'Gemini', antigravity: 'Antigravity', opencode: 'OpenCode', cursor: 'Cursor', copilot: 'Copilot', hermes: 'Hermes', agent: 'Agent' };
// The agent as shown: without its version, and "claude" rather than "claude-code".
const agentShort = (c) => String(c?.agent_name || "").replace(/ \d[\w.-]*/, "").replace(/^claude-code/, "claude");
function agentBrand(c) {
  const n = String(c?.agent_name || '').toLowerCase();
  if (/claude/.test(n)) return 'claude';
  if (/codex|openai|chatgpt/.test(n)) return 'codex';
  if (/gemini/.test(n)) return 'gemini';
  if (/\bagy\b|antigravity/.test(n)) return 'antigravity';
  if (/opencode/.test(n)) return 'opencode';
  if (/cursor/.test(n)) return 'cursor';
  if (/copilot/.test(n)) return 'copilot';
  if (/hermes/.test(n)) return 'hermes';
  return 'agent';
}
