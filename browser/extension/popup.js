const api = globalThis.browser || chrome;
const $ = id => document.getElementById(id);
const origin = api.runtime.getURL('').replace(/\/$/, '');
// Firefox's URL.origin is the extension origin too; never use runtime.id (the
// Gecko add-on ID is different from the runtime's installation UUID).
$('origin').textContent = origin;
$('pairCommand').textContent = `browser-daemon --url https://YOUR-DAEMON --secret-file /PATH/TO/SECRET extension pair --origin ${origin}`;
if (new URLSearchParams(location.search).has('tab')) document.body.classList.add('tab');
let state;
let initialized = false;
let setupRemembered;
let busy = false;
let popupError = '';
let dismissedConnectionStatus = '';
let renderedTabs = '';
let renderedAllowlist = '';
let approvalIdentity = '';
const draftReady = api.storage.session.get('pairingDraft').then(saved => {
  if (saved.pairingDraft?.expires > Date.now()) $('code').value = saved.pairingDraft.code;
  else void api.storage.session.remove('pairingDraft');
});

const server = () => ($('serverChoice').hidden ? $('server').value : $('serverChoice').value).trim();
function saveSettings() {
  return api.storage.local.set({server: server(), name: $('name').value, rememberPreference: $('rememberKey').checked});
}
for (const id of ['server', 'serverChoice', 'name', 'rememberKey']) $(id).addEventListener('input', () => void saveSettings().catch(showError));
$('code').addEventListener('input', () => {
  const code = $('code').value;
  void (code ? api.storage.session.set({pairingDraft: {code, expires: Date.now() + 300000}})
    : api.storage.session.remove('pairingDraft')).catch(showError);
});
const permissionPattern = address => { const u = new URL(address); return `${u.protocol}//${u.hostname}/*`; };

async function message(value) {
  const response = await api.runtime.sendMessage(value);
  if (response?.error) throw new Error(response.error);
  return response;
}
function renderError(connectionStatus = '') {
  const status = connectionStatus === dismissedConnectionStatus ? '' : connectionStatus;
  $('errorText').textContent = popupError || status;
  $('error').hidden = !popupError && !status;
}
function showError(error) { popupError = error.message; renderError(); }
function clearError() { popupError = ''; renderError(); }
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

function renderHeader() {
  const reg = state.registration;
  const sharingSummary = state.sharing.all ? 'All current and future web tabs will be shared.'
    : state.sharing.autoShare ? 'New web tabs will be shared automatically. Choose existing tabs after connecting.'
    : 'Choose tabs to share after connecting.';
  const soundSummary = state.sharing.audio ? 'Viewers can hear media playing in shared tabs.' : 'Sound sharing is off.';
  $('approvalSharing').textContent = sharingSummary;
  $('approvalSound').textContent = soundSummary;
  $('setupSharing').textContent = `${sharingSummary} ${soundSummary}${state.sharing.control ? ' Remote control is enabled.' : ''}`;
  const companionMissing = !state.chromium && !state.nativeReady;
  $('inputMode').textContent = companionMissing ? 'Companion not detected'
    : 'Trusted mouse and keyboard input';
  $('inputMode').hidden = !companionMissing;
  $('title').textContent = reg ? (state.saved.name || 'Browser node') : 'Browser node';
  // The version where the eye looks first; the daemon gets room in the main card.
  $('subtitle').textContent = `v${state.version}${state.nativeReady ? ' · companion' : ''}`;
  $('subtitle').title = reg ? '' : state.status;
  $('pill').textContent = reg ? 'Connected' : state.connecting ? 'Connecting' : 'Offline';
  $('pill').className = `pill status-pill${reg ? ' on' : state.connecting ? ' wait' : ' off'}`;
  $('pill').disabled = state.connecting || (!reg && !state.rememberedBrowser);
  $('pill').title = reg ? 'Disconnect this browser' : state.connecting ? 'Connecting'
    : state.rememberedBrowser ? 'Reconnect this browser' : 'Offline';
  const connectionStatus = !reg && !!state.rememberedBrowser && !state.connecting &&
    state.status && state.status !== 'Disconnected' ? state.status : '';
  if (connectionStatus !== dismissedConnectionStatus) dismissedConnectionStatus = '';
  renderError(connectionStatus);
}

$('dismissError').addEventListener('click', () => {
  popupError = '';
  if (!state?.registration && state?.rememberedBrowser && !state.connecting) dismissedConnectionStatus = state.status;
  renderHeader();
});

function favicon(tab) {
  const box = document.createElement('span');
  box.className = 'fav';
  const letter = () => { box.replaceChildren(); box.textContent = (new URL(tab.url).hostname.replace(/^www\./, '')[0] || '?').toUpperCase(); };
  if (tab.icon) {
    const img = document.createElement('img');
    img.alt = ''; img.src = tab.icon; img.referrerPolicy = 'no-referrer';
    img.addEventListener('error', letter);
    box.append(img);
  } else letter();
  return box;
}

function renderTabs() {
  const signature = JSON.stringify(state.tabs);
  if (signature === renderedTabs) return;
  renderedTabs = signature;
  const shared = state.tabs.filter(t => t.shared).length;
  $('sharedCount').textContent = state.tabs.length ? `${shared}/${state.tabs.length}` : '';
  const all = state.sharing.all;
  $('shareAllTabs').textContent = shared === state.tabs.length && shared ? 'Unshare all' : 'Share all';
  $('shareAllTabs').hidden = !state.tabs.length || all;
  $('tabs').classList.toggle('locked', all);
  $('noTabs').hidden = !!state.tabs.length;
  $('tabs').hidden = !state.tabs.length;
  $('tabs').replaceChildren(...state.tabs.map(tab => {
    const item = document.createElement('li');
    item.className = tab.shared ? 'on' : '';
    const label = document.createElement('label');
    const text = document.createElement('span');
    text.className = 'tab-text';
    const title = document.createElement('b');
    title.textContent = tab.title || tab.url;
    const host = document.createElement('small');
    host.textContent = new URL(tab.url).host + (tab.active ? ' · current tab' : '');
    text.append(title, host);
    const toggle = document.createElement('input');
    toggle.type = 'checkbox'; toggle.className = 'switch'; toggle.checked = tab.shared; toggle.dataset.id = tab.id;
    toggle.disabled = all;
    if (all) toggle.title = 'Every tab is shared: turn off "Share all tabs" to choose';
    toggle.setAttribute('aria-label', `Share ${tab.title || tab.url}`);
    label.append(favicon(tab), text, toggle);
    item.append(label);
    return item;
  }));
}

function renderDaemonAllowlist(origins) {
  const managed = state.allowlist.managed;
  const mode = state.allowlist.mode || 'custom';
  const signature = JSON.stringify({origins, managed, mode, discoveryOrigin: state.allowlist.discoveryOrigin, error: state.allowlist.error});
  if (signature === renderedAllowlist) return;
  renderedAllowlist = signature;
  $('daemonAllowlist').replaceChildren(...origins.map(value => {
    const url = new URL(value);
    const row = document.createElement('div');
    row.className = 'daemon-origin';
    row.setAttribute('role', 'listitem');

    const icon = document.createElement('span');
    icon.className = 'daemon-origin-icon';
    icon.setAttribute('aria-hidden', 'true');
    icon.innerHTML = '<svg viewBox="0 0 24 24"><path d="M12 3 4.5 6v5.5c0 4.5 3.1 7.8 7.5 9.5 4.4-1.7 7.5-5 7.5-9.5V6L12 3Z"/><path d="m9 12 2 2 4-4"/></svg>';

    const text = document.createElement('span');
    text.className = 'daemon-origin-text';
    const host = document.createElement('b');
    host.textContent = url.host;
    const address = document.createElement('small');
    address.textContent = value;
    text.append(host, address);
    row.append(icon, text);

    if (!managed && mode === 'custom') {
      const remove = document.createElement('button');
      remove.type = 'button';
      remove.className = 'daemon-origin-remove';
      remove.setAttribute('aria-label', `Remove ${value}`);
      remove.title = 'Remove daemon';
      remove.innerHTML = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 7 10 10M17 7 7 17"/></svg>';
      remove.addEventListener('click', async () => {
        try {
          await message({type: 'setDaemonAllowlist', origins: origins.filter(origin => origin !== value)});
          await refresh();
        } catch (error) { showError(error); }
      });
      row.append(remove);
    }
    return row;
  }));

  if (!origins.length) {
    const empty = document.createElement('p');
    empty.className = 'daemon-origin-empty muted small';
    empty.textContent = mode === 'auto' ? 'The automatic origin list is unavailable.'
      : managed ? 'No daemon origins are configured.' : 'Add a daemon origin to pair this browser.';
    $('daemonAllowlist').append(empty);
  }
  $('addDaemonOrigin').hidden = managed || mode !== 'custom';
  $('allowlistMode').value = mode;
  $('allowlistMode').disabled = managed;
  $('discoveryOriginInput').value = state.allowlist.discoveryOrigin || '';
  $('discoveryOriginInput').disabled = managed;
  $('discoveryConfig').hidden = managed || mode !== 'auto';
  if (state.allowlist.error) $('allowlistStatus').textContent = state.allowlist.error;
}

async function refresh() {
  await draftReady;
  state = await message({type: 'state'});
  const reg = state.registration;
  renderHeader();
  $('approval').hidden = !state.dashboardRequest;
  if (state.dashboardRequest && approvalIdentity !== state.dashboardRequest.server) {
    approvalIdentity = state.dashboardRequest.server;
    $('approvalRemember').checked = state.dashboardRequest.remember;
    $('approvalControl').checked = state.dashboardRequest.control;
    $('approvalName').value = state.saved.name || state.defaultName;
  }
  if (!state.dashboardRequest) approvalIdentity = '';
  if (state.dashboardRequest) $('approvalHost').textContent = new URL(state.dashboardRequest.server).host;
  document.body.classList.toggle('asking', !!state.dashboardRequest);
  $('connected').hidden = !reg;
  $('setup').hidden = !!reg || !!state.dashboardRequest;

  // Setup
  const origins = state.allowlist.origins;
  $('serverChoice').hidden = !origins.length;
  $('server').hidden = !!origins.length;
  if (JSON.stringify([...$('serverChoice').options].map(o => o.value)) !== JSON.stringify(origins)) {
    $('serverChoice').replaceChildren(...origins.map(o => new Option(new URL(o).host, o)));
  }
  if (!initialized) {
    $('server').value = state.saved.server || '';
    if (origins.includes(state.saved.server)) $('serverChoice').value = state.saved.server;
    $('name').value = state.saved.name || '';
    $('name').placeholder = state.defaultName;
    $('rememberKey').checked = state.saved.rememberPreference === true;
    initialized = true;
  }
  $('remembered').hidden = !state.rememberedBrowser || !!reg;
  const hasRemembered = !!state.rememberedBrowser;
  if (setupRemembered !== hasRemembered) {
    $('newConnection').open = !hasRemembered;
    setupRemembered = hasRemembered;
  }
  $('newConnectionTitle').textContent = hasRemembered ? 'Connect a different browser' : 'Connect this browser';
  if (state.rememberedBrowser) {
    const host = new URL(state.rememberedBrowser.server).host;
    $('rememberedStatus').textContent = state.connecting
      ? `Reconnecting ${state.rememberedBrowser.name} to ${host}…`
      : `Disconnected.\nReconnect to ${host}?`;
  }
  $('forgetRemembered').hidden = !state.rememberedBrowser;
  // Forgetting has to tell the daemon, so it only works while connected.
  $('forgetRemembered').disabled = !reg;
  $('forgetHint').hidden = !state.rememberedBrowser || !!reg;

  // Connected
  if (reg) {
    $('code').value = '';
    if (document.activeElement !== $('control')) $('control').checked = state.sharing.control;
    if (document.activeElement !== $('autoShare')) $('autoShare').checked = state.sharing.autoShare;
    if (document.activeElement !== $('shareAll')) $('shareAll').checked = state.sharing.all;
    if (document.activeElement !== $('follow')) $('follow').checked = state.sharing.follow;
    if (document.activeElement !== $('shareFiles')) $('shareFiles').checked = state.sharing.files;
    if (document.activeElement !== $('sharePublic')) $('sharePublic').checked = state.sharing.public;
    if (document.activeElement !== $('shareAudio')) $('shareAudio').checked = state.sharing.audio;
    if (document.activeElement !== $('addressSuggestions')) $('addressSuggestions').checked = state.addressSuggestions;
    $('autoShareRow').hidden = state.sharing.all;
    $('serverHost').textContent = new URL(reg.server).host;
    $('serverHost').title = reg.server;
    $('dashboard').href = `${reg.server}/#nodes`;
    renderTabs();
  } else renderedTabs = '';

  // Advanced
  const autoStatus = state.allowlist.mode === 'auto'
    ? `Loaded from ${new URL(state.allowlist.discoveryOrigin).host}; updates arrive over the daemon connection.`
    : state.allowlist.managed ? 'Set by your system configuration.' : 'Only exact daemon origins are trusted.';
  $('allowlistStatus').textContent = state.allowlist.error || (state.allowlist.managed && state.allowlist.mode === 'auto'
    ? `${autoStatus} Set by your system configuration.` : autoStatus);
  renderDaemonAllowlist(origins);
  if (!origins.length && !state.allowlist.managed) $('advanced').open = true;
  $('companion').hidden = state.chromium;
  $('nativeLabel').textContent = state.nativeReady ? 'Disable companion' : 'Enable companion';
  $('native').disabled = !reg;
  $('diagnosticStatus').textContent = reg?.id || state.rememberedBrowser?.name || 'Not connected';
  $('connectionExpiry').textContent = reg
    ? reg.expires_at ? new Date(reg.expires_at * 1000).toLocaleString() : 'Never'
    : 'Not connected';
  $('connectionHistory').replaceChildren(...state.history.slice().reverse().map(event => {
    const row = document.createElement('li');
    const severity = event.message === 'Connected' ? 'success'
      : /disconnect|closed|failed|rejected|expired|revoked|invalid/i.test(event.message) ? 'failure' : 'pending';
    row.className = `history-event ${severity}`;
    const marker = document.createElement('span');
    marker.className = 'history-marker';
    marker.setAttribute('aria-hidden', 'true');
    const time = document.createElement('time');
    time.dateTime = new Date(event.at).toISOString();
    time.textContent = new Date(event.at).toLocaleTimeString([], {hour: 'numeric', minute: '2-digit'});
    const messageText = document.createElement('span');
    messageText.textContent = event.message;
    row.append(marker, time, messageText);
    return row;
  }));
}

// Firefox captures tabs without its companion only with access to all sites.
async function capturePermission() {
  if (state.chromium) return;
  const sitePermission = {origins: ['<all_urls>']};
  if (!(await api.permissions.contains(sitePermission))
      && !(await api.permissions.request(sitePermission))) throw new Error('Firefox needs access to your sites to show shared tabs.');
}
async function share(ids, control = $('control').checked) {
  if (busy) return;
  busy = true; clearError();
  try {
    const urls = Object.fromEntries(state.tabs.filter(t => ids.includes(t.id)).map(t => [t.id, t.url]));
    await message({type: 'share', tabs: ids, urls, control});
    renderedTabs = '';
    await refresh();
  } catch (error) { showError(error); renderedTabs = ''; await refresh().catch(() => {}); }
  finally { busy = false; }
}
const sharedIds = () => state.tabs.filter(t => t.shared).map(t => t.id);

// Permission requests must start in the click itself (a user gesture).
$('tabs').addEventListener('click', event => {
  const toggle = event.target.closest('input.switch');
  if (!toggle) return;
  const id = Number(toggle.dataset.id);
  const ids = toggle.checked ? [...sharedIds(), id] : sharedIds().filter(t => t !== id);
  void (toggle.checked ? capturePermission() : Promise.resolve()).then(() => share(ids), error => { toggle.checked = false; showError(error); });
});
$('shareAllTabs').addEventListener('click', event => {
  event.preventDefault();
  event.stopPropagation();
  const all = sharedIds().length === state.tabs.length;
  void (all ? Promise.resolve() : capturePermission()).then(() => share(all ? [] : state.tabs.map(t => t.id)), showError);
});
$('control').addEventListener('change', () => {
  clearError();
  void message({type: 'setControl', control: $('control').checked}).then(refresh, showError);
});
$('shareAll').addEventListener('click', () => {
  const enabled = $('shareAll').checked;
  void (enabled ? capturePermission() : Promise.resolve())
    .then(() => message({type: 'shareAll', enabled}))
    .then(() => { renderedTabs = ''; return refresh(); }, error => { $('shareAll').checked = !enabled; showError(error); });
});
$('autoShare').addEventListener('click', () => {
  const enabled = $('autoShare').checked;
  void (enabled ? capturePermission() : Promise.resolve())
    .then(() => message({type: 'autoShare', enabled}))
    .then(refresh, error => { $('autoShare').checked = !enabled; showError(error); });
});

$('follow').addEventListener('click', () => {
  const enabled = $('follow').checked;
  void message({type: 'follow', enabled}).then(refresh, error => { $('follow').checked = !enabled; showError(error); });
});

$('connect').addEventListener('submit', async event => {
  event.preventDefault();
  if (busy) return;
  busy = true; clearError();
  try {
    const address = NodePolicy.approvedServer(server(), state?.allowlist.origins || []);
    const prepared = message({type: 'prepareConnect', server: address, code: $('code').value.trim(), name: $('name').value || state.defaultName, remember: $('rememberKey').checked});
    const permission = api.permissions.request({origins: [permissionPattern(address)]});
    await prepared;
    if (!(await permission)) { await message({type: 'cancelConnect'}); throw new Error('Allow access to the daemon to connect.'); }
    await message({type: 'finishConnect'});
    await refresh();
  } catch (error) { showError(error); }
  finally { busy = false; }
});
$('pill').addEventListener('click', async () => {
  if (!state.registration && !state.rememberedBrowser) return;
  try {
    await message({type: state.registration ? 'stop' : 'resumeRemembered'});
    await refresh();
  }
  catch (error) { showError(error); }
});
$('native').addEventListener('click', async () => {
  clearError();
  try {
    const nativePermission = {permissions: ['nativeMessaging']};
    if (!state.nativeReady && !(await api.permissions.contains(nativePermission))
        && !(await api.permissions.request(nativePermission))) throw new Error('Native messaging permission was denied.');
    await message({type: 'native', enable: !state.nativeReady});
    await refresh();
  } catch (error) { showError(error); }
});
$('copyCompanionCommand').addEventListener('click', async event => {
  const button = event.currentTarget;
  try {
    await navigator.clipboard.writeText($('companionCommand').textContent.trim());
    button.title = 'Copied';
    button.setAttribute('aria-label', 'Copied setup commands');
  } catch {
    button.title = 'Could not copy';
    button.setAttribute('aria-label', 'Could not copy setup commands');
  }
  window.setTimeout(() => {
    button.title = 'Copy setup commands';
    button.setAttribute('aria-label', 'Copy setup commands');
  }, 1600);
});
async function pairFromDashboard(address, name) {
  clearError();
  try {
    address = NodePolicy.approvedServer(address, state?.allowlist.origins || []);
    const configured = message({type: 'configureDashboard', server: address, name});
    // Every allowed daemon at once: any of their dashboards can pair afterwards.
    const origins = [...new Set([address, ...state.allowlist.origins].map(permissionPattern))];
    const permission = api.permissions.request({origins});
    await configured;
    if (!(await permission)) throw new Error('Allow access to your daemons to pair from their dashboards.');
    await message({type: 'configureDashboard', server: address, name});
    await api.tabs.create({url: `${address}/#nodes/add`});
  } catch (error) { showError(error); }
}
$('enableDashboard').addEventListener('click', () => pairFromDashboard(server(), $('name').value || state.defaultName));
$('repairRemembered').addEventListener('click', () => pairFromDashboard(state.rememberedBrowser.server, state.rememberedBrowser.name));
$('approvePairing').addEventListener('click', async () => {
  try {
    const name = $('approvalName').value.trim() || state.defaultName;
    $('name').value = name; await saveSettings();
    await message({type: 'approveDashboard', name, remember: $('approvalRemember').checked, control: $('approvalControl').checked});
    await refresh();
  } catch (error) { showError(error); }
});
$('rejectPairing').addEventListener('click', async () => {
  try { await message({type: 'rejectDashboard'}); await refresh(); }
  catch (error) { showError(error); }
});
$('resumeRemembered').addEventListener('click', async () => {
  try { await message({type: 'resumeRemembered'}); await refresh(); }
  catch (error) { showError(error); }
});
$('forgetRemembered').addEventListener('click', async () => {
  try { await message({type: 'forgetRemembered'}); await refresh(); }
  catch (error) { showError(error); }
});
$('addDaemonOrigin').addEventListener('submit', async event => {
  event.preventDefault();
  try {
    const value = $('daemonOriginInput').value.trim();
    if (!value) throw new Error('Enter a daemon origin.');
    const origins = NodePolicy.daemonOrigins([...state.allowlist.origins, value]);
    await message({type: 'setDaemonAllowlist', origins});
    $('daemonOriginInput').value = '';
    await refresh();
  } catch (error) { showError(error); }
});
$('allowlistMode').addEventListener('change', async () => {
  const mode = $('allowlistMode').value;
  if (mode === 'auto') {
    $('discoveryConfig').hidden = false;
    return;
  }
  try {
    await message({type: 'setDaemonOriginMode', mode});
    await refresh();
  } catch (error) { showError(error); await refresh(); }
});
$('discoveryConfig').addEventListener('submit', async event => {
  event.preventDefault();
  try {
    await message({type: 'setDaemonOriginMode', mode: 'auto', discoveryOrigin: $('discoveryOriginInput').value.trim()});
    await refresh();
  } catch (error) { showError(error); }
});

void refresh().catch(showError);
setInterval(() => { if (!busy) void refresh().catch(showError); }, 1500);

$('shareAudio').addEventListener('click', () => {
  const enabled = $('shareAudio').checked;
  // Chromium's debugger grant covers pictures/input, but scripting media capture
  // also needs site access. Request it in this local user gesture.
  const permission = enabled ? api.permissions.request({origins: state.chromium
    ? ['http://*/*', 'https://*/*'] : ['<all_urls>']}) : Promise.resolve(true);
  void permission.then(allowed => {
    if (!allowed) throw new Error('Site access is needed to share media sound.');
    return message({type: 'shareAudio', enabled});
  }).then(refresh, error => { $('shareAudio').checked = !enabled; showError(error); });
});

$('shareFiles').addEventListener('click', () => {
  const enabled = $('shareFiles').checked;
  const requested = {origins: state.chromium ? ['http://*/*', 'https://*/*'] : ['<all_urls>'],
    ...(!state.chromium ? {permissions: ['webRequest', 'webRequestBlocking', 'webRequestFilterResponse']} : {})};
  const permission = enabled ? api.permissions.contains(requested).then(granted =>
    granted || api.permissions.request(requested)) : Promise.resolve(true);
  void permission.then(allowed => {
    if (!allowed) throw new Error('Site access is needed for file transfer.');
    return message({type: 'shareFiles', enabled});
  }).then(refresh, error => { $('shareFiles').checked = !enabled; showError(error); });
});

$('sharePublic').addEventListener('change', () => {
  void message({type: 'sharePublic', enabled: $('sharePublic').checked}).then(refresh, showError);
});

$('addressSuggestions').addEventListener('click', () => {
  const enabled = $('addressSuggestions').checked;
  const requested = {permissions: ['history', 'bookmarks']};
  const permission = enabled ? api.permissions.contains(requested).then(granted =>
    granted || api.permissions.request(requested)) : Promise.resolve(true);
  void permission.then(allowed => {
    if (!allowed) throw new Error('History and bookmark access is needed for address suggestions.');
    return message({type: 'addressSuggestions', enabled}).then(refresh);
  }).catch(error => { $('addressSuggestions').checked = !enabled; showError(error); });
});
