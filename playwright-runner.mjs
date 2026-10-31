// Runs one request of `POST /api/run` (see docs/run.md): connects Playwright to a
// session of this node over CDP, runs a script or a list of steps in a tab and
// prints one JSON line with the outcome. The daemon starts it with Node's
// permission model (no child processes, no writes outside its temp directory),
// passes the request on stdin and the token in BD_RUN_TOKEN, and kills it at the
// deadline. Nothing of the request (script, steps, args) is printed or logged.

import { readFileSync } from 'node:fs';

const LOG_LIMIT = 200;
const LOG_TEXT_LIMIT = 2000;

const request = JSON.parse(readFileSync(0, 'utf8'));
const { chromium } = await import(process.env.BD_PLAYWRIGHT_CORE + '/index.mjs');

const logs = [];
function log(type, text) {
  if (logs.length < LOG_LIMIT) logs.push({ type, text: String(text).slice(0, LOG_TEXT_LIMIT) });
}

function emit(out) {
  process.stdout.write(JSON.stringify(out) + '\n');
}

/** A step's value: `value`, or the request's `args[value_arg]` (kept out of the steps). */
function value(s) {
  return s.value_arg !== undefined ? request.args?.[s.value_arg] : s.value;
}

/** A step's target: a Playwright selector, or the page itself when none is given. */
function locator(page, step) {
  if (!step.selector) throw new Error(`${step.action}: selector is required`);
  const loc = page.locator(step.selector);
  return step.nth === undefined ? loc.first() : loc.nth(step.nth);
}

const actions = {
  goto: (page, s) => page.goto(s.url, { waitUntil: s.wait_until || 'load', timeout: s.timeout }).then(() => page.url()),
  reload: (page, s) => page.reload({ waitUntil: s.wait_until || 'load', timeout: s.timeout }).then(() => page.url()),
  back: (page, s) => page.goBack({ waitUntil: s.wait_until || 'load', timeout: s.timeout }).then(() => page.url()),
  click: (page, s) => locator(page, s).click({ timeout: s.timeout, button: s.button, clickCount: s.count }),
  dblclick: (page, s) => locator(page, s).dblclick({ timeout: s.timeout }),
  hover: (page, s) => locator(page, s).hover({ timeout: s.timeout }),
  fill: (page, s) => locator(page, s).fill(String(value(s) ?? ''), { timeout: s.timeout }),
  type: (page, s) => locator(page, s).pressSequentially(String(value(s) ?? ''), { delay: s.delay, timeout: s.timeout }),
  press: (page, s) => (s.selector ? locator(page, s).press(s.key, { timeout: s.timeout }) : page.keyboard.press(s.key)),
  check: (page, s) => locator(page, s).setChecked(s.checked ?? true, { timeout: s.timeout }),
  select: (page, s) => locator(page, s).selectOption(value(s), { timeout: s.timeout }),
  wait_for: async (page, s) => {
    if (s.selector) return locator(page, s).waitFor({ state: s.state || 'visible', timeout: s.timeout });
    if (s.url) return page.waitForURL(s.url.startsWith('/') && s.url.endsWith('/') && s.url.length > 1
      ? new RegExp(s.url.slice(1, -1)) : s.url, { timeout: s.timeout });
    if (s.load_state) return page.waitForLoadState(s.load_state, { timeout: s.timeout });
    if (s.ms !== undefined) return page.waitForTimeout(Math.min(Number(s.ms), 60_000));
    throw new Error('wait_for: selector, url, load_state or ms is required');
  },
  text: (page, s) => (s.all
    ? page.locator(s.selector).allInnerTexts()
    : locator(page, s).innerText({ timeout: s.timeout })),
  html: (page, s) => (s.selector ? locator(page, s).innerHTML({ timeout: s.timeout }) : page.content()),
  value: (page, s) => locator(page, s).inputValue({ timeout: s.timeout }),
  attribute: (page, s) => locator(page, s).getAttribute(s.name, { timeout: s.timeout }),
  count: (page, s) => page.locator(s.selector).count(),
  visible: (page, s) => locator(page, s).isVisible(),
  url: page => page.url(),
  title: page => page.title(),
  evaluate: (page, s) => page.evaluate(
    ({ expression, arg }) => {
      // eslint-disable-next-line no-new-func
      const value = (0, eval)(expression);
      return typeof value === 'function' ? value(arg) : value;
    },
    { expression: s.expression, arg: s.arg },
  ),
  screenshot: (page, s) => (s.selector ? locator(page, s).screenshot({ timeout: s.timeout })
    : page.screenshot({ fullPage: !!s.full_page, timeout: s.timeout }))
    .then(buf => buf.toString('base64')),
};

async function runSteps(page, steps) {
  const results = {};
  const done = [];
  for (const [index, step] of steps.entries()) {
    const fn = actions[step.action];
    if (!fn) throw Object.assign(new Error(`step ${index}: unknown action '${step.action}'`), { done });
    const started = Date.now();
    try {
      const value = await fn(page, step);
      if (step.as) results[step.as] = value ?? null;
      done.push({ action: step.action, ms: Date.now() - started });
    } catch (err) {
      if (step.optional) {
        done.push({ action: step.action, ms: Date.now() - started, skipped: true });
        if (step.as) results[step.as] = null;
        continue;
      }
      err.message = `step ${index} (${step.action}): ${err.message}`;
      err.done = done;
      throw err;
    }
  }
  return { result: results, steps: done };
}

async function runScript(ctx, source) {
  // The script is a function expression: `async ({ page, context, args, log }) => ...`.
  const fn = (0, eval)(`(${source})`);
  if (typeof fn !== 'function') throw new Error('script must be a function expression');
  return { result: (await fn(ctx)) ?? null };
}

/** The page with the CDP target id `id` (the daemon's page id), if Playwright knows it. */
async function pageById(context, id) {
  for (const page of context.pages()) {
    const cdp = await context.newCDPSession(page);
    try {
      const { targetInfo } = await cdp.send('Target.getTargetInfo');
      if (targetInfo.targetId === id) return page;
    } finally {
      await cdp.detach().catch(() => {});
    }
  }
  return null;
}

/** JSON can only carry plain data: buffers become base64, the rest goes through JSON. */
function plain(value) {
  return JSON.parse(JSON.stringify(value ?? null, (_k, v) => {
    if (v && v.type === 'Buffer' && Array.isArray(v.data)) return Buffer.from(v.data).toString('base64');
    return v;
  }));
}

let browser;
let page;
let ownPage = false;
const started = Date.now();
const out = { ok: false };
const deadline = setTimeout(() => {
  out.error = `timed out after ${request.timeout_ms} ms`;
  out.timed_out = true;
  finish();
}, request.timeout_ms);

let finished = false;
async function finish() {
  if (finished) return;
  finished = true;
  clearTimeout(deadline);
  if (page && !page.isClosed()) {
    out.url = page.url();
    if (!out.ok && request.screenshot_on_error !== false) {
      out.screenshot = await page.screenshot({ timeout: 5000 }).then(b => b.toString('base64')).catch(() => undefined);
    }
    if (ownPage && !request.keep_open) await page.close().catch(() => {});
  }
  out.logs = logs;
  out.ms = Date.now() - started;
  emit(out);
  // Disconnects; the session itself stays (a throwaway one ends with its last client).
  await Promise.race([browser?.close().catch(() => {}), new Promise(r => setTimeout(r, 3000))]);
  process.exit(0);
}

try {
  const headers = process.env.BD_RUN_TOKEN ? { authorization: `Bearer ${process.env.BD_RUN_TOKEN}` } : {};
  // The script runs in this process: it gets the browser, not the token.
  delete process.env.BD_RUN_TOKEN;
  browser = await chromium.connectOverCDP(request.endpoint, { headers, timeout: 30_000 });
  const context = browser.contexts()[0] || await browser.newContext();
  if (request.tab && request.tab !== 'new') {
    page = await pageById(context, request.tab);
    if (!page) throw new Error(`tab ${request.tab} not found in the session`);
  } else {
    page = await context.newPage();
    ownPage = true;
  }
  page.setDefaultTimeout(request.action_timeout_ms);
  page.on('console', msg => log(msg.type(), msg.text()));
  page.on('pageerror', err => log('pageerror', err.message));
  if (request.url) await page.goto(request.url, { waitUntil: 'load' });

  const ran = request.steps
    ? await runSteps(page, request.steps)
    : await runScript({ page, context, browser, args: request.args ?? {}, log: text => log('script', text) }, request.script);
  out.result = plain(ran.result);
  if (ran.steps) out.steps = ran.steps;
  out.ok = true;
  if (request.keep_open) out.page = await pageTargetId(context, page).catch(() => null);
} catch (err) {
  out.error = String(err?.message || err).split('\n')[0];
  if (err?.done) out.steps = err.done;
}
await finish();

async function pageTargetId(context, p) {
  const cdp = await context.newCDPSession(p);
  try {
    return (await cdp.send('Target.getTargetInfo')).targetInfo.targetId;
  } finally {
    await cdp.detach().catch(() => {});
  }
}
