import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';

const html = await readFile(new URL('../handoff.html', import.meta.url), 'utf8');
const source = html.match(/async function closeMultipleTabs\(tab, direction\) \{[\s\S]*?\n\}\nfor \(const \[id, direction\]/)?.[0].replace(/\nfor \(const \[id, direction\]$/, '');
assert.ok(source, 'bulk tab handler exists');

async function close(direction, shared = false, confirm = true) {
  const closed = [];
  const switched = [];
  const pages = [
    { id: '1', pinned: true, control: true },
    { id: '2', control: true },
    { id: '3', control: true },
    { id: '4', control: false },
    { id: '5', control: true },
  ];
  const context = {
    lastStripPages: pages, sharedBrowserId: shared ? 'browser' : null,
    sharedBase: '/api', pageId: '2', encodeURIComponent, JSON, Number, String,
    ask: async () => confirm, authHeaders: () => ({}), noteClosedTab: () => {},
    fetch: async (url, opts) => { closed.push(shared ? String(JSON.parse(opts.body).tab) : url.split('/').at(-1)); return { ok: true }; },
    switchTab: async id => switched.push(id), loadTabs: async () => {}, toast: () => {},
  };
  await runInNewContext(`${source}; closeMultipleTabs(lastStripPages[2], ${JSON.stringify(direction)})`, context);
  return { closed, switched };
}

test('closes tabs in strip order while preserving the chosen and pinned tabs', async () => {
  assert.deepEqual(await close('before'), { closed: ['2'], switched: ['3'] });
  assert.deepEqual(await close('after'), { closed: ['4', '5'], switched: [] });
  assert.deepEqual(await close('others'), { closed: ['2', '4', '5'], switched: ['3'] });
});

test('leaves uncontrolled shared tabs and respects cancellation', async () => {
  assert.deepEqual(await close('others', true), { closed: ['2', '5'], switched: ['3'] });
  assert.deepEqual(await close('others', false, false), { closed: [], switched: [] });
});
