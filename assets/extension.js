/* Temporary nodes are displayed apart from trusted daemons. They cannot spawn
   sessions, accept peer forwarding, or change cluster configuration. */
const $ = id => document.getElementById(id);
let selected = null;
let imaging = false;
let polling = false;
let nodeReports = [];
let wheelBusy = false;

async function request(method, path, body) {
  const response = await fetch(path, {method, headers: {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body), cache: 'no-store'});
  if (!response.ok) throw new Error(`Request failed (${response.status}). The browser may have disconnected or stopped sharing.`);
  const result = await response.json();
  if (result.error) throw new Error(result.error);
  return result;
}
function error(e) { $('status').textContent = e.message; }
function button(text, action) {
  const b = document.createElement('button'); b.textContent = text;
  b.addEventListener('click', () => void action().catch(error));
  return b;
}
function base(daemon) { return daemon ? `/api/peers/${encodeURIComponent(daemon)}` : '/api'; }
async function refresh() {
  if (polling) return;
  polling = true;
  try {
    const mesh = await request('GET', '/api/mesh/overview');
    const daemons = ['', ...mesh.peers.filter(p => p.status === 'online').map(p => p.name)];
    const results = await Promise.allSettled(daemons.map(async daemon => {
      const response = await request('GET', `${base(daemon)}/extension/nodes`);
      return response.nodes.map(node => ({...node, daemon}));
    }));
    nodeReports = results.flatMap(result => result.status === 'fulfilled' ? result.value : []);
    const unavailable = results.filter(r => r.status === 'rejected').length;
    $('status').textContent = `${nodeReports.length} temporary browser(s) connected.${unavailable ? ` ${unavailable} daemon(s) could not report extension support.` : ''}`;
    const fragment = document.createDocumentFragment();
    for (const node of nodeReports) {
      const section = document.createElement('section');
      const heading = document.createElement('h2'); heading.textContent = node.name;
      const detail = document.createElement('p');
      detail.textContent = `${node.browser}${node.native_control ? ' + native control' : ''} via ${node.daemon || mesh.local_node.name} · expires ${new Date(node.expires_at * 1000).toLocaleTimeString()} · ${node.tabs.length} shared tab(s)`;
      section.append(heading, detail, button('Disconnect browser', async () => {
        await request('DELETE', `${base(node.daemon)}/extension/nodes/${node.id}`);
        if (selected?.node.id === node.id) leave();
        await refresh();
      }));
      const list = document.createElement('ul');
      for (const tab of node.tabs) {
        const li = document.createElement('li');
        li.append(button(`${tab.title || tab.url} — ${tab.control ? 'control' : 'view only'}`, async () => open(node, tab)));
        const url = document.createElement('small'); url.textContent = ` ${tab.url}`;
        li.append(url); list.append(li);
      }
      section.append(list); fragment.append(section);
    }
    $('nodes').replaceChildren(fragment);
    if (selected) {
      const tab = nodeReports.find(n => n.id === selected.node.id && n.daemon === selected.node.daemon)?.tabs.find(t => t.id === selected.tab.id);
      if (!tab) { leave(); $('status').textContent = 'The browser disconnected or stopped sharing this tab.'; }
      else { selected.tab = tab; setControl(tab.control); }
    }
  } catch (e) { error(e); }
  finally { polling = false; }
}
function setControl(control) {
  $('access').textContent = control ? 'Control allowed by the browser owner' : 'View only';
  for (const id of ['back', 'forward', 'reload', 'focus', 'navigate', 'type', 'enter', 'tab', 'address', 'text']) $(id).disabled = !control;
}
function leave() {
  selected = null; $('viewer').hidden = true; $('screen').removeAttribute('src');
  $('output').textContent = ''; $('text').value = '';
}
async function open(node, tab) {
  selected = {node, tab}; $('viewer').hidden = false;
  $('title').textContent = tab.title || tab.url; $('address').value = tab.url;
  $('output').textContent = ''; setControl(tab.control);
  await screenshot();
}
async function command(action, params = {}) {
  const current = selected;
  if (!current) throw new Error('Select a shared tab.');
  const response = await request('POST', `${base(current.node.daemon)}/extension/nodes/${current.node.id}/command`, {tab: current.tab.id, action, params});
  return response.result;
}
async function screenshot() {
  if (!selected || imaging || document.hidden) return;
  imaging = true;
  const current = selected;
  try {
    const result = await command('screenshot');
    if (selected === current && typeof result?.data === 'string' && ['jpeg', 'png'].includes(result.format)) {
      selected.viewport = {width: result.width, height: result.height};
      $('screen').src = `data:image/${result.format};base64,${result.data}`;
    }
  } catch (e) { error(e); }
  finally { imaging = false; }
}
for (const action of ['back', 'forward', 'reload', 'focus']) $(action).addEventListener('click', () => void command(action).then(screenshot).catch(error));
$('navigate').addEventListener('click', () => void command('navigate', {url: $('address').value}).catch(error));
$('type').addEventListener('click', () => {
  const text = $('text').value; $('text').value = '';
  void command('type', {text}).then(screenshot).catch(error);
});
$('enter').addEventListener('click', () => void command('key', {key: 'Enter'}).then(screenshot).catch(error));
$('tab').addEventListener('click', () => void command('key', {key: 'Tab'}).then(screenshot).catch(error));
$('snapshot').addEventListener('click', () => void command('snapshot').then(result => { $('output').textContent = JSON.stringify(result, null, 2); }).catch(error));
$('leave').addEventListener('click', leave);
$('screen').addEventListener('click', event => {
  if (!selected?.tab.control) return;
  const screen = $('screen'); const rect = screen.getBoundingClientRect();
  void command('click', {x: (event.clientX - rect.left) * (selected.viewport?.width || screen.naturalWidth) / rect.width,
    y: (event.clientY - rect.top) * (selected.viewport?.height || screen.naturalHeight) / rect.height}).then(screenshot).catch(error);
});
$('screen').addEventListener('wheel', event => {
  if (!selected?.tab.control) return;
  event.preventDefault();
  if (wheelBusy) return;
  wheelBusy = true;
  void command('scroll', {x: event.deltaX, y: event.deltaY}).then(screenshot).catch(error).finally(() => { wheelBusy = false; });
}, {passive: false});
$('screen').addEventListener('keydown', event => {
  if (!selected?.tab.control || event.ctrlKey || event.metaKey || event.altKey) return;
  if (event.key.length === 1) {
    event.preventDefault(); void command('type', {text: event.key}).then(screenshot).catch(error);
  } else if (['Enter', 'Tab', 'Escape', 'Backspace', 'Delete', 'ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) {
    event.preventDefault(); void command('key', {key: event.key}).then(screenshot).catch(error);
  }
});
void refresh();
setInterval(() => { if (!document.hidden) void refresh(); }, 3000);
setInterval(() => void screenshot(), 1200);
