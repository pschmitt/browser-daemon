const api = globalThis.browser || chrome;
const $ = id => document.getElementById(id);
const origin = api.runtime.getURL('').replace(/\/$/, '');
// Firefox's URL.origin is the extension origin too; never use runtime.id (the
// Gecko add-on ID is different from the runtime's installation UUID).
$('origin').textContent = origin;
$('pairCommand').textContent = `browser-daemon --url https://YOUR-DAEMON --secret-file /PATH/TO/SECRET extension pair --origin ${origin}`;
let state;
let initialized = false;
let connecting = false;
let displayedTabs = [];
let draft = null;
let renderedTabs = '';
let approvalIdentity = '';
const draftReady = api.storage.session.get(['pairingDraft', 'sharingDraft']).then(saved => {
  if (saved.pairingDraft?.expires > Date.now()) $('code').value = saved.pairingDraft.code;
  else void api.storage.session.remove('pairingDraft');
  draft = saved.sharingDraft || null;
});
function saveSettings() {
  return api.storage.local.set({server: $('server').value, name: $('name').value, rememberPreference: $('rememberKey').checked});
}
for (const id of ['server', 'name', 'rememberKey']) $(id).addEventListener('input', () => void saveSettings().catch(showError));
$('code').addEventListener('input', () => {
  const code = $('code').value;
  void (code ? api.storage.session.set({pairingDraft: {code, expires: Date.now() + 300000}})
    : api.storage.session.remove('pairingDraft')).catch(showError);
});
function saveSharingDraft() {
  if (!state?.registration) return;
  const selected = new Set([...$('tabs').querySelectorAll('input:checked')].map(e => Number(e.value)));
  draft = {node: state.registration.id, control: $('control').checked,
    tabs: Object.fromEntries(displayedTabs.filter(t => selected.has(t.id)).map(t => [t.id, t.url]))};
  void api.storage.session.set({sharingDraft: draft}).catch(showError);
}
$('tabs').addEventListener('change', saveSharingDraft);
$('control').addEventListener('change', saveSharingDraft);
$('openSettings').addEventListener('click', () => void api.tabs.create({url: api.runtime.getURL('popup.html')}));
const permissionPattern = address => { const u = new URL(address); return `${u.protocol}//${u.hostname}/*`; };

async function message(value) {
  const response = await api.runtime.sendMessage(value);
  if (response?.error) throw new Error(response.error);
  return response;
}
function showError(error) { $('error').textContent = error.message; $('error').hidden = false; }
async function refresh() {
  await draftReady;
  state = await message({type: 'state'});
  $('allowlistStatus').textContent = state.allowlist.error || (state.allowlist.managed ? 'Managed by your system configuration. Only these exact origins are trusted.' : 'Only these exact origins are trusted. Add your daemon here before pairing.');
  $('daemonAllowlist').readOnly = state.allowlist.managed;
  $('saveDaemonAllowlist').hidden = state.allowlist.managed;
  if (!initialized || state.allowlist.managed) $('daemonAllowlist').value = state.allowlist.origins.join('\n');
  if (!state.allowlist.origins.length) $('allowlistPanel').open = true;
  $('remembered').hidden = !state.rememberedBrowser;
  if (state.rememberedBrowser) $('rememberedStatus').textContent = `${state.rememberedBrowser.name} is remembered. ${state.rememberedBrowser.paused ? 'Automatic reconnect is paused.' : 'Reconnects automatically after a browser restart; tab sharing requires your approval.'}`;
  $('resumeRemembered').hidden = !!state.registration;
  $('approval').hidden = !state.dashboardRequest;
  if (state.dashboardRequest && approvalIdentity !== state.dashboardRequest.server) {
    approvalIdentity = state.dashboardRequest.server;
    $('rememberKey').checked = state.dashboardRequest.remember;
  }
  if (!state.dashboardRequest) approvalIdentity = '';
  if (state.dashboardRequest) $('approvalServer').textContent = `Requested by ${state.dashboardRequest.server}. Browser name: ${$('name').value || 'My browser'}.`;
  $('status').textContent = state.registration
    ? `Connected · ${state.tabs.filter(t => t.shared).length} shared tab(s)` : state.status;
  $('connectionHistory').replaceChildren(...state.history.map(event => {
    const row = document.createElement('li');
    row.textContent = `${new Date(event.at).toLocaleTimeString()} — ${event.message}`;
    return row;
  }));
  $('diagnosticStatus').textContent = state.registration
    ? `Registered as ${state.registration.id}. ${state.tabs.filter(t => t.shared).length ? 'Your browser is listed in Nodes and its shared tabs in Sessions on the dashboard.' : 'Connected, but no tabs are shared yet. Select tabs and apply below.'}`
    : 'Not registered with the daemon. Your setup fields are saved. Open setup in a tab if a permission prompt closes this popup.';
  if (state.registration) $('dashboard').href = `${state.registration.server}/extension/`;
  $('dashboard').hidden = !state.registration;
  $('setup').hidden = !!state.registration;
  $('connected').hidden = !state.registration;
  $('companion').hidden = state.chromium;
  $('native').textContent = state.nativeReady ? 'Disable companion and stop sharing' : 'Enable installed companion';
  $('native').disabled = !state.registration;
  $('nativeStatus').textContent = state.nativeReady ? 'Companion connected — trusted input available' : 'Companion disabled — DOM input only';
  if (!initialized) {
    $('server').value = state.saved.server || '';
    $('name').value = state.saved.name || '';
    if (!state.dashboardRequest) $('rememberKey').checked = state.saved.rememberPreference === true;
    initialized = true;
  }
  if (state.registration) {
    $('code').value = '';
    $('expires').textContent = `Connected to ${state.registration.server}. ${state.registration.expires_at ? `Expires ${new Date(state.registration.expires_at * 1000).toLocaleTimeString()}.` : 'No time limit.'}`;
    $('sharingFeedback').textContent = `${state.tabs.filter(t => t.shared).length} tab(s) currently shared. Changes take effect when you click Apply.`;
    $('browserNote').textContent = state.chromium
      ? 'Chromium requires debugger permission at installation. It attaches only to shared tabs; control requires your separate grant here. Its debugging bar can stop sharing too.'
      : state.nativeReady ? 'Firefox companion connected. Trusted input and content script evaluation are available only for selected tabs. Browser chrome and other tabs remain outside the grant.'
        : 'Without the companion, Firefox asks for access to all sites to capture screenshots. Only selected tabs are sent. Control uses DOM actions; trusted key events are unavailable.';
    const signature = JSON.stringify(state.tabs);
    if (signature !== renderedTabs) {
      renderedTabs = signature;
      $('tabs').replaceChildren();
      displayedTabs = state.tabs;
      $('control').checked = draft?.node === state.registration.id ? draft.control : state.tabs.some(t => t.shared && t.control);
      for (const tab of state.tabs) {
        const label = document.createElement('label');
        label.className = 'check tab';
        const input = document.createElement('input');
        input.type = 'checkbox'; input.value = tab.id; input.checked = draft?.node === state.registration.id ? draft.tabs[tab.id] === tab.url : tab.shared;
        const text = document.createElement('span'); text.textContent = tab.title || tab.url;
        const address = document.createElement('small');
        const addressURL = new URL(tab.url); address.textContent = `${addressURL.origin}${addressURL.pathname}${addressURL.search || addressURL.hash ? '…' : ''}`;
        text.append(address); label.append(input, text); $('tabs').append(label);
      }
      if (!state.tabs.length) $('tabs').textContent = 'No eligible HTTP(S) tabs.';
    }
  } else { $('tabs').replaceChildren(); renderedTabs = ''; }
}
$('connect').addEventListener('submit', async event => {
  event.preventDefault();
  if (connecting) return;
  connecting = true; $('error').hidden = true;
  try {
    const server = NodePolicy.approvedServer($('server').value, state?.allowlist.origins || []);
    const prepared = message({type: 'prepareConnect', server, code: $('code').value.trim(), name: $('name').value, remember: $('rememberKey').checked});
    const permission = api.permissions.request({origins: [permissionPattern(server)]});
    await prepared;
    const granted = await permission;
    if (!granted) { await message({type: 'cancelConnect'}); throw new Error('Daemon connection permission was denied. Your setup fields are saved; grant access and try again.'); }
    await message({type: 'finishConnect'});
    await refresh();
  } catch (error) { showError(error); }
  finally { connecting = false; }
});
$('sharing').addEventListener('submit', async event => {
  event.preventDefault(); $('error').hidden = true;
  try {
    const tabs = [...$('tabs').querySelectorAll('input:checked')].map(e => Number(e.value));
    if (tabs.length) {
      const origins = state.chromium || state.nativeReady
        ? [...new Set(displayedTabs.filter(t => tabs.includes(t.id)).map(t => permissionPattern(t.url)))]
        : ['<all_urls>'];
      // Called directly from the submit gesture: required by permissions.request.
      const request = {origins};
      if (!(await api.permissions.request(request))) throw new Error('Tab access permission was denied.');
    }
    const urls = Object.fromEntries(displayedTabs.filter(t => tabs.includes(t.id)).map(t => [t.id, t.url]));
    await message({type: 'share', tabs, urls, control: $('control').checked});
    draft = null;
    await api.storage.session.remove('sharingDraft');
    renderedTabs = '';
    $('tabs').replaceChildren();
    await refresh();
  } catch (error) { showError(error); }
});
$('stop').addEventListener('click', async () => {
  try { await message({type: 'stop'}); $('tabs').replaceChildren(); await refresh(); }
  catch (error) { showError(error); }
});
$('native').addEventListener('click', async () => {
  $('error').hidden = true;
  try {
    if (!state.nativeReady && !(await api.permissions.request({permissions: ['nativeMessaging']}))) throw new Error('Native messaging permission was denied.');
    await message({type: 'native', enable: !state.nativeReady});
    await refresh();
  } catch (error) { showError(error); }
});
void refresh().catch(showError);
setInterval(() => void refresh().catch(showError), 1500);

$('enableDashboard').addEventListener('click', async () => {
  $('error').hidden = true;
  try {
    const server = NodePolicy.approvedServer($('server').value, state?.allowlist.origins || []);
    const configured = message({type: 'configureDashboard', server, name: $('name').value});
    const permission = api.permissions.request({origins: [permissionPattern(server)]});
    await configured;
    if (!(await permission)) throw new Error('Daemon site access was denied. Setup fields are saved.');
    await message({type: 'configureDashboard', server, name: $('name').value});
    await api.tabs.create({url: `${server}/extension/`});
  } catch (error) { showError(error); }
});
$('approvePairing').addEventListener('click', async () => {
  try {
    await message({type: 'approveDashboard', name: $('name').value.trim() || 'My browser', remember: $('rememberKey').checked});
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
    $('allowlistStatus').textContent = 'Allowlist saved.';
  } catch (error) { showError(error); }
});
