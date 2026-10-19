// Keep old bookmarks working; all browsing and control now use the regular view.
if (location.pathname.startsWith('/extension/')) {
  const query = new URLSearchParams(location.search);
  if (query.has('browser')) {
    const target = new URLSearchParams({browser: query.get('browser'), daemon: query.get('daemon') || ''});
    const page = query.get('tab') || query.get('page'); if (page) target.set('page', page);
    location.replace('/handoff/?' + target);
  } else location.replace('/#nodes');
}
