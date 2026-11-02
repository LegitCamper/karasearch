# Karasearch

A small Rust homepage for a self-hosted Karakeep library. Axum serves the frontend and talks to Karakeep's REST API; no Node build, separate account database, or changes to Karakeep required.

- Create a Karasearch account, then sign in with a username and password your password manager can fill. It unlocks your stored Karakeep and Hister keys. Several people can each keep their own keys on one instance.
- Search your library, browse tags, and load more results.
- Pin links to a bookmark bar using Karakeep favourites.
- Browse Karakeep lists as nested folders on the bar (`parentId` nesting, one click to open).
- Live library search while typing, with a suggestion dropdown (first row is `your query (search)`).
- Add/remove tags and save new links directly to Karakeep.
- Customize the heading, theme (Paper, After hours, Catppuccin Latte/Frappe/Macchiato/Mocha, Dracula), compact layout, and search engines.
- Show recently indexed pages from [Hister](https://github.com/asciimoo/hister) on the front page, beside your bookmark bar.
- Submit web searches to Private Search or any HTTP(S) search URL with a `{query}` placeholder. External search runs only on submit, not while typing.

## Run locally

Requires Rust/Cargo and a running Karakeep instance with its web port published to the host. The source checkout at `../karakeep` is not a running service.

```sh
KARAKEEP_URL=http://127.0.0.1:3000 \
SEARCH_URL='http://localhost:8080/search?q={query}&t=general' \
cargo run
```

Open `http://127.0.0.1:8787`. The first visit to an unclaimed server shows a **Create your account** form: pick a username and password, and paste your Karakeep API key (create one in Karakeep's **Settings > API Keys**). Give the key user/bookmark/tag read access and bookmark/tag write access for editing, if your Karakeep version supports scoped keys.

After that, sign in with only the username and password. The upstream keys stay on the server, so your password manager stores one credential instead of two API keys. Change the stored keys later under **Customize > Connected accounts**.

Karasearch accounts are its own, separate from Karakeep's. Existing Karakeep email/password or OAuth login remains in Karakeep; this app does not copy its password database or impersonate its login form.

The Private Search default matches `../private-search`: port 8080, `/search?q=...&t=general`. Change its address in Customize if hosted elsewhere. Its URL must be reachable **from your browser**, unlike `KARAKEEP_URL`, which must be reachable **from the Rust server**.

| Environment | Default | Purpose |
| --- | --- | --- |
| `KARAKEEP_URL` | `http://127.0.0.1:3000` | Karakeep base URL, without `/api/v1`; a base path is supported |
| `HISTER_URL` | unset | Hister base URL, without `/api`; unset hides all Hister features |
| `BIND_ADDR` | `127.0.0.1:8787` | Listener address |
| `SEARCH_URL` | `http://localhost:8080/search?q={query}&t=general` | Initial external engine template |
| `COOKIE_SECURE` | `false` | Set `true` behind HTTPS |
| `SESSION_STORE` | unset | Encrypted account and session file; Compose sets `/data/sessions.enc`. **Without it, accounts vanish on restart.** |
| `ALLOW_REGISTRATION` | `false` | Keep accepting new accounts after the first; required for a shared, multi-user instance |
| `SESSION_SECRET` | unset | Stable random secret (at least 32 characters), required with `SESSION_STORE` |

The binary reads environment variables, not `.env` files. Docker Compose reads `.env` for interpolation. Empty or unset `SEARCH_URL` uses the default. Existing browser preferences override `SEARCH_URL` after customization.

## Hister

Set `HISTER_URL` to a Hister instance reachable **from the Rust server** to add indexed pages and Hister search. When unset, the token field is hidden and `/api/history` returns 404; search explains that Hister is not configured.

```sh
KARAKEEP_URL=http://127.0.0.1:3000 \
HISTER_URL=http://127.0.0.1:4433 \
cargo run
```

Hister owns its own accounts; Karasearch only stores a token for one of them. On a Hister server with `user_handling: true`, an administrator creates accounts with `POST /api/users` and reads each access token from `GET /api/users/{id}/token`; a user can rotate their own with `POST /api/users/{id}/token`. On a single-user server, the token is `app.access_token` from the Hister config. Paste that token in once at registration, or later under **Customize > Connected accounts**.

Keys are validated before they are stored: Karasearch calls Karakeep's `users/me` and Hister's `api/profile`, and registration fails if either rejects its key, so a half-configured account is never saved. Both keys are checked again at each sign-in, so a revoked key is reported at the login form instead of as a broken home page. An account without a Hister token works fine; the indexed-pages section stays hidden and history requests return 409. Search prompts that account to connect Hister.

The front page reads `GET /api/history` for Hister's recently indexed documents (up to 100 records, 24 shown), not just links opened from Hister's search UI. Search also queries Hister's indexed documents through `GET /api/hister/search`, which proxies Hister's `/search?format=json` using your stored token. Search pages contain up to 40 records with an independent **More Hister results** button. There is no history deletion here.

Search results and suggestions explicitly identify **Karakeep / bookmark** versus **Hister / indexed page**. **Save to Karakeep**, pins, and tags only write to Karakeep. Hister indexing remains managed by your Hister browser extension, crawler, or imports; Karasearch does not submit pages to Hister or discover browser history itself.

**Session file format changed.** The store now holds accounts as well as sessions, so the encryption context moved to `v3`. An existing `SESSION_STORE` file from an earlier version will not decrypt, and startup stops with an error rather than discarding it silently. Delete that file once, then register again.

## Accounts

Karasearch accounts exist only to unlock the upstream keys it holds for you. There is no email, no password reset, and no admin console: lose the password and the fix is deleting the store file and registering again.

Karasearch is multi-user: each account holds its own Karakeep key and its own Hister token, and sessions never share them. Accounts are capped at 256.

Only the default is restrictive. The first registration on an empty store succeeds; afterwards registration returns 403 unless `ALLOW_REGISTRATION=true`, because an instance briefly reachable on a network should not be claimable by whoever loads it first. For a shared server, set `ALLOW_REGISTRATION=true` and leave it on; the login form then shows sign-in with a link to create an account. For a personal one, leave it unset, or turn it on only while adding people.

Passwords must be 12-256 characters, and no composition rules are enforced; length and a password manager do more than forced punctuation. They are hashed with PBKDF2-HMAC-SHA256 at 600,000 rounds, with a unique 16-byte salt per account, and compared in constant time. Hashing is deliberately slow, which is why sign-in takes a moment.

Password hashes and both upstream keys live in the same encrypted store as sessions. The keys must be recoverable to call Karakeep and Hister, so they are encrypted rather than hashed: anyone holding both `SESSION_SECRET` and the store file can read them. Accounts are capped at 256.

This is a personal start page, not a hardened identity provider. There is no rate limiting on sign-in attempts; put a reverse proxy in front of it before exposing it to the internet.

## Health check

`GET /healthz` returns `{"status":"ok"}` with no authentication. It reports only that the process serves HTTP; it never calls Karakeep, so a Karakeep outage will not restart a working container. The image declares a `HEALTHCHECK` that runs `karasearch healthcheck`, which requests `/healthz` over loopback and exits non-zero on failure, so no `curl` or `wget` is needed in the runtime image. Check status with `docker compose ps` or point an external monitor at the path.

## Container

Generate a secret once with `openssl rand -hex 32`, then put `SESSION_SECRET=<generated value>` in your `.env` file (gitignored). Keep existing `.env` settings and restrict its permissions with `chmod 600 .env`. Compose requires this secret before starting.

```sh
docker compose up --build -d
```

### Sessions across restarts

Compose mounts the named `sessions` volume at `/data`, writable by the container's non-root user (UID 10001); the rest of the filesystem stays read-only. Sessions and their Karakeep API keys are encrypted with ChaCha20-Poly1305, using a key derived from `SESSION_SECRET`. A hash cannot replace the upstream API key: Karakeep requires the original bearer token on each request.

Keep the same secret and volume when recreating containers. Sessions keep their original **12-hour expiry**; a restart does not extend it. Logout persists removal before reporting success. Failed saves return errors instead of claiming that login/logout succeeded. Wrong secrets, corrupt files, and unwritable storage stop startup rather than silently discarding sessions. Existing memory-only sessions need one new login after enabling storage.

Keep the secret outside the session volume. Back up both separately; anyone with both can recover API keys. Encryption does not protect against someone who controls the host or running container. `docker compose down` preserves the volume; **`docker compose down -v` deletes saved sessions**. Do not rotate the secret without deliberately resetting the session file and signing everyone in again.

For native runs, create a private writable directory, set `SESSION_STORE` to a file inside it, and supply the same `SESSION_SECRET` on every run. Bind mounts must be writable by UID 10001. Run only one Karasearch process per session file; this is not a shared store for replicas. Leave `SESSION_STORE` unset for the simpler memory-only mode.

`KARAKEEP_URL` needs the scheme. A bare `karakeep:3000` is rejected, because `karakeep:` parses as the URL scheme; use `http://karakeep:3000`. Startup errors name the variable and the value they rejected.

Compose connects to Karakeep's host-published port 3000 through `host.docker.internal`. This requires that port to be reachable from the container network. If Karakeep is in another Compose stack, preferably join Karasearch to its Docker network and set `KARAKEEP_URL=http://<karakeep-service>:3000`. Do not use `localhost` to refer to another container. Change the port to match your setup. No Karakeep volumes or database files are mounted.

The default port mapping is loopback-only. For LAN access, deliberately change the binding and place the app behind an HTTPS reverse proxy with `COOKIE_SECURE=true`. Do not send API keys over untrusted plain HTTP. This is a personal/local app, not a hardened public login service: add ingress rate limiting before public exposure.

## Search and customization

Choose **Karakeep + Hister** to search both connected services, or an engine for browser navigation. Without a Hister connection, this option reads **Karakeep bookmarks**. Each service receives the query unchanged, so service-specific query syntax may produce different results. Results have separate source labels, counts, pagination, and error states; one service failing does not discard the other's results. Selecting a tag or folder still browses only Karakeep. `/` focuses search when not editing another field.

Customize accepts one engine per line:

```text
Private Search | http://localhost:8080/search?q={query}&t=general
DuckDuckGo | https://duckduckgo.com/?q={query}
Google | https://www.google.com/search?q={query}
```

### Browser-native search

`GET /?q=<query>` searches both connected services on load. It redirects to the first configured web engine only after successful searches return no results from either source; an upstream failure is not treated as an empty result. `/opensearch.xml` advertises that template (built from the request `Host`; the scheme follows `COOKIE_SECURE`), and `index.html` links to it. Signing in is still required; an unauthenticated `/?q=...` shows the login form, then runs the query after login.

Queries are URL-encoded before substitution. Search engines are not proxied. Bookmark URLs and labels are rendered without trusting HTML from Karakeep; there are no third-party fonts, favicons, scripts, or analytics requests.

## Deliberate limits

- Accounts and API keys stay server-side, encrypted on disk when `SESSION_STORE` is set, otherwise only in memory. Opaque HttpOnly/SameSite cookies expire after 12 hours. Without persistence, a restart deletes every account, not only sessions. Logout removes the session and leaves the account and its keys in place. Sessions are capped at 1,024 and accounts at 256. A new login replaces that browser's previous session.
- No password reset, email, or account recovery. Deleting the store file is the only reset, and it discards every account.
- Mutations require a custom same-origin header; no cross-origin API access is enabled. Upstream redirects are disabled to prevent forwarding credentials to another host. The upstream host is operator-configured, never supplied by the login form.
- Preferences live in browser local storage, keyed by Karakeep user ID. They do not sync between devices. Favourites and tags do sync because Karakeep owns them. Pinning changes your Karakeep favourites, not a separate list.
- Folders are read-only here: lists are created, renamed, and filled in Karakeep. Each folder loads its first 40 bookmarks on first open and caches them until refresh; smart lists work because Karakeep resolves the query.
- Live search queries Karakeep and connected Hister, debounced 220 ms. The selected web engine is contacted only when you submit or pick the `(search)` suggestion row.
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

For manual browser checks without touching a real library, start `node tests/mock-karakeep.mjs` and `node tests/mock-hister.mjs`, then run `KARAKEEP_URL=http://127.0.0.1:18787 HISTER_URL=http://127.0.0.1:18789 BIND_ADDR=127.0.0.1:18788 cargo run` in another terminal. Visit `http://localhost:18788`, register any username and password, and paste the fixture key `test-key` with the fixture token `test-hister-token`. Fixture data is synthetic and resets when its process stops.
