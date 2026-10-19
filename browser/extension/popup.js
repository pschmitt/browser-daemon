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
const permissionPattern = address => { const u = new URL(address); return `${u.protocol}//${u.hostname}/*`; };

async function message(value) {
  const response = await api.runtime.sendMessage(value);
  if (response?.error) throw new Error(response.error);
  return response;
}
function showError(error) { $('error').textContent = error.message; $('error').hidden = false; }
async function refresh() {
  state = await message({type: 'state'});
  $('status').textContent = state.status;
  $('setup').hidden = !!state.registration;
  $('connected').hidden = !state.registration;
  $('companion').hidden = state.chromium;
  $('native').textContent = state.nativeReady ? 'Disable companion and stop sharing' : 'Enable installed companion';
  $('native').disabled = !state.registration;
  $('nativeStatus').textContent = state.nativeReady ? 'Companion connected — trusted input available' : 'Companion disabled — DOM input only';
  if (!initialized) {
    $('server').value = state.saved.server || '';
    $('name').value = state.saved.name || '';
    initialized = true;
  }
  if (state.registration) {
    $('expires').textContent = `Connected to ${state.registration.server}. Expires ${new Date(state.registration.expires_at * 1000).toLocaleTimeString()}.`;
    $('browserNote').textContent = state.chromium
      ? 'Chromium requires debugger permission at installation. It attaches only to shared tabs; control requires your separate grant here. Its debugging bar can stop sharing too.'
      : state.nativeReady ? 'Firefox companion connected. Trusted input and content script evaluation are available only for selected tabs. Browser chrome and other tabs remain outside the grant.'
        : 'Without the companion, Firefox asks for access to all sites to capture screenshots. Only selected tabs are sent. Control uses DOM actions; trusted key events are unavailable.';
    if (!$('tabs').childElementCount) {
      displayedTabs = state.tabs;
      $('control').checked = state.tabs.some(t => t.shared && t.control);
      for (const tab of state.tabs) {
        const label = document.createElement('label');
        label.className = 'check tab';
        const input = document.createElement('input');
        input.type = 'checkbox'; input.value = tab.id; input.checked = tab.shared;
        const text = document.createElement('span'); text.textContent = tab.title || tab.url;
        const address = document.createElement('small'); address.textContent = tab.url;
        text.append(address); label.append(input, text); $('tabs').append(label);
      }
      if (!state.tabs.length) $('tabs').textContent = 'No eligible HTTP(S) tabs.';
    }
  } else $('tabs').replaceChildren();
}
$('connect').addEventListener('submit', async event => {
  event.preventDefault();
  if (connecting) return;
  connecting = true; $('error').hidden = true;
  try {
    const server = NodePolicy.serverURL($('server').value);
    const granted = await api.permissions.request({origins: [permissionPattern(server)]});
    if (!granted) throw new Error('Daemon connection permission was denied.');
    await message({type: 'connect', server, code: $('code').value.trim(), name: $('name').value});
    $('code').value = '';
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
