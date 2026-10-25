import http from 'node:http';

const tags = [{ id: 'tag-rust', name: 'rust' }, { id: 'tag-design', name: 'design' }];
const bookmarks = Array.from({ length: 43 }, (_, index) => ({
  id: `bookmark-${index}`,
  title: ['The Rust programming language', 'A quieter kind of design', 'Building things for the web', 'Notes from the workshop'][index % 4] + (index > 3 ? ` / ${index + 1}` : ''),
  favourited: index < 3,
  content: { type: 'link', url: `https://example.com/reading/${index}`, description: 'A useful find, kept for another day. Explore the ideas that make the web a little more interesting.' },
  tags: [{ ...tags[index % 2] }],
}));

const server = http.createServer(async (request, response) => {
  const url = new URL(request.url, 'http://localhost');
  const send = (status, value) => { response.writeHead(status, { 'Content-Type': 'application/json' }); response.end(JSON.stringify(value)); };
  if (request.headers.authorization !== 'Bearer test-key') return send(401, { error: 'Invalid fixture key' });
  let text = '';
  for await (const chunk of request) text += chunk;
  let body;
  try { body = text ? JSON.parse(text) : {}; } catch { return send(400, { error: 'Invalid JSON' }); }
  if (url.pathname === '/api/v1/users/me') return send(200, { id: 'fixture-user', name: 'Test reader', email: 'reader@example.test', localUser: true });
  if (url.pathname === '/api/v1/tags') return send(200, { tags, nextCursor: null });
  if (url.pathname === '/api/v1/lists') return send(200, { lists: [
    { id: 'list-read', name: 'Reading', icon: '\u{1F4DA}', parentId: null, type: 'manual', public: false },
    { id: 'list-later', name: 'Later', icon: '\u{1F558}', parentId: 'list-read', type: 'manual', public: false },
    { id: 'list-work', name: 'Work', icon: '\u{1F4BC}', parentId: null, type: 'manual', public: false },
  ] });
  const listMatch = url.pathname.match(/^\/api\/v1\/lists\/([^/]+)\/bookmarks$/);
  const tagMatch = url.pathname.match(/^\/api\/v1\/tags\/([^/]+)\/bookmarks$/);
  if (request.method === 'GET' && (url.pathname.startsWith('/api/v1/bookmarks') || tagMatch || listMatch)) {
    let result = bookmarks;
    if (url.searchParams.get('favourited') === 'true') result = result.filter(bookmark => bookmark.favourited);
    if (tagMatch) result = result.filter(bookmark => bookmark.tags.some(tag => tag.id === tagMatch[1]));
    if (listMatch) result = result.filter((bookmark, index) => (listMatch[1] === 'list-read' ? index % 3 === 0 : listMatch[1] === 'list-work' ? index % 5 === 0 : false));
    if (url.pathname.endsWith('/search')) result = result.filter(bookmark => bookmark.title.toLowerCase().includes((url.searchParams.get('q') || '').toLowerCase()));
    const offset = Number(url.searchParams.get('cursor') || 0);
    const limit = Number(url.searchParams.get('limit') || 40);
    return send(200, { bookmarks: result.slice(offset, offset + limit), nextCursor: offset + limit < result.length ? String(offset + limit) : null });
  }
  if (request.method === 'POST' && url.pathname === '/api/v1/bookmarks') {
    const bookmark = { id: `bookmark-${bookmarks.length}`, title: body.title, content: { type: 'link', url: body.url }, favourited: false, tags: [] };
    bookmarks.unshift(bookmark);
    return send(201, bookmark);
  }
  const match = url.pathname.match(/^\/api\/v1\/bookmarks\/([^/]+)(\/tags)?$/);
  const bookmark = match && bookmarks.find(bookmark => bookmark.id === match[1]);
  if (!bookmark) return send(404, { error: 'Not found' });
  if (request.method === 'PATCH' && !match[2]) { bookmark.favourited = body.favourited; return send(200, bookmark); }
  if (match[2] && ['POST', 'DELETE'].includes(request.method)) {
    const changed = [];
    for (const input of body.tags) {
      let tag = tags.find(tag => tag.name === input.tagName);
      if (!tag && request.method === 'POST') { tag = { id: `tag-${tags.length}`, name: input.tagName }; tags.push(tag); }
      if (!tag) continue;
      if (request.method === 'POST' && !bookmark.tags.some(existing => existing.id === tag.id)) bookmark.tags.push({ ...tag });
      if (request.method === 'DELETE') bookmark.tags = bookmark.tags.filter(existing => existing.id !== tag.id);
      changed.push(tag.id);
    }
    return send(200, { [request.method === 'POST' ? 'attached' : 'detached']: changed });
  }
  send(405, { error: 'Method not allowed' });
});
server.listen(18787, '127.0.0.1', () => console.log('Test-only Karakeep fixture: http://127.0.0.1:18787; API key: test-key'));
