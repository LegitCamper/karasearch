export function safeUrl(value) {
  try {
    const url = new URL(value);
    return ['http:', 'https:'].includes(url.protocol) && !url.username && !url.password ? url.href : null;
  } catch { return null; }
}

export function parseEngines(text) {
  const lines = text.split('\n').map(line => line.trim()).filter(Boolean);
  if (!lines.length || lines.length > 12) throw new Error('Add between 1 and 12 search engines.');
  const names = new Set();
  return lines.map(line => {
    const separator = line.indexOf('|');
    const name = line.slice(0, separator).trim();
    const template = line.slice(separator + 1).trim();
    if (separator < 1 || !name || name.length > 40 || names.has(name.toLowerCase())) throw new Error('Each engine needs a unique name, followed by | and its URL.');
    const marker = 'karasearchquerymarker';
    const checked = safeUrl(template.replaceAll('{query}', marker));
    if (!checked || !template.includes('{query}') || template.length > 2048 || new URL(checked).host.includes(marker)) throw new Error(`${name}: use an HTTP(S) URL with {query} in its path or query, not its hostname.`);
    names.add(name.toLowerCase());
    return { name, template };
  });
}

export const THEMES = ['light', 'dark', 'latte', 'frappe', 'macchiato', 'mocha', 'dracula'];

export function histerInfo(document) {
  const url = safeUrl(document.url);
  const domain = url ? new URL(url).hostname.replace(/^www\./, '') : 'Indexed document';
  return { url, domain, title: document.title || domain, description: document.text || '' };
}

export function engineUrl(template, query) {
  const destination = template.replaceAll('{query}', encodeURIComponent(query));
  if (!safeUrl(destination)) throw new Error('Invalid search engine URL. Update Customize settings.');
  return destination;
}

if (typeof document !== 'undefined') start();

function start() {
  const byId = id => document.getElementById(id);
  let user = null;
  let config = {};
  let defaults;
  let preferences;
  let current = { title: 'All bookmarks', params: {} };
  let cursor = null;
  let tagCursor = null;
  let generation = 0;
  let shown = 0;
  let editedBookmark = null;
  let allTags = [];
  let latest = [];
  let histerLatest = [];
  let histerCursor = null;
  let histerShown = 0;
  let liveTimer = null;

  function notice(message = '') {
    byId('notice').textContent = message;
    byId('notice').hidden = !message;
  }

  function showLogin() {
    user = null;
    generation++;
    document.querySelectorAll('dialog[open]').forEach(dialog => dialog.close());
    byId('login-view').hidden = false;
    for (const id of ['home-view', 'library-nav', 'account-actions']) byId(id).hidden = true;
    clearTimeout(liveTimer);
    hideSuggestions();
    for (const id of ['bookmark-grid', 'bookmark-bar', 'folder-bar', 'tag-list', 'editable-tags', 'history-list', 'hister-grid']) byId(id).replaceChildren();
    byId('history-section').hidden = true;
    byId('hister-results').hidden = true;
    latest = [];
    histerLatest = [];
    histerShown = 0;
    histerCursor = null;
    byId('search-input').value = '';
    current = { title: 'All bookmarks', params: {} };
    allTags = [];
    editedBookmark = null;
    document.documentElement.removeAttribute('data-theme');
    byId('connection-label').textContent = 'Ready for your Karakeep.';
    byId('login-form').reset();
  }

  async function api(path, options = {}) {
    const response = await fetch(`/api/${path}`, { ...options, headers: { 'Content-Type': 'application/json', 'X-Karasearch-Request': '1' } });
    let data;
    try { data = await response.json(); } catch { throw new Error(`Unexpected server response (${response.status}).`); }
    if (!response.ok) {
      if (response.status === 401 && user) showLogin();
      const error = new Error(data.error || `Request failed (${response.status}).`);
      error.status = response.status;
      throw error;
    }
    return data;
  }

  async function action(button, operation, errorTarget) {
    button.disabled = true;
    if (errorTarget) errorTarget.textContent = '';
    try { await operation(); } catch (error) {
      if (errorTarget) errorTarget.textContent = error.message;
      else notice(error.message);
    } finally { button.disabled = false; }
  }

  function node(tag, className, text) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = text;
    return element;
  }

  function button(text, className, handler) {
    const element = node('button', className, text);
    element.type = 'button';
    element.addEventListener('click', () => action(element, handler));
    return element;
  }

  function bookmarkInfo(bookmark) {
    const content = bookmark.content || {};
    const url = safeUrl(content.url);
    const domain = url ? new URL(url).hostname.replace(/^www\./, '') : content.type === 'text' ? 'Saved note' : 'Saved asset';
    return { url, domain, title: bookmark.title || content.title || domain, description: content.description || content.text || bookmark.summary || '' };
  }

  function applyPreferences() {
    document.documentElement.dataset.theme = preferences.theme;
    document.documentElement.dataset.density = preferences.density;
    byId('greeting').textContent = preferences.greeting;
    const select = byId('search-engine');
    const previous = select.value;
    select.replaceChildren(new Option(user.histerConnected ? 'Karakeep + Hister' : 'Karakeep bookmarks', 'bookmarks'), ...preferences.engines.map((engine, index) => new Option(engine.name, String(index))));
    if ([...select.options].some(option => option.value === previous)) select.value = previous;
    updateHint();
  }

  function webEngine() {
    const selected = byId('search-engine').value;
    return preferences.engines[selected === 'bookmarks' ? 0 : Number(selected)];
  }

  function updateHint() {
    const sources = user.histerConnected ? 'Karakeep bookmarks and Hister indexed pages' : 'Karakeep bookmarks';
    byId('search-hint').textContent = `Typing searches ${sources}. Submitting ${byId('search-engine').value === 'bookmarks' ? 'keeps results here' : `opens ${webEngine().name}`}.`;
  }

  function preferenceKey() { return `karasearch:${user.id}:preferences`; }

  async function signedIn(profile) {
    user = profile;
    preferences = structuredClone(defaults);
    try {
      const saved = JSON.parse(localStorage.getItem(preferenceKey()));
      if (saved) {
        const engines = parseEngines(saved.engines.map(engine => `${engine.name} | ${engine.template}`).join('\n'));
        preferences = { greeting: typeof saved.greeting === 'string' && saved.greeting.trim() ? saved.greeting.slice(0, 100) : defaults.greeting, theme: THEMES.includes(saved.theme) ? saved.theme : defaults.theme, density: saved.density === 'compact' ? 'compact' : 'comfortable', engines };
      }
    } catch { notice('Saved preferences could not be read. Defaults restored for this visit.'); }
    applyPreferences();
    byId('login-view').hidden = true;
    for (const id of ['home-view', 'library-nav', 'account-actions']) byId(id).hidden = false;
    byId('account-name').textContent = user.name || user.email || 'My library';
    byId('connection-label').textContent = user.histerConnected ? 'Karakeep + Hister connected.' : 'Karakeep connected.';
    byId('search-input').focus();
    const wanted = new URLSearchParams(location.search).get('q');
    if (wanted && wanted.trim()) {
      try { await handoff(wanted.trim().slice(0, 2048)); } catch (error) { notice(error.message); }
      return;
    }
    await refresh();
  }

  // Browser-level search entry (/?q=...): library first, web engine when the library has nothing.
  async function handoff(query) {
    byId('search-input').value = query;
    history.replaceState(null, '', '/');
    const owner = user;
    const results = selectCollection(`Results for "${query}"`, { q: query });
    const requestGeneration = generation;
    await Promise.all([results, loadPins(), loadTags(), loadLists(), loadHistory()]);
    if (owner === user && requestGeneration === generation && !shown && !histerShown) window.location.assign(engineUrl(preferences.engines[0].template, query));
  }

  async function refresh() {
    const results = await Promise.allSettled([loadBookmarks(), loadPins(), loadTags(), loadLists(), loadHistory()]);
    const errors = results.filter(result => result.status === 'rejected').map(result => result.reason.message);
    if (errors.length) notice([...new Set(errors)].join(' '));
  }

  async function selectCollection(title, params) {
    clearTimeout(liveTimer);
    hideSuggestions();
    current = { title, params };
    byId('search-input').value = params.q || '';
    byId('search-engine').value = 'bookmarks';
    updateHint();
    await loadBookmarks();
  }

  async function loadBookmarks(more = false) {
    if (more) return loadKarakeepBookmarks(true);
    const owner = user;
    const bookmarks = loadKarakeepBookmarks();
    const requestGeneration = generation;
    const results = await Promise.allSettled([bookmarks, loadHisterSearch()]);
    if (owner !== user || requestGeneration !== generation) return;
    const errors = results.filter(result => result.status === 'rejected').map(result => result.reason.message);
    if (errors.length) throw new Error(errors.join(' '));
  }

  async function loadKarakeepBookmarks(more = false) {
    const requestGeneration = more ? generation : ++generation;
    const query = new URLSearchParams(current.params);
    if (more && cursor) query.set('cursor', cursor);
    const grid = byId('bookmark-grid');
    grid.setAttribute('aria-busy', 'true');
    byId('load-more').hidden = true;
    if (!more) { grid.replaceChildren(); shown = 0; cursor = null; latest = []; }
    byId('library-heading').textContent = current.title;
    byId('result-status').textContent = 'Searching Karakeep...';
    byId('all-bookmarks').classList.toggle('active', !Object.keys(current.params).length);
    byId('pinned-bookmarks').classList.toggle('active', current.params.favourited === 'true');
    for (const element of byId('tag-list').children) element.classList.toggle('active', element.dataset.tag === current.params.tag);
    try {
      const data = await api(`bookmarks?${query}`);
      if (requestGeneration !== generation || !user) return;
      cursor = data.nextCursor;
      latest = more ? [...latest, ...data.bookmarks] : data.bookmarks;
      shown += data.bookmarks.length;
      for (const bookmark of data.bookmarks) grid.append(bookmarkCard(bookmark));
      if (!shown) grid.append(node('div', 'empty-state', current.params.q ? 'Nothing here yet. Try another query, or choose a search engine to explore the web.' : 'A little room for good finds. Save a link, or pick another collection.'));
      byId('result-status').textContent = `${shown} Karakeep bookmark${shown === 1 ? '' : 's'}${cursor ? ' loaded - more available' : ''}`;
    } catch (error) {
      if (requestGeneration !== generation || !user) return;
      byId('result-status').textContent = 'Karakeep unavailable. Use Refresh to try again.';
      throw error;
    } finally {
      if (requestGeneration === generation) {
        grid.setAttribute('aria-busy', 'false');
        byId('load-more').hidden = !cursor;
      }
    }
  }

  async function loadHisterSearch(more = false) {
    const section = byId('hister-results');
    const grid = byId('hister-grid');
    const status = byId('hister-status');
    const query = current.params.q;
    if (!more) { grid.replaceChildren(); histerLatest = []; histerShown = 0; histerCursor = null; }
    section.hidden = !query;
    byId('search-results').classList.toggle('has-query', Boolean(query));
    byId('load-hister-more').hidden = true;
    grid.setAttribute('aria-busy', 'false');
    if (!query) return;
    byId('hister-heading').textContent = `Hister results for "${query}"`;
    if (!config.hister || !user?.histerConnected) {
      status.textContent = config.hister ? 'Hister not connected. Add your token in Customize > Connected accounts.' : 'Hister not configured. Set HISTER_URL on the server to enable search.';
      return;
    }
    const owner = user;
    const requestGeneration = generation;
    const params = new URLSearchParams({ q: query });
    if (more && histerCursor) params.set('cursor', histerCursor);
    grid.setAttribute('aria-busy', 'true');
    status.textContent = 'Searching Hister...';
    try {
      const data = await api(`hister/search?${params}`);
      if (owner !== user || requestGeneration !== generation) return;
      const previousCursor = histerCursor;
      histerCursor = data.nextCursor && data.nextCursor !== previousCursor ? data.nextCursor : null;
      histerLatest = more ? [...histerLatest, ...data.documents] : data.documents;
      histerShown += data.documents.length;
      for (const document of data.documents) grid.append(histerCard(document));
      status.textContent = histerShown ? `${histerShown} Hister indexed page${histerShown === 1 ? '' : 's'}${histerCursor ? ' loaded - more available' : ''}` : 'No matching pages in Hister. Indexing is handled by Hister, not Save to Karakeep.';
    } catch (error) {
      if (owner !== user || requestGeneration !== generation) return;
      status.textContent = `Hister search failed: ${error.message} Use Refresh to retry.`;
      throw error;
    } finally {
      if (owner === user && requestGeneration === generation) {
        grid.setAttribute('aria-busy', 'false');
        byId('load-hister-more').hidden = !histerCursor;
      }
    }
  }

  async function loadPins() {
    const owner = user;
    const data = await api('bookmarks?favourited=true');
    if (!user || owner !== user) return;
    const bar = byId('bookmark-bar');
    bar.replaceChildren();
    for (const bookmark of data.bookmarks) {
      const info = bookmarkInfo(bookmark);
      if (!info.url) continue;
      const link = node('a', 'bar-link');
      link.href = info.url;
      link.title = info.title;
      link.append(node('span', 'bar-icon', info.title.charAt(0).toUpperCase()), node('span', 'bar-title', info.title));
      bar.append(link);
    }
    if (!bar.children.length) bar.append(node('p', 'small-note', 'Pin a link with the star on its card. Your Karakeep favourites live here.'));
    if (data.nextCursor) bar.append(button('See all pinned', 'quiet-button', () => selectCollection('Pinned bookmarks', { favourited: 'true' })));
  }

  async function loadHistory() {
    const section = byId('history-section');
    if (!config.hister || !user?.histerConnected) { section.hidden = true; return; }
    const owner = user;
    const data = await api('history');
    if (!user || owner !== user) return;
    const list = byId('history-list');
    list.replaceChildren();
    const seen = new Set();
    for (const item of data.documents || []) {
      const url = safeUrl(item.url);
      if (!url || seen.has(url)) continue;
      seen.add(url);
      const title = item.title || new URL(url).hostname.replace(/^www\./, '');
      const link = node('a', 'bar-link');
      link.href = url;
      link.title = `${title} - ${url}`;
      link.append(node('span', 'bar-icon', title.charAt(0).toUpperCase()), node('span', 'bar-title', title));
      list.append(link);
      if (seen.size >= 24) break;
    }
    if (!list.children.length) list.append(node('p', 'small-note', 'No indexed pages yet. Connect your browser extension or crawler to Hister; saving here only adds Karakeep bookmarks.'));
    section.hidden = false;
  }

  async function loadTags(more = false) {
    const owner = user;
    const data = await api(`tags${more && tagCursor ? `?cursor=${encodeURIComponent(tagCursor)}` : ''}`);
    if (!user || owner !== user) return;
    tagCursor = data.nextCursor;
    allTags = more ? [...allTags, ...data.tags] : data.tags;
    const list = byId('tag-list');
    list.replaceChildren();
    for (const tag of allTags) {
      const element = button(tag.name, 'nav-item', () => selectCollection(tag.name, { tag: tag.id }));
      element.dataset.tag = tag.id;
      element.classList.toggle('active', current.params.tag === tag.id);
      list.append(element);
    }
    if (!allTags.length) list.append(node('p', 'small-note', 'Add tags from any bookmark.'));
    byId('more-tags').hidden = !tagCursor;
  }

  function bookmarkCard(bookmark) {
    const info = bookmarkInfo(bookmark);
    const card = node('article', 'bookmark-card');
    const top = node('div', 'card-top');
    const pin = button(bookmark.favourited ? '\u2605' : '\u2606', 'pin-button', async () => {
      await api(`bookmarks/${encodeURIComponent(bookmark.id)}`, { method: 'PATCH', body: JSON.stringify({ favourited: !bookmark.favourited }) });
      bookmark.favourited = !bookmark.favourited;
      pin.textContent = bookmark.favourited ? '\u2605' : '\u2606';
      pin.setAttribute('aria-pressed', String(bookmark.favourited));
      await loadPins();
      if (current.params.favourited) await loadBookmarks();
    });
    pin.setAttribute('aria-label', `Pin ${info.title}`);
    pin.setAttribute('aria-pressed', String(Boolean(bookmark.favourited)));
    top.append(node('span', 'site-icon', info.domain.charAt(0).toUpperCase()), node('span', 'card-domain', info.domain), pin);
    const heading = node('h3');
    const title = node(info.url ? 'a' : 'span', '', info.title);
    if (info.url) title.href = info.url;
    heading.append(title);
    const bottom = node('div', 'card-bottom');
    const tags = node('div', 'card-tags');
    for (const tag of bookmark.tags || []) tags.append(button(tag.name, 'tag-chip', () => selectCollection(tag.name, { tag: tag.id })));
    bottom.append(tags, button('Edit tags', 'quiet-button', () => {
      editedBookmark = bookmark;
      byId('tag-bookmark-title').textContent = info.title;
      byId('new-tag').value = '';
      byId('tag-form').querySelector('.form-error').textContent = '';
      renderEditableTags();
      byId('tags-dialog').showModal();
    }));
    card.append(node('span', 'source-badge source-karakeep', 'Karakeep / bookmark'), top, heading, node('p', 'card-description', info.description), bottom);
    return card;
  }

  function histerCard(document) {
    const info = histerInfo(document);
    const card = node('article', 'bookmark-card');
    const heading = node('h3');
    const title = node(info.url ? 'a' : 'span', '', info.title);
    if (info.url) title.href = info.url;
    heading.append(title);
    card.append(node('span', 'source-badge source-hister', 'Hister / indexed page'), node('p', 'card-domain', info.domain), heading, node('p', 'card-description', info.description));
    return card;
  }

  async function loadLists() {
    const owner = user;
    const data = await api('lists');
    if (!user || owner !== user) return;
    const lists = (data.lists || []).filter(list => typeof list.id === 'string' && typeof list.name === 'string');
    const children = parent => lists.filter(list => (list.parentId || null) === parent);
    const bar = byId('folder-bar');
    bar.replaceChildren();
    for (const list of children(null)) bar.append(folder(list, children, 'folder'));
    if (!lists.length) bar.append(node('p', 'small-note', 'Make lists in Karakeep and they show up here as folders.'));
  }

  function folder(list, children, className) {
    const details = node('details', className);
    const summary = node('summary', '', `${list.icon || '\u{1F4C1}'} ${list.name}`);
    const panel = node('div', className === 'folder' ? 'folder-panel' : '');
    details.append(summary, panel);
    details.addEventListener('toggle', () => {
      if (!details.open) return;
      for (const other of details.parentElement.children) if (other !== details && other.tagName === 'DETAILS') other.open = false;
      if (panel.dataset.loaded) return;
      panel.dataset.loaded = '1';
      panel.append(node('p', 'small-note', 'Loading...'));
      action(summary, async () => {
        const data = await api(`bookmarks?list=${encodeURIComponent(list.id)}`);
        panel.replaceChildren();
        for (const child of children(list.id)) panel.append(folder(child, children, 'nested'));
        for (const bookmark of data.bookmarks) {
          const info = bookmarkInfo(bookmark);
          if (!info.url) continue;
          const link = node('a', '', info.title);
          link.href = info.url;
          link.title = info.title;
          panel.append(link);
        }
        if (!panel.children.length) panel.append(node('p', 'small-note', 'This list is empty.'));
        panel.append(button('Open in library', 'quiet-button', () => selectCollection(list.name, { list: list.id })));
      }).catch(() => { panel.dataset.loaded = ''; });
    });
    return details;
  }

  function hideSuggestions() {
    byId('suggestions').hidden = true;
    byId('suggestions').replaceChildren();
  }

  function suggestion(text, label, handler) {
    const row = button('', '', handler);
    row.setAttribute('role', 'option');
    row.append(node('span', 'suggest-text', text), node('span', 'suggest-tag', label));
    return row;
  }

  function renderSuggestions(query) {
    const box = byId('suggestions');
    box.replaceChildren();
    if (!query || !user) return hideSuggestions();
    // ponytail: library results already appear live while typing, so the first row always
    // means "take this to the web" - the selected engine, or the first one when browsing bookmarks.
    const engine = webEngine();
    box.append(suggestion(`${query} (search)`, engine.name, async () => {
      hideSuggestions();
      window.location.assign(engineUrl(engine.template, query));
    }));
    for (const [source, results, describe] of [['Karakeep', latest, bookmarkInfo], ['Hister', histerLatest, histerInfo]]) {
      for (const result of results.slice(0, 3)) {
        const info = describe(result);
        if (!info.url) continue;
        box.append(suggestion(info.title, `${source} / ${info.domain}`, async () => window.location.assign(info.url)));
      }
    }
    box.hidden = false;
  }

  async function liveSearch(query) {
    current = { title: query ? `Results for "${query}"` : 'All bookmarks', params: query ? { q: query } : {} };
    const owner = user;
    const results = loadBookmarks();
    const requestGeneration = generation;
    try { await results; } finally {
      if (user === owner && requestGeneration === generation && byId('search-input').value.trim() === query) renderSuggestions(query);
    }
  }

  function renderEditableTags() {
    const container = byId('editable-tags');
    container.replaceChildren();
    for (const tag of editedBookmark.tags || []) {
      const remove = node('button', 'tag-chip', `${tag.name} \u00d7`);
      remove.type = 'button';
      remove.setAttribute('aria-label', `Remove tag ${tag.name}`);
      remove.addEventListener('click', () => action(remove, async () => {
        const bookmark = editedBookmark;
        await api(`bookmarks/${encodeURIComponent(bookmark.id)}/tags`, { method: 'DELETE', body: JSON.stringify({ name: tag.name }) });
        bookmark.tags = bookmark.tags.filter(existing => existing.id !== tag.id);
        if (editedBookmark === bookmark) renderEditableTags();
        await refresh();
      }, byId('tag-form').querySelector('.form-error')));
      container.append(remove);
    }
    if (!container.children.length) container.append(node('p', 'small-note', 'No tags yet. Give this find a home.'));
  }

  // One form, two modes: an unclaimed server registers, an existing account signs in.
  function loginMode(registering) {
    byId('key-fields').hidden = !registering;
    byId('api-key').required = registering;
    byId('hister-field').hidden = !(registering && config.hister);
    byId('login-step').textContent = registering ? '01 / MAKE YOURSELF AT HOME' : '01 / WELCOME BACK';
    byId('login-heading').textContent = registering ? 'Create your account' : 'Sign in';
    byId('login-action').textContent = registering ? 'Create account' : 'Sign in';
    byId('login-blurb').textContent = registering
      ? 'Pick a username and password, then paste your keys once. Future sign-ins need only the password.'
      : 'One account, saved in your password manager. It unlocks your saved keys.';
    byId('password').setAttribute('autocomplete', registering ? 'new-password' : 'current-password');
    // Offer the other mode only when the server would accept a new account.
    byId('mode-switch').hidden = !config.canRegister;
    byId('switch-mode').textContent = registering ? 'Already have an account? Sign in' : 'Need an account? Create one';
    byId('login-form').querySelector('.form-error').textContent = '';
  }

  byId('login-form').addEventListener('submit', event => {
    event.preventDefault();
    const form = event.currentTarget;
    const registering = !byId('key-fields').hidden;
    const body = { username: byId('username').value, password: byId('password').value };
    if (registering) {
      body.api_key = byId('api-key').value;
      body.hister_token = byId('hister-token').value || null;
    }
    action(event.submitter, async () => {
      notice();
      const profile = await api(registering ? 'register' : 'login', { method: 'POST', body: JSON.stringify(body) });
      form.reset();
      // The server is claimed now, so a later sign-out shows the sign-in form.
      // A server that only allowed one account is claimed now.
      if (registering && !config.openRegistration) config.canRegister = false;
      await signedIn(profile);
    }, form.querySelector('.form-error'));
  });

  byId('save-keys').addEventListener('click', event => action(event.currentTarget, async () => {
    const profile = await api('keys', {
      method: 'POST',
      body: JSON.stringify({ api_key: byId('setting-api-key').value, hister_token: byId('setting-hister').value || null }),
    });
    byId('setting-api-key').value = '';
    byId('setting-hister').value = '';
    user = profile;
    applyPreferences();
    byId('connection-label').textContent = user.histerConnected ? 'Karakeep + Hister connected.' : 'Karakeep connected.';
    notice('Keys updated.');
    await refresh();
  }, byId('key-error')));
  byId('switch-mode').addEventListener('click', () => loginMode(byId('key-fields').hidden));
  byId('logout').addEventListener('click', event => action(event.currentTarget, async () => { await api('logout', { method: 'POST' }); showLogin(); notice('Signed out.'); }));
  byId('search-engine').addEventListener('change', () => {
    updateHint();
    const query = byId('search-input').value.trim();
    if (!byId('suggestions').hidden) renderSuggestions(query);
  });
  byId('search-input').addEventListener('input', () => {
    const query = byId('search-input').value.trim();
    generation++;
    latest = [];
    histerLatest = [];
    hideSuggestions();
    clearTimeout(liveTimer);
    liveTimer = setTimeout(() => liveSearch(query).catch(error => notice(error.message)), 220);
  });
  byId('search-input').addEventListener('keydown', event => {
    if (event.key === 'ArrowDown') {
      const query = byId('search-input').value.trim();
      if (byId('suggestions').hidden) renderSuggestions(query);
      const first = byId('suggestions').querySelector('button');
      if (first) { event.preventDefault(); first.focus(); }
    } else if (event.key === 'Escape') hideSuggestions();
  });
  byId('suggestions').addEventListener('keydown', event => {
    const rows = [...byId('suggestions').querySelectorAll('button')];
    const index = rows.indexOf(document.activeElement);
    if (event.key === 'ArrowDown' && index + 1 < rows.length) { event.preventDefault(); rows[index + 1].focus(); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); (index > 0 ? rows[index - 1] : byId('search-input')).focus(); }
    else if (event.key === 'Escape') { hideSuggestions(); byId('search-input').focus(); }
  });
  for (const type of ['pointerdown', 'focusin']) document.addEventListener(type, event => {
    if (!event.target.closest('.search-shell')) hideSuggestions();
  });
  byId('search-form').addEventListener('submit', event => {
    event.preventDefault();
    action(event.submitter, async () => {
      const query = byId('search-input').value.trim();
      clearTimeout(liveTimer);
      hideSuggestions();
      if (byId('search-engine').value === 'bookmarks') {
        await selectCollection(query ? `Results for "${query}"` : 'All bookmarks', query ? { q: query } : {});
      } else if (query) {
        const engine = preferences.engines[Number(byId('search-engine').value)];
        window.location.assign(engineUrl(engine.template, query));
      }
    });
  });
  byId('all-bookmarks').addEventListener('click', event => action(event.currentTarget, () => selectCollection('All bookmarks', {})));
  byId('pinned-bookmarks').addEventListener('click', event => action(event.currentTarget, () => selectCollection('Pinned bookmarks', { favourited: 'true' })));
  byId('load-more').addEventListener('click', event => action(event.currentTarget, () => loadBookmarks(true)));
  byId('load-hister-more').addEventListener('click', event => action(event.currentTarget, () => loadHisterSearch(true)));
  byId('more-tags').addEventListener('click', event => action(event.currentTarget, () => loadTags(true)));
  byId('refresh').addEventListener('click', event => action(event.currentTarget, async () => { notice(); await refresh(); }));

  byId('customize').addEventListener('click', () => {
    byId('setting-greeting').value = preferences.greeting;
    byId('setting-theme').value = preferences.theme;
    byId('setting-density').value = preferences.density;
    byId('setting-engines').value = preferences.engines.map(engine => `${engine.name} | ${engine.template}`).join('\n');
    byId('settings-form').querySelector('.form-error').textContent = '';
    byId('settings-dialog').showModal();
  });
  byId('settings-form').addEventListener('submit', event => {
    event.preventDefault();
    action(event.submitter, async () => {
      const greeting = byId('setting-greeting').value.trim();
      if (!greeting) throw new Error('Enter a homepage heading.');
      const updated = { greeting, theme: byId('setting-theme').value, density: byId('setting-density').value, engines: parseEngines(byId('setting-engines').value) };
      localStorage.setItem(preferenceKey(), JSON.stringify(updated));
      preferences = updated;
      applyPreferences();
      byId('settings-dialog').close();
      notice('Preferences saved in this browser.');
    }, event.currentTarget.querySelector('.form-error'));
  });
  byId('add-bookmark').addEventListener('click', () => {
    byId('save-form').reset();
    byId('save-form').querySelector('.form-error').textContent = '';
    byId('save-dialog').showModal();
  });
  byId('save-form').addEventListener('submit', event => {
    event.preventDefault();
    action(event.submitter, async () => {
      const url = safeUrl(byId('bookmark-url').value.trim());
      if (!url) throw new Error('Enter an HTTP(S) URL without credentials.');
      await api('bookmarks', { method: 'POST', body: JSON.stringify({ url, title: byId('bookmark-title').value.trim() }) });
      byId('save-dialog').close();
      current = { title: 'All bookmarks', params: {} };
      byId('search-input').value = '';
      await refresh();
      notice('Link saved to Karakeep.');
    }, event.currentTarget.querySelector('.form-error'));
  });
  byId('tag-form').addEventListener('submit', event => {
    event.preventDefault();
    action(event.submitter, async () => {
      const name = byId('new-tag').value.trim();
      if (!name) throw new Error('Enter a tag name.');
      const bookmark = editedBookmark;
      const result = await api(`bookmarks/${encodeURIComponent(bookmark.id)}/tags`, { method: 'POST', body: JSON.stringify({ name }) });
      for (const id of result.attached || []) {
        if (!(bookmark.tags || []).some(existing => existing.id === id)) bookmark.tags = [...(bookmark.tags || []), { id, name }];
      }
      if (editedBookmark === bookmark) {
        byId('new-tag').value = '';
        renderEditableTags();
      }
      await refresh();
    }, event.currentTarget.querySelector('.form-error'));
  });
  for (const close of document.querySelectorAll('[data-close]')) close.addEventListener('click', () => close.closest('dialog').close());
  document.addEventListener('keydown', event => {
    if (event.key === '/' && !event.ctrlKey && !event.metaKey && !event.altKey && user && !document.querySelector('dialog[open]') && !event.target.closest('input,textarea,select,[contenteditable]')) {
      event.preventDefault();
      byId('search-input').focus();
    }
  });
  byId('date-label').textContent = new Intl.DateTimeFormat(undefined, { weekday: 'long', month: 'long', day: 'numeric' }).format(new Date()).toUpperCase();

  async function boot() {
    try {
      config = await api('config');
      // Default to registering only on a server with no accounts yet; an open
      // server shows sign-in, because most visitors there already have one.
      loginMode(config.canRegister && !config.openRegistration);
      byId('setting-hister-field').hidden = !config.hister;
      defaults = { greeting: 'Make room for curiosity.', theme: 'light', density: 'comfortable', engines: parseEngines(`Private Search | ${config.searchUrl}`) };
      try { await signedIn(await api('me')); } catch (error) {
        showLogin();
        if (error.status !== 401) notice(error.message);
      }
    } catch (error) { notice(`${error.message} Reload this page to retry.`); }
    finally { byId('boot-status').hidden = true; }
  }
  boot();
}
