/* Signals: small facts derived from all tabs (a tab playing sound, a meeting
 * open, own rules), sent to the paired daemon, which pushes them to Home
 * Assistant (docs/local-api.md). Opt-in. Private windows never count, and
 * only these facts leave the browser, not the tab list. */
globalThis.bdSignals = (() => {
  const ext = globalThis.browser || chrome;
  const NAME = /^[a-z][a-z0-9_]{0,31}$/;
  const BUILT_IN = ['media', 'meeting'];
  // Tabs that mean a meeting is open, by provider.
  const MEETINGS = [
    ['zoom', /^https:\/\/([a-z0-9-]+\.)?zoom\.us\/(wc|j|s)\//i],
    ['jitsi', /^https:\/\/meet\.jit\.si\/[^/?#]+/i],
    ['teams', /^https:\/\/(teams\.microsoft\.com\/v2|teams\.cloud\.microsoft)/i],
    ['meet', /^https:\/\/meet\.google\.com\/[a-z]+-[a-z]+-[a-z]+/i],
  ];
  let send = null;
  let enabled = false;
  let managedOn = false;
  let rules = [];
  let last = '';
  let timer = null;

  function site(url) {
    try { return new URL(url).hostname; } catch { return ''; }
  }

  // Own rules from the `signalRules` policy: {name, url, title, match},
  // regexes, case-insensitive. `match` tests the title or the URL; a tab must
  // pass every pattern the rule has.
  function compile(list) {
    return (Array.isArray(list) ? list : []).slice(0, 30).map(rule => {
      if (!rule || !NAME.test(rule.name) || BUILT_IN.includes(rule.name)) return null;
      try {
        const url = rule.url ? new RegExp(rule.url, 'i') : null;
        const title = rule.title ? new RegExp(rule.title, 'i') : null;
        const match = rule.match ? new RegExp(rule.match, 'i') : null;
        return url || title || match ? {name: rule.name, url, title, match} : null;
      } catch { return null; }
    }).filter(Boolean);
  }

  function derive(tabs) {
    const open = tabs.filter(t => !t.incognito);
    const audible = open.filter(t => t.audible && !t.mutedInfo?.muted);
    const playing = audible.find(t => t.active) || audible[0];
    const signals = {
      media: {on: !!playing, attributes: playing ? {title: (playing.title || '').slice(0, 1024), site: site(playing.url)} : {}},
    };
    let meeting = null;
    for (const tab of open) {
      const hit = MEETINGS.find(([, pattern]) => pattern.test(tab.url || ''));
      if (hit) { meeting = {provider: hit[0], url: (tab.url || '').slice(0, 1024)}; break; }
    }
    signals.meeting = {on: !!meeting, attributes: meeting || {}};
    for (const rule of rules) {
      const count = open.filter(t => (!rule.url || rule.url.test(t.url || '')) &&
        (!rule.title || rule.title.test(t.title || '')) &&
        (!rule.match || rule.match.test(t.title || '') || rule.match.test(t.url || ''))).length;
      signals[rule.name] = {on: count > 0, attributes: {count}};
    }
    return signals;
  }

  async function publish(force) {
    timer = null;
    if (!send || !enabled) return;
    const signals = derive(await ext.tabs.query({}));
    const text = JSON.stringify(signals);
    if (!force && text === last) return;
    last = text;
    send({type: 'signals', signals});
  }

  function publishSoon() {
    if (!send || !enabled || timer) return;
    timer = setTimeout(() => void publish(false), 300);
  }

  async function refresh() {
    let managed = {};
    try { managed = await ext.storage.managed.get(['signals', 'signalRules']); } catch { /* No policy. */ }
    const {signals: local} = await ext.storage.local.get('signals');
    const was = enabled;
    managedOn = managed.signals === true;
    enabled = managedOn || local === true;
    rules = compile(managed.signalRules);
    // Switched off: clear them on the daemon instead of leaving them stale.
    if (was && !enabled && send) { last = ''; send({type: 'signals', signals: {}}); }
    if (enabled) await publish(true);
  }

  // The daemon announced support (`registered.signals`): start sending.
  function attach(sender) {
    send = sender;
    last = '';
    void refresh();
  }

  function detach() {
    send = null;
    last = '';
    clearTimeout(timer);
    timer = null;
  }

  async function setEnabled(value) {
    await ext.storage.local.set({signals: value === true});
    await refresh();
  }

  function status() {
    return {enabled, managed: managedOn, connected: !!send, rules: rules.map(r => r.name)};
  }

  ext.tabs.onCreated.addListener(publishSoon);
  ext.tabs.onRemoved.addListener(publishSoon);
  ext.tabs.onActivated.addListener(publishSoon);
  ext.tabs.onReplaced?.addListener(publishSoon);
  ext.tabs.onUpdated.addListener((id, change) => {
    if (change.url !== undefined || change.title !== undefined || change.audible !== undefined ||
        change.mutedInfo !== undefined) publishSoon();
  });
  ext.storage.onChanged.addListener((changes, area) => {
    if ((area === 'local' || area === 'managed') && (changes.signals || changes.signalRules)) void refresh();
  });

  return {attach, detach, setEnabled, status, derive};
})();
