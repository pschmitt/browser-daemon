/* Runs in each shared page's isolated extension world. No microphone, desktop,
 * or other tab is captured. A short lease stops capture if the worker dies. */
function captureMediaAudio(token, stopToken) {
  const previous = globalThis.__bdMediaAudio;
  if (previous?.token === token && token) { previous.until = Date.now() + 20000; return; }
  if (!token && previous?.token !== stopToken) return;
  previous?.stop();
  if (!token) return;
  const api = globalThis.browser || chrome;
  const context = new AudioContext({sampleRate: 48000});
  const mix = context.createGain();
  const processor = context.createScriptProcessor(4096, 2, 2);
  const silence = context.createGain();
  silence.gain.value = 0;
  mix.connect(processor); processor.connect(silence); silence.connect(context.destination);
  const sources = new Map();
  const state = {token, until: Date.now() + 20000, stopped: false, pending: false};
  globalThis.__bdMediaAudio = state;
  function release(element) {
    const source = sources.get(element);
    if (!source) return;
    source.node?.disconnect(); source.gain.disconnect();
    source.stream.getTracks().forEach(track => track.stop());
    sources.delete(element);
  }
  state.stop = () => {
    if (state.stopped) return;
    state.stopped = true;
    clearInterval(timer);
    removeEventListener('pagehide', state.stop);
    for (const element of sources.keys()) release(element);
    processor.onaudioprocess = null;
    processor.disconnect(); mix.disconnect(); silence.disconnect();
    void context.close().catch(() => {});
    if (globalThis.__bdMediaAudio === state) delete globalThis.__bdMediaAudio;
  };
  let roots = [document], scans = 0;
  const scan = () => {
    if (Date.now() > state.until) { state.stop(); return; }
    for (const element of sources.keys()) if (!element.isConnected || element.ended) release(element);
    // Include media inside open shadow roots, without changing the page's audio graph.
    if (scans++ % 5 === 0) {
      roots = [document];
      for (let i = 0; i < roots.length && roots.length < 64; i++) {
        for (const node of roots[i].querySelectorAll('*')) if (node.shadowRoot && roots.length < 64) roots.push(node.shadowRoot);
      }
    }
    for (const root of roots) {
      for (const element of root.querySelectorAll('audio,video')) {
        if (!sources.has(element) && !element.paused && element.readyState >= 2) {
          try {
            const capture = element.captureStream || element.mozCaptureStream;
            if (!capture) continue;
            const stream = capture.call(element);
            // We relay only sound; release captured video tracks immediately.
            stream.getVideoTracks().forEach(track => track.stop());
            const gain = context.createGain(); gain.connect(mix);
            sources.set(element, {stream, gain, node: null});
          } catch { /* Protected media and unavailable capture are left alone. */ }
        }
        const source = sources.get(element);
        if (!source) continue;
        const tracks = source.stream.getAudioTracks().filter(track => track.readyState === 'live');
        if (source.node && !tracks.length) { release(element); continue; }
        const trackKey = tracks.map(track => track.id).join(',');
        if (tracks.length && source.trackKey !== trackKey) {
          source.node?.disconnect();
          source.trackKey = trackKey;
          source.node = context.createMediaStreamSource(new MediaStream(tracks));
          source.node.connect(source.gain);
        }
        source.gain.gain.value = element.muted || element.paused ? 0 : element.volume;
      }
    }
    if (context.state === 'suspended') void context.resume().catch(() => {});
  };
  processor.onaudioprocess = event => {
    if (state.stopped || state.pending || !sources.size) return;
    const input = event.inputBuffer;
    const left = input.getChannelData(0), right = input.getChannelData(1);
    const bytes = new Uint8Array(left.length * 4);
    const view = new DataView(bytes.buffer);
    for (let i = 0; i < left.length; i++) {
      view.setInt16(i * 4, Math.round(Math.max(-1, Math.min(1, left[i])) * 32767), true);
      view.setInt16(i * 4 + 2, Math.round(Math.max(-1, Math.min(1, right[i])) * 32767), true);
    }
    if (!bytes.some(byte => byte !== 0)) return;
    state.pending = true;
    void api.runtime.sendMessage({type: 'mediaAudio', token, rate: context.sampleRate,
      data: btoa(String.fromCharCode(...bytes))}).then(result => {
      if (!result?.ok) state.stop();
    }, () => state.stop()).finally(() => { state.pending = false; });
  };
  const timer = setInterval(scan, 1000);
  addEventListener('pagehide', state.stop, {once: true});
  scan();
}
