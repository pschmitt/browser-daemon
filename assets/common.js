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
  if (u.protocol !== "blob:") u.searchParams.set("inline", "1");
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
function faviconSrc(url, iconUrl = '') {
  let u;
  try { u = new URL(url, location.href); } catch { return ""; }
  if (iconUrl) {
    if (iconUrl.length <= 176 * 1024 && /^data:image\/(?:png|jpeg|gif|x-icon|webp);base64,[A-Za-z0-9+/]+={0,2}$/.test(iconUrl)) return iconUrl;
    try {
      const icon = new URL(iconUrl);
      if (/^https?:$/.test(icon.protocol)) {
        return `/api/favicon?url=${encodeURIComponent(u.origin)}&icon=${encodeURIComponent(icon.href)}`;
      }
    } catch { /* use the node's site-icon lookup below */ }
  }
  const host = /^https?:$/.test(u.protocol) ? u.hostname : "";
  const lookable = host && host.includes(".") && !/^[\d.]+$/.test(host) && !host.includes(":");
  return lookable ? `/api/favicon?url=${encodeURIComponent(u.origin)}` : "";
}

// Which agent drives a session, by the name its browser MCP reports ("claude-code 2.1.296",
// "codex", ...): the agent's own mark instead of a generic robot.
const AGENT_SVG = {
  claude: '<path d="M12 3v18M3 12h18M5.64 5.64l12.72 12.72M18.36 5.64 5.64 18.36" fill="none" stroke="#d97757" stroke-width="2.4" stroke-linecap="round"/>',
  // OpenAI's knot (simple-icons).
  codex: '<path fill="currentColor" stroke="none" d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z"/>',
  gemini: '<defs><linearGradient id="ag-gem" x1="3" y1="21" x2="21" y2="3" gradientUnits="userSpaceOnUse"><stop stop-color="#4f8cf7"/><stop offset=".55" stop-color="#9b72f2"/><stop offset="1" stop-color="#f48fb1"/></linearGradient></defs><path d="M12 2c.7 5.6 4.4 9.3 10 10-5.6.7-9.3 4.4-10 10-.7-5.6-4.4-9.3-10-10 5.6-.7 9.3-4.4 10-10z" fill="url(#ag-gem)" stroke="none"/>',
  // The real Antigravity mark (Wikipedia's Google_Antigravity_Logo.svg, as in the
  // google-antigravity-usage HA integration), fitted from its 85x78 box.
  antigravity: '<g stroke="none" transform="translate(-2.365 -2.753) scale(0.2588)"><path d="M89.6992 93.695C94.3659 97.195 101.366 94.8617 94.9492 88.445C75.6992 69.7783 79.7825 18.445 55.8659 18.445C31.9492 18.445 36.0325 69.7783 16.7825 88.445C9.78251 95.445 17.3658 97.195 22.0325 93.695C40.1159 81.445 38.9492 59.8617 55.8659 59.8617C72.7825 59.8617 71.6159 81.445 89.6992 93.695Z" fill="#3186FF"/> <mask id="agy-m" maskUnits="userSpaceOnUse" x="13" y="18" width="85" height="78" style="mask-type: alpha;"><path d="M89.6992 93.695C94.3659 97.195 101.366 94.8617 94.9492 88.445C75.6992 69.7783 79.7825 18.445 55.8659 18.445C31.9492 18.445 36.0325 69.7783 16.7825 88.445C9.78251 95.445 17.3658 97.195 22.0325 93.695C40.1159 81.445 38.9492 59.8617 55.8659 59.8617C72.7825 59.8617 71.6159 81.445 89.6992 93.695Z" fill="black"/></mask> <g mask="url(#agy-m)"><g filter="url(#agy-f0)"><ellipse cx="22.7873" cy="26.8098" rx="22.7873" ry="26.8098" transform="matrix(-0.112784 0.99362 -0.99362 -0.112781 66.2473 -15.5344)" fill="#FFE432"/></g><g filter="url(#agy-f1)"><ellipse cx="96.491" cy="35.1231" rx="29.5007" ry="30.1492" transform="rotate(76.9243 96.491 35.1231)" fill="#FC413D"/></g><g filter="url(#agy-f2)"><ellipse cx="9.02988" cy="41.6647" rx="30.832" ry="39.9417" transform="rotate(74.1257 9.02988 41.6647)" fill="#00B95C"/></g><g filter="url(#agy-f3)"><ellipse cx="9.02988" cy="41.6647" rx="30.832" ry="39.9417" transform="rotate(74.1257 9.02988 41.6647)" fill="#00B95C"/></g><g filter="url(#agy-f4)"><ellipse cx="11.2212" cy="42.8915" rx="30.22" ry="33.2695" transform="rotate(45.6065 11.2212 42.8915)" fill="#00B95C"/></g><g filter="url(#agy-f5)"><ellipse cx="75.7546" cy="104.822" rx="29.0177" ry="27.943" transform="rotate(76.9243 75.7546 104.822)" fill="#3186FF"/></g><g filter="url(#agy-f6)"><ellipse cx="33.5661" cy="35.4043" rx="33.5661" ry="35.4043" transform="matrix(-0.409539 0.912293 -0.912294 -0.409537 101.25 -15.1674)" fill="#FBBC04"/></g><g filter="url(#agy-f7)"><path d="M2.56802 149.695C-15.8116 142.48 15.5987 83.1163 23.4093 63.2203C31.22 43.3244 52.4514 33.0447 70.831 40.26C89.2107 47.4753 110.996 87.2162 103.185 107.112C95.3742 127.008 20.9477 156.91 2.56802 149.695Z" fill="#3186FF"/></g><g filter="url(#agy-f8)"><path d="M113.934 75.8079C109.013 81.5509 96.1724 78.6224 85.253 69.2667C74.3335 59.911 69.4704 47.6711 74.391 41.928C79.3116 36.185 92.1525 39.1136 103.072 48.4692C113.991 57.8249 118.855 70.0648 113.934 75.8079Z" fill="#749BFF"/></g><g filter="url(#agy-f9)"><ellipse cx="92.611" cy="23.7962" rx="44.2411" ry="27.5016" transform="rotate(34.0763 92.611 23.7962)" fill="#FC413D"/></g><g filter="url(#agy-f10)"><ellipse cx="23.4949" cy="29.5887" rx="23.7071" ry="13.7869" transform="rotate(112.516 23.4949 29.5887)" fill="#FFEE48"/></g></g> <defs><filter id="agy-f0" x="2.49348" y="-26.5423" width="69.0899" height="61.2525" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="3.89034" result="b"/></filter><filter id="agy-f1" x="28.7524" y="-32.0333" width="135.477" height="134.313" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="18.8078" result="b"/></filter><filter id="agy-f2" x="-62.2884" y="-21.9253" width="142.637" height="127.18" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.9884" result="b"/></filter><filter id="agy-f3" x="-62.2884" y="-21.9253" width="142.637" height="127.18" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.9884" result="b"/></filter><filter id="agy-f4" x="-52.5697" y="-20.8346" width="127.582" height="127.452" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.9884" result="b"/></filter><filter id="agy-f5" x="17.3619" y="45.4646" width="116.786" height="118.715" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="15.1937" result="b"/></filter><filter id="agy-f6" x="-7.44765" y="-60.4737" width="125.303" height="122.858" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="13.7698" result="b"/></filter><filter id="agy-f7" x="-27.7086" y="13.3597" width="157.119" height="162.029" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="12.297" result="b"/></filter><filter id="agy-f8" x="50.4638" y="16.981" width="87.3973" height="83.7738" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="11.0036" result="b"/></filter><filter id="agy-f9" x="34.2604" y="-28.457" width="116.701" height="104.506" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="9.29385" result="b"/></filter><filter id="agy-f10" x="-15.1522" y="-15.9493" width="77.2941" height="91.076" filterUnits="userSpaceOnUse" color-interpolation-filters="sRGB"><feFlood flood-opacity="0" result="BackgroundImageFix"/><feBlend mode="normal" in="SourceGraphic" in2="BackgroundImageFix" result="shape"/><feGaussianBlur stdDeviation="11.5027" result="b"/></filter></defs></g>',
  opencode: '<rect x="4" y="3.5" width="16" height="17" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.8"/><rect x="8.5" y="8" width="7" height="8" rx="1" fill="currentColor" stroke="none"/>',
  cursor: '<path d="M5 3l15 8.2-6.3 1.8L10.5 20z" fill="currentColor" stroke="none"/>',
  copilot: '<g fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="7" width="18" height="12" rx="6"/><path d="M8 7c0-2.5 2-4 4-4s4 1.5 4 4"/></g><circle cx="9" cy="13" r="1.4" fill="currentColor" stroke="none"/><circle cx="15" cy="13" r="1.4" fill="currentColor" stroke="none"/>',
  // Nous Research's Hermes Agent mark (the "nous-girl" mono variant from NousResearch/hermes-agent,
  // MIT, see THIRD-PARTY.md), downscaled to 48 px: its white outline keeps it legible on dark themes.
  hermes: '<image href="data:image/png;base64,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" width="24" height="24"/>',
  agent: '<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="8" width="16" height="12" rx="3"/><path d="M12 4v4M9.5 13v1M14.5 13v1M9.5 17h5M2 13v3M22 13v3"/></g>',
};
const AGENT_TITLE = { claude: 'Claude', codex: 'Codex', gemini: 'Gemini', antigravity: 'Antigravity', opencode: 'OpenCode', cursor: 'Cursor', copilot: 'Copilot', hermes: 'Hermes', agent: 'Agent' };
// The agent as shown: without its version, and "claude" rather than "claude-code".
const agentShort = (c) => String(c?.agent_name || "").replace(/ \d[\w.-]*/, "").replace(/^claude-code/, "claude");
// Shared session-card and live-view label: agent and topic, or its origin.
function agentLabel(c) {
  if (!c.agent_name) return `Agent · ${c.from}`;
  return c.topic ? `${agentShort(c)}: ${c.topic}` : `${agentShort(c)} · ${c.from}`;
}
function agentBrand(c) {
  const n = String(c?.agent_name || c?.integration || c?.kind || '').toLowerCase();
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

// Markdown for notes, rendered here (no library): the text is escaped first, so
// nothing in it becomes markup but what this makes; links only to http(s) and
// mailto, opened in a new tab without a referrer. Headings, emphasis, inline
// and fenced code, links (and bare URLs), lists (with [ ] / [x]), quotes, rules.
function markdown(src) {
  const safeUrl = (u) => (/^(https?:\/\/|mailto:)/i.test(u) ? u : null);
  const inline = (text) => {
    const codes = [];
    let s = escapeHtml(text).replace(/`([^`\n]+)`/g, (_, c) => `\u0000${codes.push(c) - 1}\u0000`);
    s = s.replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, (m, label, url) => {
      const u = safeUrl(url.replace(/&amp;/g, '&'));
      return u ? `<a href="${escapeHtml(u)}" target="_blank" rel="noopener noreferrer">${label}</a>` : m;
    });
    s = s.replace(/(^|[\s(])((?:https?:\/\/)[^\s<)]+)/g, (m, pre, url) => `${pre}<a href="${url}" target="_blank" rel="noopener noreferrer">${url}</a>`);
    s = s.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>').replace(/__([^_\n]+)__/g, '<strong>$1</strong>');
    s = s.replace(/(^|[^*\w])\*([^*\n]+)\*/g, '$1<em>$2</em>').replace(/(^|[^_\w])_([^_\n]+)_(?!\w)/g, '$1<em>$2</em>');
    s = s.replace(/~~([^~\n]+)~~/g, '<del>$1</del>');
    return s.replace(/\u0000(\d+)\u0000/g, (_, i) => `<code>${codes[+i]}</code>`);
  };
  const lines = String(src || '').replace(/\r\n?/g, '\n').split('\n');
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const fence = line.match(/^\s*```/);
    if (fence) {
      const body = [];
      for (i++; i < lines.length && !/^\s*```/.test(lines[i]); i++) body.push(lines[i]);
      i++;
      out.push(`<pre><code>${escapeHtml(body.join('\n'))}</code></pre>`);
      continue;
    }
    const h = line.match(/^(#{1,3})\s+(.*)$/);
    if (h) { out.push(`<h${h[1].length + 3}>${inline(h[2])}</h${h[1].length + 3}>`); i++; continue; }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { out.push('<hr>'); i++; continue; }
    if (/^\s*>/.test(line)) {
      const body = [];
      for (; i < lines.length && /^\s*>/.test(lines[i]); i++) body.push(lines[i].replace(/^\s*>\s?/, ''));
      out.push(`<blockquote>${markdown(body.join('\n'))}</blockquote>`);
      continue;
    }
    const item = (l) => l.match(/^\s*(?:([-*+])|(\d+)[.)])\s+(.*)$/);
    if (item(line)) {
      const ordered = !!item(line)[2];
      const items = [];
      for (; i < lines.length && item(lines[i]) && !!item(lines[i])[2] === ordered; i++) {
        let text = item(lines[i])[3];
        const box = text.match(/^\[([ xX])\]\s+(.*)$/);
        text = box ? `<input type="checkbox" disabled${box[1] !== ' ' ? ' checked' : ''}> ${inline(box[2])}` : inline(text);
        items.push(`<li${box ? ' class="task"' : ''}>${text}</li>`);
      }
      out.push(ordered ? `<ol>${items.join('')}</ol>` : `<ul>${items.join('')}</ul>`);
      continue;
    }
    if (!line.trim()) { i++; continue; }
    const para = [];
    for (; i < lines.length && lines[i].trim() && !/^\s*(```|#{1,3}\s|>|([-*+]|\d+[.)])\s)/.test(lines[i]); i++) para.push(inline(lines[i]));
    if (!para.length) { para.push(inline(lines[i])); i++; }
    out.push(`<p>${para.join('<br>')}</p>`);
  }
  return out.join('');
}

// A link laid out to be read: its site, its path and (muted) its last part, the
// long token of a public link. It wraps anywhere rather than being cut off.
function urlHtml(url) {
  let u;
  try { u = new URL(url); } catch { return escapeHtml(url); }
  const parts = u.pathname.split('/');
  const last = parts.pop();
  return `<span class="u-site">${escapeHtml(u.origin)}</span><span class="u-path">${escapeHtml(parts.join('/') + '/')}</span><span class="u-token">${escapeHtml(last + u.search + u.hash)}</span>`;
}

// A picture (a QR code) large over everything; a click or Esc closes it. Placed in
// the open dialog, if any, since a modal dialog is above the rest of the page.
function zoomImage(src, alt) {
  const box = document.createElement('div');
  box.className = 'zoom-overlay';
  box.setAttribute('role', 'dialog');
  box.setAttribute('aria-label', alt);
  box.innerHTML = `<img src="${escapeHtml(src)}" alt="${escapeHtml(alt)}"><span>Click or press Esc to close</span>`;
  const close = () => { box.remove(); removeEventListener('keydown', onKey, true); };
  const onKey = (e) => { if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); close(); } };
  box.addEventListener('click', close);
  addEventListener('keydown', onKey, true);
  (document.querySelector('dialog[open]') || document.body).append(box);
}

/* ---------------------------------------------------------------- keyboard shortcuts
   One list for the dashboard and the live view: the cheat-sheet in both, and the results of
   the dashboard's search. `scopes` says where a shortcut works: "*dash" (the whole
   dashboard), a view of it ("sessions", "profiles", "recordings", "nodes", "engines",
   "labels"), "player" (the recording player) or "live" (the live view). Keep it in step
   with the key handlers; "Mod" is Ctrl, or Cmd on a Mac. */
const BD_MAC = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent || "");
const BD_VIEWS = ["sessions", "profiles", "recordings", "nodes", "engines", "labels"];
const BD_SHORTCUTS = [
  { where: "Search", scopes: ["*dash"], keys: [["Mod", "K"]], desc: "Search everything: actions, settings, sessions, profiles, recordings, nodes, labels" },
  { where: "Search", scopes: ["*dash"], keys: [["↑"], ["↓"], ["Ctrl", "N"], ["Ctrl", "P"]], desc: "Move through the results" },
  { where: "Search", scopes: ["*dash"], keys: [["Enter"]], desc: "Open the selected result" },
  { where: "Search", scopes: ["*dash"], keys: [["Tab"], ["Shift", "Tab"]], desc: "Narrow the results to the next or previous kind" },
  { where: "Dashboard", scopes: ["*dash"], keys: [["Alt", "N"]], desc: "New session (opens the dialog)" },
  { where: "Dashboard", scopes: ["*dash"], keys: [["Double-click"]], desc: "On the New button: start a session right away, with the defaults and no start page" },
  { where: "Dashboard", scopes: ["*dash"], keys: [["?"]], desc: "Show the shortcuts of the current view" },
  { where: "Dashboard", scopes: ["*dash"], keys: [["Esc"]], desc: "Close the open dialog, sheet or menu" },
  { where: "Dashboard", scopes: BD_VIEWS.filter((v) => v !== "engines"), keys: [["Right-click"], ["Shift", "F10"]], desc: "Actions of a session, profile, recording, node or label (Shift+right-click: the browser's menu)" },
  { where: "Dashboard", scopes: ["sessions", "profiles", "nodes"], keys: [["Enter"], ["Space"]], desc: "Open the focused card or row" },
  { where: "Notes", scopes: ["*dash"], keys: [["Mod", "Enter"]], desc: "Save the note you are writing" },
  { where: "Recording player", scopes: ["player"], keys: [["Space"], ["K"]], desc: "Play or pause" },
  { where: "Recording player", scopes: ["player"], keys: [["→"], ["L"], ["."]], desc: "Next frame" },
  { where: "Recording player", scopes: ["player"], keys: [["←"], ["J"], [","]], desc: "Previous frame" },
  { where: "Recording player", scopes: ["player"], keys: [["Shift", "→"], ["Shift", "←"]], desc: "Jump 5 seconds forward or back" },
  { where: "Recording player", scopes: ["player"], keys: [["0"], ["…"], ["9"]], desc: "Jump to 0% … 90% of the recording" },
  { where: "Recording player", scopes: ["player"], keys: [["M"]], desc: "Mute or unmute (recordings with sound)" },
  { where: "Recording player", scopes: ["player"], keys: [["↑"], ["↓"]], desc: "Volume (recordings with sound)" },
  { where: "Recording player", scopes: ["player"], keys: [["F"]], desc: "Full screen" },
  { where: "Recording player", scopes: ["player"], keys: [["Ctrl", "scroll"], ["Pinch"]], desc: "Zoom the picture at the pointer" },
  { where: "Live view", scopes: ["live"], keys: [["Alt", "L"]], desc: "Go to this view's address bar" },
  { where: "Live view", scopes: ["live"], keys: [["Alt", "Q"], ["Alt", "W"]], desc: "Stop the session (asks first)" },
  { where: "Live view", scopes: ["live"], keys: [["Alt", "D"]], desc: "Back to the list of sessions" },
  { where: "Live view", scopes: ["live"], keys: [["Alt", "H"]], desc: "Show these shortcuts" },
  { where: "Live view", scopes: ["live"], keys: [["Ctrl", "+"], ["Ctrl", "-"], ["Ctrl", "0"]], desc: "Zoom this view's picture in, out, back to 100% (the page does not see it)" },
  { where: "Live view", scopes: ["live"], keys: [["Mod", "A"], ["Mod", "Z"], ["Mod", "Y"]], desc: "Select all, undo, redo in the page" },
  { where: "Live view", scopes: ["live"], keys: [["Mod", "C"], ["Mod", "X"], ["Mod", "V"]], desc: "Copy, cut and paste between this device and the page" },
  { where: "Live view", scopes: ["live"], keys: [["Esc"]], desc: "Close the open list or dialog; otherwise the page gets it" },
  { where: "Live view", scopes: ["live"], keys: [["Any key"]], desc: "Typing and named keys (Enter, Tab, arrows, …) go to the page while you control it" },
  { where: "Live view", scopes: ["live"], keys: [["Mod", "T"], ["Mod", "W"], ["…"]], desc: "With “Capture keys” on, other Ctrl shortcuts go to the page instead of this browser" },
];
const bdKey = (k) => (k === "Mod" ? (BD_MAC ? "⌘" : "Ctrl") : k === "Alt" && BD_MAC ? "⌥" : k);
const bdKeysText = (sc) => sc.keys.map((chord) => chord.map(bdKey).join("+")).join(" or ");
// The cheat-sheet inside `host`: the shortcuts of `scopes` (this view), or all of them, and a filter.
function bdShortcutsPanel(host, { scopes = [], label = "", all = false, filter = "" } = {}) {
  let mode = scopes.length && !all ? "view" : "all", q = filter;
  host.innerHTML = `<div class="kb-tools"><div class="kb-seg" role="group" aria-label="Show">
      ${scopes.length ? `<button type="button" data-mode="view">${esc(label || "This view")}</button>` : ""}<button type="button" data-mode="all">Everything</button></div>
    <input class="kb-filter" type="search" placeholder="Filter shortcuts…" aria-label="Filter shortcuts" autocomplete="off" spellcheck="false"></div><div class="kb-list" role="list"></div>`;
  const input = host.querySelector(".kb-filter"), list = host.querySelector(".kb-list");
  input.value = q;
  const render = () => {
    host.querySelectorAll("[data-mode]").forEach((b) => b.classList.toggle("on", b.dataset.mode === mode));
    const words = q.toLowerCase().split(/\s+/).filter(Boolean);
    const rows = BD_SHORTCUTS.filter((sc) => (mode === "all" || sc.scopes.some((s) => scopes.includes(s)))
      && words.every((w) => `${sc.desc} ${sc.where} ${bdKeysText(sc)}`.toLowerCase().includes(w)));
    const groups = [...new Set(rows.map((sc) => sc.where))];
    list.innerHTML = groups.map((g) => `<div class="kb-group" role="listitem"><h3>${esc(g)}</h3>${rows.filter((sc) => sc.where === g).map((sc) =>
      `<div class="kb-row"><span class="kb-desc">${esc(sc.desc)}</span><span class="kb-keys">${sc.keys.map((chord) =>
        `<span class="kb-chord">${chord.map((k) => `<kbd>${esc(bdKey(k))}</kbd>`).join("")}</span>`).join('<i>or</i>')}</span></div>`).join("")}</div>`).join("")
      || `<p class="kb-none">No shortcut matches “${esc(q.trim())}”.</p>`;
  };
  host.querySelectorAll("[data-mode]").forEach((b) => b.addEventListener("click", () => { mode = b.dataset.mode; render(); }));
  input.addEventListener("input", () => { q = input.value; render(); });
  render();
  return input;
}
