import http from 'node:http';

// Test-only Hister stand-in: profile + navigated-link history, matching
// GET /api/profile and GET /api/history?opened=true.
const visited = Array.from({ length: 12 }, (_, index) => ({
  id: index + 1,
  url: `https://example.com/visited/${index}`,
  title: ['Rust release notes', 'A quiet corner of the web', 'Reading list', 'Workshop notes'][index % 4] + ` / ${index + 1}`,
  query: index % 3 === 0 ? 'rust' : '',
  added: Math.floor(Date.now() / 1000) - index * 3600,
  add_count: (index % 4) + 1,
}));

const server = http.createServer((request, response) => {
  const url = new URL(request.url, 'http://localhost');
  const send = (status, value) => { response.writeHead(status, { 'Content-Type': 'application/json' }); response.end(JSON.stringify(value)); };
  if (request.headers.authorization !== 'Bearer test-hister-token') return send(403, { error: 'Invalid fixture token' });
  if (url.pathname === '/api/profile') return send(200, { user_id: 1, username: 'test reader', is_admin: false, revision: 1 });
  if (url.pathname === '/api/history') {
    if (url.searchParams.get('opened') === 'true') {
      const lastId = Number(url.searchParams.get('last_id') || 0);
      const page = visited.filter(item => !lastId || item.id > lastId).slice(0, 100);
      return send(200, { documents: page, last_id: page.at(-1)?.id || 0, last_updated_at: '' });
    }
    return send(200, { documents: visited.map(({ url, title, added }) => ({ url, title, updated: added })), last: '' });
  }
  send(404, { error: 'Not found' });
});
server.listen(18789, '127.0.0.1', () => console.log('Test-only Hister fixture: http://127.0.0.1:18789; token: test-hister-token'));
