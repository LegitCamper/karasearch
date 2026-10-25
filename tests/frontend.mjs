import assert from 'node:assert/strict';
import { safeUrl, parseEngines, engineUrl, THEMES } from '../static/app.js';

assert.equal(safeUrl('javascript:alert(1)'), null);
assert.equal(safeUrl('https://user:pass@example.com'), null);
assert.equal(safeUrl('/relative'), null);
assert.equal(safeUrl('http://localhost:8080'), 'http://localhost:8080/');
const engines = parseEngines('Private Search | http://localhost:8080/search?q={query}&t=general\nDuckDuckGo | https://duckduckgo.com/?q={query}');
assert.equal(engines.length, 2);
assert.equal(engineUrl(engines[0].template, 'rust & web/#?'), 'http://localhost:8080/search?q=rust%20%26%20web%2F%23%3F&t=general');
for (const text of ['', 'Engine | javascript:{query}', 'Engine | https://example.com/', 'Engine | https://{query}.example.com/', 'Engine | https://example.com/?q={query}\nengine | https://other.com/?q={query}']) {
  assert.throws(() => parseEngines(text));
}
assert.throws(() => engineUrl('javascript:{query}', 'test'));
assert.ok(THEMES.includes('mocha') && THEMES.includes('dracula') && !THEMES.includes('../evil'));
console.log('Frontend URL, engine and theme checks passed.');
