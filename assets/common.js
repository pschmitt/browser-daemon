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
  antigravity: '<path d="M3.5 20.5 12 4l8.5 16.5M8.4 14.6h7.2" fill="none" stroke="#5b8dee" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/>',
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
