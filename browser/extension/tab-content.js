/* What a tab shows, read only: its text, its HTML, a PNG screenshot, and a
 * PDF. Chromium prints a real PDF; Firefox gives an extension no PDF bytes
 * (tabs.saveAsPDF only opens a Save dialog), so there the PDF is the whole
 * page captured as one JPEG. Used by the local tab API (local-api.js) and by
 * the daemon's commands for shared tabs (background.js). */
globalThis.tabContent = (() => {
  const ext = globalThis.browser || chrome;
  const onChromium = typeof ext.runtime.getBrowserInfo !== 'function';

  // Chromium: the debugger, attached for the call unless the daemon's sharing
  // attached it already (then its own `cdp` helper speaks to it).
  async function devtools(tabId, method, params = {}) {
    const target = {tabId};
    try {
      await ext.debugger.attach(target, '1.3');
    } catch (error) {
      if (typeof globalThis.cdp === 'function' && /already attached/i.test(error.message || '')) {
        return globalThis.cdp(tabId, method, params);
      }
      throw error;
    }
    try {
      return await ext.debugger.sendCommand(target, method, params);
    } finally {
      await ext.debugger.detach(target).catch(() => {});
    }
  }

  async function page(tabId, kind) {
    const [frame] = await ext.scripting.executeScript({
      target: {tabId},
      func: html => (html ? document.documentElement.outerHTML : document.body?.innerText ?? ''),
      args: [kind === 'html'],
    });
    return {kind, text: String(frame?.result ?? '')};
  }

  async function screenshot(tabId) {
    if (!onChromium) {
      const url = await ext.tabs.captureTab(tabId, {format: 'png'});
      return {kind: 'screenshot', format: 'png', data: url.split(',')[1]};
    }
    const tab = await ext.tabs.get(tabId);
    if (tab.active) {
      const url = await ext.tabs.captureVisibleTab(tab.windowId, {format: 'png'});
      return {kind: 'screenshot', format: 'png', data: url.split(',')[1]};
    }
    const shot = await devtools(tabId, 'Page.captureScreenshot', {format: 'png'});
    return {kind: 'screenshot', format: 'png', data: shot.data};
  }

  const ascii = text => Uint8Array.from(text, c => c.charCodeAt(0));

  // A one-page PDF around a JPEG, `width` x `height` CSS pixels at 96 dpi.
  function jpegPdf(jpeg, width, height) {
    const w = (width * 0.75).toFixed(2);
    const h = (height * 0.75).toFixed(2);
    const draw = `q ${w} 0 0 ${h} 0 0 cm /Im0 Do Q`;
    const parts = [];
    const offsets = [];
    let size = 0;
    const put = chunk => { const bytes = typeof chunk === 'string' ? ascii(chunk) : chunk; parts.push(bytes); size += bytes.length; };
    const object = (n, body) => { offsets[n] = size; put(`${n} 0 obj\n${body}\nendobj\n`); };
    put('%PDF-1.4\n');
    object(1, '<</Type/Catalog/Pages 2 0 R>>');
    object(2, '<</Type/Pages/Kids[3 0 R]/Count 1>>');
    object(3, `<</Type/Page/Parent 2 0 R/MediaBox[0 0 ${w} ${h}]/Resources<</XObject<</Im0 4 0 R>>>>/Contents 5 0 R>>`);
    offsets[4] = size;
    put(`4 0 obj\n<</Type/XObject/Subtype/Image/Width ${width}/Height ${height}/ColorSpace/DeviceRGB/BitsPerComponent 8/Filter/DCTDecode/Length ${jpeg.length}>>\nstream\n`);
    put(jpeg);
    put('\nendstream\nendobj\n');
    object(5, `<</Length ${draw.length}>>\nstream\n${draw}\nendstream`);
    const xref = size;
    put(`xref\n0 6\n0000000000 65535 f \n${offsets.slice(1).map(o => `${String(o).padStart(10, '0')} 00000 n \n`).join('')}`);
    put(`trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n${xref}\n%%EOF\n`);
    const out = new Uint8Array(size);
    let at = 0;
    for (const part of parts) { out.set(part, at); at += part.length; }
    let binary = '';
    for (let i = 0; i < out.length; i += 0x8000) binary += String.fromCharCode(...out.subarray(i, i + 0x8000));
    return btoa(binary);
  }

  // Firefox: the whole page as one JPEG (1 image pixel per CSS pixel).
  async function firefoxPdf(tabId) {
    const [frame] = await ext.scripting.executeScript({
      target: {tabId},
      func: () => [document.documentElement.scrollWidth, document.documentElement.scrollHeight],
    });
    const [width, height] = Array.isArray(frame?.result) ? frame.result.map(n => Math.max(1, Math.round(n))) : [0, 0];
    if (!width || !height) throw new Error('The page has no size to print.');
    // Firefox caps a capture at 32767 pixels a side.
    const tall = Math.min(height, 32767);
    const url = await ext.tabs.captureTab(tabId, {format: 'jpeg', quality: 92, scale: 1, rect: {x: 0, y: 0, width, height: tall}});
    const jpeg = Uint8Array.from(atob(url.split(',')[1]), c => c.charCodeAt(0));
    return {kind: 'pdf', format: 'pdf', raster: true, data: jpegPdf(jpeg, width, tall)};
  }

  async function pdf(tabId) {
    if (!onChromium) return firefoxPdf(tabId);
    const printed = await devtools(tabId, 'Page.printToPDF', {printBackground: true});
    return {kind: 'pdf', format: 'pdf', data: printed.data};
  }

  async function get(tabId, kind) {
    if (kind === 'text' || kind === 'html') return page(tabId, kind);
    if (kind === 'screenshot') return screenshot(tabId);
    if (kind === 'pdf') return pdf(tabId);
    throw new Error(`Unknown content: ${kind}`);
  }

  return {get};
})();
