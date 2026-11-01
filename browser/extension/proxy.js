/* Routing this browser through the daemon's HTTP proxy. The pure parts: what the
   daemon may offer, which hosts stay direct, and the login. Shared with the
   background worker and the dependency-free checks. */
globalThis.NodeProxy = (() => {
  // The choice that sends traffic out of the daemon node's own network.
  const DIRECT = 'direct';
  // Never sent through a proxy: this machine and every network it is on, where
  // the proxy's own view would be the wrong one (and the daemon refuses them).
  const NETWORKS = ['127.0.0.0/8', '10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16', '100.64.0.0/10', '169.254.0.0/16'];
  const text = (value, max) => typeof value === 'string' && value.length > 0 && value.length <= max && !/[\u0000-\u001f\u007f]/.test(value);
  const bare = host => host.replace(/^\[|\]$/g, '').toLowerCase().replace(/\.$/, '');

  // `[{id, name}]`, what the daemon lets this browser pick from.
  function choices(value) {
    if (!Array.isArray(value) || value.length > 256) throw new Error('Invalid proxy list');
    return value.map(entry => {
      if (!entry || !text(entry.id, 255) || entry.id === DIRECT || typeof entry.name !== 'string' || entry.name.length > 255) throw new Error('Invalid proxy list');
      return {id: entry.id, name: entry.name || entry.id};
    });
  }

  // What the daemon's `registered` message carries: where its proxy listens
  // (absent host: where the daemon is), the login and the choices.
  function offer(value, server) {
    if (!value || typeof value !== 'object' || !Number.isInteger(value.port) || value.port < 1 || value.port > 65535 ||
        !text(value.password, 255) || /\s/.test(value.password)) throw new Error('Invalid proxy offer');
    const host = value.host === undefined ? new URL(server).hostname : value.host;
    if (!text(host, 253) || !/^(\[[0-9a-f:.]+\]|[a-z0-9._-]+)$/i.test(host)) throw new Error('Invalid proxy offer');
    // The daemon's own name, to call its network by.
    const node = value.node !== undefined && text(value.node, 80) ? value.node : '';
    // This browser's login, issued by the daemon (absent: an older daemon).
    const user = value.user !== undefined && text(value.user, 80) && /^[\w.-]+$/.test(value.user) ? value.user : '';
    return {host: bare(host), port: value.port, password: value.password, node, user, proxies: choices(value.proxies ?? [])};
  }

  // The login's user. The daemon keeps each browser's pick and knows the browser
  // by it, so it does not change with the pick: Firefox remembers the first login
  // it gives a proxy. An older daemon takes the proxy's name there (percent-encoded,
  // as ids hold colons).
  function identity(offer, choice) {
    return offer.user || encodeURIComponent(choice);
  }

  // `Proxy-Authorization`.
  function login(user, password) {
    return 'Basic ' + btoa(`${user}:${password}`);
  }

  function address(host) {
    const v4 = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(host);
    return v4 && v4.slice(1).every(n => +n <= 255) ? v4.slice(1).map(Number) : null;
  }
  function inNetwork(octets, cidr) {
    const [net, bits] = cidr.split('/');
    const base = net.split('.').map(Number);
    let left = +bits;
    for (let i = 0; i < 4 && left > 0; i++, left -= 8) {
      const mask = left >= 8 ? 255 : (255 << (8 - left)) & 255;
      if ((octets[i] & mask) !== (base[i] & mask)) return false;
    }
    return true;
  }

  // Whether a request stays direct: the daemons (this connection must not
  // depend on itself), local names and addresses of private networks.
  function direct(host, daemons) {
    host = bare(host);
    if (daemons.some(d => bare(d) === host)) return true;
    if (!host.includes('.') && !host.includes(':')) return true;
    if (host === 'localhost' || host.endsWith('.localhost') || host.endsWith('.local') || host === '::1') return true;
    if (/^(fe[89ab][0-9a-f]:|f[cd][0-9a-f]{2}:)/.test(host)) return true;
    const octets = address(host);
    return !!octets && NETWORKS.some(n => inNetwork(octets, n));
  }

  // Chromium's `proxy.settings` bypass rules for the same.
  function bypass(daemons) {
    return ['<local>', '*.local', '*.localhost', ...NETWORKS, '[::1]', ...new Set(daemons.map(bare))];
  }

  return {DIRECT, choices, offer, identity, login, direct, bypass};
})();
