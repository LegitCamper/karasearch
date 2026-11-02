import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';
import { safeUrl, parseEngines, engineUrl, histerInfo, THEMES } from '../static/app.js';

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
assert.equal(histerInfo({ url: 'javascript:alert(1)' }).url, null);
assert.equal(histerInfo({ url: 'file:///private/document' }).url, null);
assert.equal(histerInfo({ url: 'https://www.example.com/page' }).title, 'example.com');
assert.equal(histerInfo({ title: '<script>plain text</script>', text: '<b>snippet</b>' }).description, '<b>snippet</b>');
console.log('Frontend URL, engine and theme checks passed.');

const markup = await readFile(new URL('../static/index.html', import.meta.url), 'utf8');
const ids = [...markup.matchAll(/\bid="([^"]+)"/g)].map(match => match[1]);
assert.equal(new Set(ids).size, ids.length, 'Every control needs a unique ID');
assert.match(markup, /<details id="library-nav"[^>]*><summary>Browse Karakeep bookmarks<\/summary>/);
assert.match(markup, /id="suggestions"[^>]*role="region"[^>]*aria-label="Search suggestions"/);
assert.match(markup, /<h1 id="query-heading">/);
for (const source of ['karakeep-results', 'hister-results']) {
  assert.ok(markup.includes(`href="#${source}"`), `${source} needs a visible jump link`);
  assert.match(markup, new RegExp(`<section[^>]*id="${source}"[^>]*tabindex="-1"`));
}
console.log('Search navigation and suggestion markup checks passed.');

const themeScript = await readFile(new URL('../static/theme.js', import.meta.url), 'utf8');
assert.ok(markup.includes('<script src="/theme.js"></script>'), 'Theme bootstrap must not be deferred');
assert.ok(markup.indexOf('<script src="/theme.js"></script>') < markup.indexOf('<link rel="stylesheet"'), 'Theme must run synchronously before CSS');
for (const saved of [...THEMES, null, 'invalid-theme']) {
  for (const dark of [true, false]) {
    const document = { documentElement: { dataset: {} } };
    runInNewContext(themeScript, {
      document,
      localStorage: { getItem: name => { assert.equal(name, 'karasearch:theme'); return saved; } },
      window: { matchMedia: () => ({ matches: dark }) },
    });
    assert.equal(document.documentElement.dataset.theme, THEMES.includes(saved) ? saved : dark ? 'dark' : 'light');
  }
}
const blockedStorageDocument = { documentElement: { dataset: {} } };
runInNewContext(themeScript, {
  document: blockedStorageDocument,
  localStorage: { getItem: () => { throw new Error('Storage disabled'); } },
  window: { matchMedia: () => ({ matches: true }) },
});
assert.equal(blockedStorageDocument.documentElement.dataset.theme, 'dark');
for (const service of ['karakeep', 'hister']) {
  assert.match(markup, new RegExp(`id="open-${service}"[^>]*target="_blank"[^>]*rel="noopener noreferrer"`));
  assert.ok(ids.includes(`setting-${service}-url`));
}
console.log('Pre-paint theme and service shortcut checks passed.');
