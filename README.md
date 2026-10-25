# Karasearch

A small Rust homepage for a self-hosted Karakeep library. Axum serves the frontend and talks to Karakeep's REST API; no Node build, separate account database, or changes to Karakeep required.

- Sign in with a personal Karakeep API key.
- Search your library, browse tags, and load more results.
- Pin links to a bookmark bar using Karakeep favourites.
- Browse Karakeep lists as nested folders on the bar (`parentId` nesting, one click to open).
- Live library search while typing, with a suggestion dropdown (first row is `your query (search)`).
- Add/remove tags and save new links directly to Karakeep.
- Customize the heading, theme (Paper, After hours, Catppuccin Latte/Frappe/Macchiato/Mocha, Dracula), compact layout, and search engines.
- Submit web searches to Private Search or any HTTP(S) search URL with a `{query}` placeholder. External search runs only on submit, not while typing.

## Run locally

Requires Rust/Cargo and a running Karakeep instance with its web port published to the host. The source checkout at `../karakeep` is not a running service.

```sh
KARAKEEP_URL=http://127.0.0.1:3000 \
SEARCH_URL='http://localhost:8080/search?q={query}&t=general' \
cargo run
```

Open `http://127.0.0.1:8787`. Create an API key in Karakeep's **Settings > API Keys**, then paste it into the login form. Give the key user/bookmark/tag read access and bookmark/tag write access for editing, if your Karakeep version supports scoped keys. Existing Karakeep email/password or OAuth login remains in Karakeep; this app does not copy its password database or impersonate its login form.

The Private Search default matches `../private-search`: port 8080, `/search?q=...&t=general`. Change its address in Customize if hosted elsewhere. Its URL must be reachable **from your browser**, unlike `KARAKEEP_URL`, which must be reachable **from the Rust server**.

| Environment | Default | Purpose |
| --- | --- | --- |
| `KARAKEEP_URL` | `http://127.0.0.1:3000` | Karakeep base URL, without `/api/v1`; a base path is supported |
| `BIND_ADDR` | `127.0.0.1:8787` | Listener address |
| `SEARCH_URL` | `http://localhost:8080/search?q={query}&t=general` | Initial external engine template |
| `COOKIE_SECURE` | `false` | Set `true` behind HTTPS |

The binary reads environment variables, not `.env` files. Docker Compose reads `.env` for interpolation. Empty or unset `SEARCH_URL` uses the default. Existing browser preferences override `SEARCH_URL` after customization.

## Container

```sh
docker compose up --build -d
```

Compose connects to Karakeep's host-published port 3000 through `host.docker.internal`. This requires that port to be reachable from the container network. If Karakeep is in another Compose stack, preferably join Karasearch to its Docker network and set `KARAKEEP_URL=http://<karakeep-service>:3000`. Do not use `localhost` to refer to another container. Change the port to match your setup. No Karakeep volumes or database files are mounted.

The default port mapping is loopback-only. For LAN access, deliberately change the binding and place the app behind an HTTPS reverse proxy with `COOKIE_SECURE=true`. Do not send API keys over untrusted plain HTTP. This is a personal/local app, not a hardened public login service: add ingress rate limiting before public exposure.

## Search and customization

Choose **My bookmarks** for Karakeep search, or an engine for browser navigation. Karakeep's own query syntax is passed through unchanged. Selecting a tag browses that collection; submitting a search starts a library-wide search rather than silently filtering only loaded cards. `/` focuses search when not editing another field.

Customize accepts one engine per line:

```text
Private Search | http://localhost:8080/search?q={query}&t=general
DuckDuckGo | https://duckduckgo.com/?q={query}
Google | https://www.google.com/search?q={query}
```

Queries are URL-encoded before substitution. Search engines are not proxied. Bookmark URLs and labels are rendered without trusting HTML from Karakeep; there are no third-party fonts, favicons, scripts, or analytics requests.

## Deliberate limits

- API keys live only in server memory. Opaque HttpOnly/SameSite cookies expire after 12 hours; restart signs everyone out. Logout removes the session, not the upstream API key. Sessions are capped at 1,024. A new login replaces that browser's previous session.
- Mutations require a custom same-origin header; no cross-origin API access is enabled. Upstream redirects are disabled to prevent forwarding credentials to another host. The upstream host is operator-configured, never supplied by the login form.
- Preferences live in browser local storage, keyed by Karakeep user ID. They do not sync between devices. Favourites and tags do sync because Karakeep owns them. Pinning changes your Karakeep favourites, not a separate list.
- Folders are read-only here: lists are created, renamed, and filled in Karakeep. Each folder loads its first 40 bookmarks on first open and caches them until refresh; smart lists work because Karakeep resolves the query.
- Live search queries only Karakeep, debounced 220 ms. The selected web engine is contacted only when you submit or pick the `(search)` suggestion row.
- The bar shows links among the first 40 favourites. **Pinned** provides paginated access to all favourites. Bookmark pages contain 40 records; tags load 100 at a time.
- Notes/assets can appear in library results, but have no external link. This app is not a content reader or attachment viewer.
- Static assets are embedded into the binary; restart `cargo run` after frontend edits.

## Checks

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
node tests/frontend.mjs
```

Rust tests use a loopback mock upstream to check authentication, protected routes, CSRF rejection, session expiry/logout, query encoding/pagination, and mutation payloads. They do not require a live Karakeep instance or credentials.

For manual browser checks without touching a real library, start `node tests/mock-karakeep.mjs`, then run `KARAKEEP_URL=http://127.0.0.1:18787 BIND_ADDR=127.0.0.1:18788 cargo run` in another terminal. Visit `http://localhost:18788` and use the fixture key `test-key`. Fixture data is synthetic and resets when its process stops.
