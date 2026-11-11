/* Hides what a screenshot of a shared tab must not show: black boxes over the
 * matching elements, drawn in the page itself just before the capture. The
 * pure parts (plan, selectors, hints, secret and email matching) run in the
 * worker too, where they are tested; apply() and remove() run in the page, in
 * every frame the extension can script. Page-side state lives in this script's
 * own world, out of reach of the page's code. */
(() => {
  const CATEGORIES = ['passwords', 'cards', 'otp', 'inputs', 'secrets', 'emails'];
  const CARD_HINT = /(card|cc)[-_ ]?(number|num|no|csc|cvc|cvv|exp\w*)|cardnumber|credit[-_ ]?card|cvc|cvv|csc|expir|exp[-_ ]?(date|month|year)|security[-_ ]?code/i;
  const OTP_HINT = /(^|[^a-z])code([^a-z]|$)|otp|totp|2fa|mfa/i;
  const EMAIL = /[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}/;
  const MIN_SECRET = 4, MAX_SECRET = 1024, MAX_SECRETS = 64;
  const TEXT_FIELDS = 'textarea, [contenteditable=""], [contenteditable=true], input:not([type]), input[type=text], input[type=search], input[type=email], input[type=tel], input[type=url], input[type=number], input[type=password]';

  // The categories a request asks for, and the secrets usable for matching
  // (only strings of 4 or more characters, so a short value cannot black out
  // a page). null: nothing to censor. Anything unknown is an error: a typo
  // must not silently censor less.
  function plan(categories, secrets) {
    if (categories === undefined || categories === null || (Array.isArray(categories) && !categories.length)) return null;
    if (!Array.isArray(categories) || categories.length > CATEGORIES.length * 2) throw new Error('Unknown censor category.');
    const wanted = new Set();
    for (const category of categories) {
      if (!CATEGORIES.includes(category)) throw new Error('Unknown censor category.');
      wanted.add(category);
    }
    const usable = Array.isArray(secrets)
      ? [...new Set(secrets.filter(s => typeof s === 'string' && s.length >= MIN_SECRET && s.length <= MAX_SECRET))].slice(0, MAX_SECRETS) : [];
    return {categories: CATEGORIES.filter(c => wanted.has(c)), secrets: usable};
  }
  // CSS selectors that match elements outright; the rest needs the hints below.
  function selectors(categories) {
    const css = [];
    if (categories.includes('passwords')) css.push('input[type=password]');
    if (categories.includes('cards')) css.push('[autocomplete^=cc-]');
    if (categories.includes('otp')) css.push('[autocomplete=one-time-code]');
    return css;
  }
  const hintOf = el => [el.name, el.id, el.placeholder, el.getAttribute?.('aria-label')].filter(Boolean).join(' ');
  const cardHint = text => CARD_HINT.test(text || '');
  const otpHint = text => OTP_HINT.test(text || '');
  const hasEmail = text => EMAIL.test(text || '');
  const hasSecret = (text, secrets) => typeof text === 'string' && text !== '' && secrets.some(s => text.includes(s));

  let state = null;
  function remove() {
    if (!state) return false;
    clearTimeout(state.timer);
    try { state.host.hidePopover?.(); } catch { /* Not shown as a popover. */ }
    state.host.remove();
    state = null;
    return true;
  }
  // Draws the boxes in this frame's own viewport (so every frame masks itself,
  // whatever its origin) and waits until they have been painted. Returns how many.
  async function apply({categories, secrets}) {
    remove();
    const want = new Set(categories);
    const css = selectors(categories).join(',');
    const boxes = [];
    const add = rect => {
      if (rect.width > 0 && rect.height > 0 && rect.right > 0 && rect.bottom > 0 && rect.left < innerWidth && rect.top < innerHeight) boxes.push(rect);
    };
    // Open shadow roots are searched too; closed ones cannot be.
    const roots = [document];
    const elements = [];
    for (let i = 0; i < roots.length; i++) {
      for (const el of roots[i].querySelectorAll('*')) {
        elements.push(el);
        if (el.shadowRoot) roots.push(el.shadowRoot);
      }
    }
    for (const el of elements) {
      const tag = el.tagName;
      const field = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT';
      const value = field ? el.value : '';
      const editable = !field && el.matches(TEXT_FIELDS);
      const textLike = (field && el.matches(TEXT_FIELDS)) || editable;
      const shown = editable ? el.innerText : value;
      const hint = field ? hintOf(el) : '';
      let hit = css !== '' && el.matches(css);
      if (!hit && want.has('cards') && ((field && textLike || tag === 'SELECT') && cardHint(hint))) hit = true;
      if (!hit && want.has('otp') && field && textLike && otpHint(hint)) hit = true;
      if (!hit && want.has('inputs') && textLike && shown !== '') hit = true;
      if (!hit && want.has('secrets') && secrets.length && (field ? hasSecret(value, secrets) : editable && hasSecret(shown, secrets))) hit = true;
      if (!hit && want.has('emails') && textLike && hasEmail(shown)) hit = true;
      // A frame of another origin cannot be read: its whole box goes.
      if (!hit && want.has('cards') && (tag === 'IFRAME' || tag === 'FRAME')) {
        let readable = false;
        try { readable = !!el.contentDocument; } catch { /* Cross-origin. */ }
        hit = !readable;
      }
      if (hit) add(el.getBoundingClientRect());
    }
    // Text: the boxes of the matching text nodes.
    if ((want.has('emails') || (want.has('secrets') && secrets.length))) {
      for (const root of roots) {
        const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
        for (let node = walker.nextNode(); node; node = walker.nextNode()) {
          const text = node.data;
          if (!text || !node.parentElement || /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(node.parentElement.tagName)) continue;
          if (!((want.has('emails') && hasEmail(text)) || (want.has('secrets') && hasSecret(text, secrets)))) continue;
          const range = document.createRange();
          range.selectNodeContents(node);
          for (const rect of range.getClientRects()) add(rect);
        }
      }
    }
    if (!boxes.length) return {n: 0};
    // Opaque black, on an anonymous element in a closed shadow root, above everything
    // the page can stack (a modal dialog's top layer included).
    const host = document.createElement('div');
    const shadow = host.attachShadow({mode: 'closed'});
    const style = (el, props) => { for (const [k, v] of Object.entries(props)) el.style.setProperty(k, v, 'important'); };
    style(host, {all: 'initial', position: 'fixed', inset: '0', width: '100vw', height: '100vh', margin: '0', padding: '0', border: '0',
      background: 'transparent', overflow: 'visible', display: 'block', 'z-index': '2147483647', 'pointer-events': 'none'});
    for (const rect of boxes) {
      const box = document.createElement('div');
      style(box, {position: 'fixed', left: `${Math.floor(rect.left) - 2}px`, top: `${Math.floor(rect.top) - 2}px`,
        width: `${Math.ceil(rect.width) + 4}px`, height: `${Math.ceil(rect.height) + 4}px`, background: '#000', 'pointer-events': 'none'});
      shadow.append(box);
    }
    document.documentElement.append(host);
    try { host.setAttribute('popover', 'manual'); host.showPopover(); } catch { /* Fixed position only. */ }
    state = {host, timer: setTimeout(remove, 15000)};
    // Painted: two frames. A hidden tab never gets them, so do not wait for one.
    await Promise.race([new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done))), new Promise(done => setTimeout(done, 300))]);
    return {n: boxes.length};
  }
  globalThis.NodeCensor = {plan, selectors, cardHint, otpHint, hasEmail, hasSecret, apply, remove, CATEGORIES};
})();
