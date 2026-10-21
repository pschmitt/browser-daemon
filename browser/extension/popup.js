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
let busy = false;
let renderedTabs = '';
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
$('openSettings').addEventListener('click', () => void api.tabs.create({url: api.runtime.getURL('popup.html?tab')}));
const permissionPattern = address => { const u = new URL(address); return `${u.protocol}//${u.hostname}/*`; };

async function message(value) {
  const response = await api.runtime.sendMessage(value);
  if (response?.error) throw new Error(response.error);
  return response;
}
function showError(error) { $('error').textContent = error.message; $('error').hidden = false; }
function clearError() { $('error').hidden = true; }
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

function renderHeader() {
  const reg = state.registration;
  $('title').textContent = reg ? (state.saved.name || 'Browser node') : 'Browser node';
  // The version where the eye looks first; the daemon gets room in the main card.
  $('subtitle').textContent = `v${state.version}${state.nativeReady ? ' · companion' : ''}`;
  $('subtitle').title = reg ? '' : state.status;
  $('pill').textContent = reg ? 'Connected' : state.connecting ? 'Connecting' : 'Offline';
  $('pill').className = `pill${reg ? ' on' : state.connecting ? ' wait' : ''}`;
  $('pill').disabled = !reg;
  $('pill').title = reg ? 'Disconnect this browser' : state.connecting ? 'Connecting' : 'Offline';
}

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
  if (state.rememberedBrowser) {
    $('rememberedStatus').textContent = state.rememberedBrowser.paused
      ? `${state.rememberedBrowser.name} was disconnected. Reconnect to ${new URL(state.rememberedBrowser.server).host}?`
      : `Reconnecting ${state.rememberedBrowser.name} to ${new URL(state.rememberedBrowser.server).host}…`;
  }
  $('forgetRemembered').hidden = !state.rememberedBrowser;

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
  $('allowlistStatus').textContent = state.allowlist.error || (state.allowlist.managed
    ? 'Set by your system configuration.' : 'Exact origins, one per line. Nothing else is trusted.');
  $('daemonAllowlist').readOnly = state.allowlist.managed;
  $('saveDaemonAllowlist').hidden = state.allowlist.managed;
  if (document.activeElement !== $('daemonAllowlist')) $('daemonAllowlist').value = origins.join('\n');
  if (!origins.length && !state.allowlist.managed) $('advanced').open = true;
  $('companion').hidden = state.chromium;
  $('native').textContent = state.nativeReady ? 'Disable companion' : 'Enable companion';
  $('native').disabled = !reg;
  $('nativeStatus').textContent = state.nativeReady ? 'Connected' : '';
  $('diagnosticStatus').textContent = reg
    ? `Node ${reg.id}${reg.expires_at ? `, until ${new Date(reg.expires_at * 1000).toLocaleTimeString()}` : ', no time limit'}.` : state.status;
  $('connectionHistory').replaceChildren(...state.history.slice().reverse().map(event => {
    const row = document.createElement('li');
    row.textContent = `${new Date(event.at).toLocaleTimeString()} ${event.message}`;
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
  if (!state.registration) return;
  try { await message({type: 'stop'}); await refresh(); }
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
$('enableDashboard').addEventListener('click', async () => {
  clearError();
  try {
    const address = NodePolicy.approvedServer(server(), state?.allowlist.origins || []);
    const name = $('name').value || state.defaultName;
    const configured = message({type: 'configureDashboard', server: address, name});
    // Every allowed daemon at once: any of their dashboards can pair afterwards.
    const origins = [...new Set([address, ...state.allowlist.origins].map(permissionPattern))];
    const permission = api.permissions.request({origins});
    await configured;
    if (!(await permission)) throw new Error('Allow access to your daemons to pair from their dashboards.');
    await message({type: 'configureDashboard', server: address, name});
    await api.tabs.create({url: `${address}/#nodes/add`});
  } catch (error) { showError(error); }
});
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
$('saveDaemonAllowlist').addEventListener('click', async () => {
  try {
    const origins = $('daemonAllowlist').value.split('\n').map(v => v.trim()).filter(Boolean);
    await message({type: 'setDaemonAllowlist', origins});
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
