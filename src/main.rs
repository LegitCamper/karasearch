use std::{
    env, fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, patch, post},
};
use reqwest::{Client, Url};
use ring::{
    aead::{self, Nonce, UnboundKey},
    hkdf, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

const SESSION_SECONDS: u64 = 43_200;
const NONCE_LEN: usize = 12;
const MAX_ACCOUNTS: usize = 256;
/// OWASP's 2023 floor for PBKDF2-HMAC-SHA256. ring has no Argon2, and this
/// costs ~0.2s per sign-in, which is acceptable for a personal start page.
const PBKDF2_ROUNDS: u32 = 600_000;

/// ponytail: PBKDF2 because `ring` already ships it; move to Argon2id (new
/// dependency) if this ever faces untrusted internet traffic.
#[derive(Clone, Serialize, Deserialize)]
struct Account {
    username: String,
    /// Hex, 16 bytes, unique per account.
    salt: String,
    /// Hex, 32 bytes, PBKDF2-HMAC-SHA256 over the password.
    hash: String,
    /// Karakeep API key, encrypted at rest with the rest of this file.
    key: String,
    hister: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len() / 2)
        .map(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

impl Account {
    fn new(
        username: String,
        password: &str,
        key: String,
        hister: Option<String>,
    ) -> Result<Self, String> {
        let mut salt = [0u8; 16];
        SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| "Could not generate a password salt".to_owned())?;
        let mut hash = [0u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            PBKDF2_ROUNDS.try_into().expect("non-zero rounds"),
            &salt,
            password.as_bytes(),
            &mut hash,
        );
        Ok(Self {
            username,
            salt: hex(&salt),
            hash: hex(&hash),
            key,
            hister,
        })
    }

    /// Constant-time comparison; a mismatched or corrupt record fails closed.
    fn verify(&self, password: &str) -> bool {
        let (Some(salt), Some(expected)) = (unhex(&self.salt), unhex(&self.hash)) else {
            return false;
        };
        if expected.len() != 32 {
            return false;
        }
        let mut hash = [0u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            PBKDF2_ROUNDS.try_into().expect("non-zero rounds"),
            &salt,
            password.as_bytes(),
            &mut hash,
        );
        // Fold every byte before comparing, so timing does not reveal the
        // position of the first mismatch.
        hash.iter()
            .zip(&expected)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }
}

/// Everything persisted in the encrypted store file.
#[derive(Default, Serialize, Deserialize)]
struct Vault {
    #[serde(default)]
    accounts: Vec<Account>,
    /// (token, username, expires)
    #[serde(default)]
    sessions: Vec<(String, String, u64)>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

struct Store {
    path: PathBuf,
    key: [u8; 32],
}

impl Store {
    fn new(path: PathBuf, secret: &str) -> Result<Self, String> {
        if secret.len() < 32 {
            return Err("SESSION_SECRET must be at least 32 characters".into());
        }
        let mut key = [0u8; 32];
        hkdf::Salt::new(hkdf::HKDF_SHA256, b"karasearch session store v3")
            .extract(secret.as_bytes())
            .expand(&[b"sessions"], hkdf::HKDF_SHA256)
            .and_then(|okm| okm.fill(&mut key))
            .map_err(|_| "Could not derive the session encryption key".to_owned())?;
        Ok(Self { path, key })
    }

    fn cipher(&self) -> Result<aead::LessSafeKey, String> {
        UnboundKey::new(&aead::CHACHA20_POLY1305, &self.key)
            .map(aead::LessSafeKey::new)
            .map_err(|_| "Invalid session encryption key".to_owned())
    }

    fn load(&self) -> Result<Vault, String> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vault::default());
            }
            Err(error) => return Err(format!("Could not read {}: {error}", self.path.display())),
        };
        let mut sealed = Vec::new();
        file.take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut sealed)
            .map_err(|error| format!("Could not read session store: {error}"))?;
        if sealed.len() > 16 * 1024 * 1024 {
            return Err("Session store exceeds 16 MiB".into());
        }
        if sealed.len() <= NONCE_LEN {
            return Err("Session store file is truncated".into());
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let mut buffer = body.to_vec();
        let plain = self
            .cipher()?
            .open_in_place(
                Nonce::try_assume_unique_for_key(nonce)
                    .map_err(|_| "Invalid session store nonce".to_owned())?,
                aead::Aad::empty(),
                &mut buffer,
            )
            .map_err(|_| {
                "Could not decrypt the session store; SESSION_SECRET likely changed".to_owned()
            })?;
        let mut vault: Vault =
            serde_json::from_slice(plain).map_err(|_| "Session store is corrupt".to_owned())?;
        if vault.sessions.len() > 1024 {
            return Err("Session store exceeds session capacity".into());
        }
        if vault.accounts.len() > MAX_ACCOUNTS {
            return Err("Session store exceeds account capacity".into());
        }
        vault.sessions.retain(|(_, username, expires)| {
            *expires > now()
                && vault
                    .accounts
                    .iter()
                    .any(|account| &account.username == username)
        });
        Ok(vault)
    }

    fn save(&self, vault: &Vault) -> Result<(), String> {
        let mut buffer = serde_json::to_vec(vault).map_err(|_| "Could not encode sessions")?;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "Could not generate a nonce".to_owned())?;
        self.cipher()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                &mut buffer,
            )
            .map_err(|_| "Could not encrypt sessions".to_owned())?;
        let temporary = self.path.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("Could not open {}: {error}", temporary.display()))?;
        let result = file
            .write_all(&nonce)
            .and_then(|()| file.write_all(&buffer))
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, &self.path))
            .and_then(|()| {
                let parent = self
                    .path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty());
                fs::File::open(parent.unwrap_or_else(|| std::path::Path::new(".")))?.sync_all()
            });
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|error| format!("Could not persist {}: {error}", self.path.display()))
    }
}

#[derive(Clone)]
struct App {
    client: Client,
    upstream: Url,
    hister: Option<Url>,
    search_url: String,
    secure_cookie: bool,
    open_registration: bool,
    // ponytail: single-process store; add Redis/Postgres only when running replicas.
    vault: Arc<Mutex<Vault>>,
    store: Option<Arc<Store>>,
}

struct ApiError(StatusCode, &'static str);
type ApiResult<T> = Result<T, ApiError>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

/// Both upstreams map statuses the same way; only the service name differs, and
/// ApiError holds a &'static str, so each service carries its own literals.
fn upstream_error(hister: bool, status: StatusCode) -> ApiError {
    let (code, messages) = match status {
        StatusCode::UNAUTHORIZED => (
            status,
            (
                "Karakeep rejected this API key. Sign in again.",
                "Hister rejected this access token. Sign in again.",
            ),
        ),
        StatusCode::FORBIDDEN => (
            status,
            (
                "API key lacks permission for this action.",
                "Access token lacks permission for this action.",
            ),
        ),
        StatusCode::NOT_FOUND => (
            status,
            ("Karakeep resource not found.", "Hister resource not found."),
        ),
        StatusCode::TOO_MANY_REQUESTS => (
            status,
            (
                "Karakeep rate limit reached. Try again shortly.",
                "Hister rate limit reached. Try again shortly.",
            ),
        ),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => (
            StatusCode::BAD_REQUEST,
            (
                "Karakeep rejected this request. Check your query or input.",
                "Hister rejected this request. Check your query or input.",
            ),
        ),
        _ => (
            StatusCode::BAD_GATEWAY,
            (
                "Karakeep could not complete this request.",
                "Hister could not complete this request.",
            ),
        ),
    };
    ApiError(code, if hister { messages.1 } else { messages.0 })
}

const UNAUTHENTICATED: ApiError = ApiError(StatusCode::UNAUTHORIZED, "Sign in to continue.");

fn invalid(message: &'static str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message)
}

fn http_url(value: &str) -> Result<Url, &'static str> {
    let url = Url::parse(value).map_err(|_| "Expected an absolute HTTP(S) URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Expected an HTTP(S) URL without credentials");
    }
    Ok(url)
}

/// Name the variable and the value it rejected: a bare "invalid URL" leaves an
/// operator guessing which container setting is wrong.
fn setting(name: &str, value: &str, problem: &str) -> String {
    let example = match name {
        "SEARCH_URL" => "http://host:8080/search?q={query}",
        "HISTER_URL" => "http://host:4433",
        _ => "http://host:3000",
    };
    format!("{name} is set to '{value}', which is not usable. {problem}. Example: {name}={example}")
}

fn session_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == "karasearch_session").then_some(value))
}

impl App {
    /// The account behind this request's cookie, with its stored upstream keys.
    fn account(&self, headers: &HeaderMap) -> ApiResult<Account> {
        let mut vault = self.vault.lock().map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Session store unavailable",
            )
        })?;
        vault.sessions.retain(|(_, _, expires)| *expires > now());
        let id = session_id(headers).ok_or(UNAUTHENTICATED)?;
        let username = vault
            .sessions
            .iter()
            .find(|(token, _, _)| token == id)
            .map(|(_, username, _)| username.clone())
            .ok_or(UNAUTHENTICATED)?;
        vault
            .accounts
            .iter()
            .find(|account| account.username == username)
            .cloned()
            .ok_or(UNAUTHENTICATED)
    }

    fn key(&self, headers: &HeaderMap) -> ApiResult<String> {
        self.account(headers).map(|account| account.key)
    }

    /// The Hister token saved on this account, or a 404/409 naming what is missing.
    fn hister_key(&self, headers: &HeaderMap) -> ApiResult<String> {
        if self.hister.is_none() {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "This server is not configured for Hister. Set HISTER_URL.",
            ));
        }
        self.account(headers)?.hister.ok_or(ApiError(
            StatusCode::CONFLICT,
            "This account has no Hister token. Add one in Customize.",
        ))
    }

    /// Mutates a copy, persists it, and only then swaps it in, so a failed write
    /// never leaves memory claiming a change that is not on disk. Password hashing
    /// runs here too, hence spawn_blocking.
    async fn update_vault<T: Send + 'static>(
        &self,
        change: impl FnOnce(&mut Vault) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut vault = app.vault.lock().map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Session store unavailable",
                )
            })?;
            let mut updated = Vault {
                accounts: vault.accounts.clone(),
                sessions: vault.sessions.clone(),
            };
            updated.sessions.retain(|(_, _, expires)| *expires > now());
            let result = change(&mut updated)?;
            if let Some(store) = &app.store {
                store.save(&updated).map_err(|error| {
                    eprintln!("karasearch: {error}");
                    ApiError(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not save session changes; retry",
                    )
                })?;
            }
            *vault = updated;
            Ok(result)
        })
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Session store unavailable",
            )
        })?
    }

    async fn request(
        &self,
        key: &str,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> ApiResult<Value> {
        self.call(&self.upstream, key, method, path, query, body)
            .await
    }

    async fn call(
        &self,
        base: &Url,
        key: &str,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> ApiResult<Value> {
        let url = base.join(path).map_err(|_| invalid("Invalid API path"))?;
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(key)
            .query(query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let hister = self.hister.as_ref().is_some_and(|url| url == base);
        let response = request.send().await.map_err(|_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                if hister {
                    "Cannot reach Hister. Check HISTER_URL and container connectivity."
                } else {
                    "Cannot reach Karakeep. Check KARAKEEP_URL and container connectivity."
                },
            )
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(upstream_error(hister, status));
        }
        response.json().await.map_err(|_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                if hister {
                    "Invalid JSON response from Hister"
                } else {
                    "Invalid JSON response from Karakeep"
                },
            )
        })
    }

    fn cookie(&self, value: &str, age: u64) -> String {
        format!(
            "karasearch_session={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
            if self.secure_cookie { "; Secure" } else { "" }
        )
    }
}

async fn security(request: Request, next: Next) -> Response {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && (request
        .headers()
        .get("x-karasearch-request")
        .and_then(|value| value.to_str().ok())
        != Some("1")
        || request
            .headers()
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            == Some("cross-site"))
    {
        return ApiError(StatusCode::FORBIDDEN, "Same-origin request required").into_response();
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
    /// Required on register, optional on sign-in (ignored there).
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    hister_token: Option<String>,
}

/// Both upstreams accept opaque bearer tokens; reject anything that could not be
/// one before it reaches an upstream or the store.
fn bearer(value: &str, message: &'static str) -> ApiResult<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 4096
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid(message));
    }
    Ok(value.to_owned())
}

fn credentials(username: &str, password: &str) -> ApiResult<(String, String)> {
    let username = username.trim();
    if username.is_empty()
        || username.len() > 64
        || !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
    {
        return Err(invalid(
            "Username must be 1-64 characters: letters, digits, and - _ . @",
        ));
    }
    // Length is the only rule that matters here; composition rules push people
    // toward predictable passwords, and a manager generates these anyway.
    if password.len() < 12 || password.len() > 256 {
        return Err(invalid("Password must be 12-256 characters"));
    }
    Ok((username.to_owned(), password.to_owned()))
}

impl App {
    /// Confirms a key works before it is stored, so a typo fails at the form
    /// rather than silently breaking the home page later.
    async fn check_keys(
        &self,
        key: &str,
        hister: Option<&str>,
    ) -> ApiResult<(String, Option<String>, Value)> {
        let key = bearer(key, "Enter a valid Karakeep API key")?;
        let mut user = self
            .request(&key, Method::GET, "users/me", &[], None)
            .await?;
        if user.get("id").and_then(Value::as_str).is_none() {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "Karakeep returned no user ID",
            ));
        }
        let hister = match hister.map(str::trim) {
            None | Some("") => None,
            Some(token) => {
                let token = bearer(token, "Enter a valid Hister access token")?;
                let base = self.hister.as_ref().ok_or(ApiError(
                    StatusCode::BAD_REQUEST,
                    "This server is not configured for Hister. Set HISTER_URL.",
                ))?;
                let profile = self
                    .call(base, &token, Method::GET, "api/profile", &[], None)
                    .await?;
                if let Some(object) = user.as_object_mut() {
                    object.insert("hister".into(), profile);
                }
                Some(token)
            }
        };
        user["histerConnected"] = json!(self.hister.is_some() && hister.is_some());
        Ok((key, hister, user))
    }

    /// Issues the cookie and replaces this browser's previous session.
    async fn start_session(
        &self,
        username: String,
        headers: &HeaderMap,
    ) -> ApiResult<(String, String)> {
        let token = Uuid::new_v4().to_string();
        let cookie = self.cookie(&token, SESSION_SECONDS);
        let previous = session_id(headers).map(str::to_owned);
        let issued = token.clone();
        self.update_vault(move |vault| {
            if let Some(previous) = previous {
                vault
                    .sessions
                    .retain(|(existing, _, _)| existing != &previous);
            }
            if vault.sessions.len() >= 1024 {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Session capacity reached",
                ));
            }
            vault
                .sessions
                .push((issued, username, now() + SESSION_SECONDS));
            Ok(())
        })
        .await?;
        Ok((token, cookie))
    }
}

/// First run creates the account; afterwards registration is closed unless
/// ALLOW_REGISTRATION=true, so an exposed instance cannot be claimed by a stranger.
async fn register(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> ApiResult<Response> {
    let (username, password) = credentials(&body.username, &body.password)?;
    let key = body
        .api_key
        .as_deref()
        .ok_or(invalid("A Karakeep API key is required"))?;
    {
        let vault = app.vault.lock().map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Session store unavailable",
            )
        })?;
        if !vault.accounts.is_empty() && !app.open_registration {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "Registration is closed on this server.",
            ));
        }
    }
    let (key, hister, user) = app.check_keys(key, body.hister_token.as_deref()).await?;
    let name = username.clone();
    app.update_vault(move |vault| {
        if vault
            .accounts
            .iter()
            .any(|account| account.username == name)
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "That username is already taken.",
            ));
        }
        if vault.accounts.len() >= MAX_ACCOUNTS {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Account capacity reached",
            ));
        }
        vault
            .accounts
            .push(Account::new(name, &password, key, hister).map_err(|error| {
                eprintln!("karasearch: {error}");
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not create the account; retry",
                )
            })?);
        Ok(())
    })
    .await?;
    let (_, cookie) = app.start_session(username, &headers).await?;
    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(user),
    )
        .into_response())
}

async fn login(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> ApiResult<Response> {
    let username = body.username.trim().to_owned();
    let password = body.password.clone();
    // Hashing is deliberately slow, so verify off the async runtime.
    let account = app
        .update_vault(move |vault| {
            vault
                .accounts
                .iter()
                .find(|account| account.username == username && account.verify(&password))
                .cloned()
                .ok_or(ApiError(
                    StatusCode::UNAUTHORIZED,
                    "Incorrect username or password.",
                ))
        })
        .await?;
    // Surface a revoked upstream key at sign-in instead of as a broken home page.
    let (_, _, user) = app
        .check_keys(&account.key, account.hister.as_deref())
        .await?;
    let (_, cookie) = app.start_session(account.username, &headers).await?;
    Ok(([(header::SET_COOKIE, cookie)], Json(user)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    api_key: String,
    #[serde(default)]
    hister_token: Option<String>,
}

/// Replaces the stored upstream keys for the signed-in account. The cookie is the
/// authorization here, so no password is resent.
async fn update_keys(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Keys>,
) -> ApiResult<Json<Value>> {
    let account = app.account(&headers)?;
    let (key, hister, user) = app
        .check_keys(&body.api_key, body.hister_token.as_deref())
        .await?;
    let username = account.username.clone();
    app.update_vault(move |vault| {
        let account = vault
            .accounts
            .iter_mut()
            .find(|account| account.username == username)
            .ok_or(UNAUTHENTICATED)?;
        account.key = key;
        account.hister = hister;
        Ok(())
    })
    .await?;
    Ok(Json(user))
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = session_id(&headers) {
        let token = token.to_owned();
        app.update_vault(move |vault| {
            vault.sessions.retain(|(existing, _, _)| existing != &token);
            Ok(())
        })
        .await?;
    }
    Ok((
        [(header::SET_COOKIE, app.cookie("", 0))],
        Json(json!({ "ok": true })),
    )
        .into_response())
}

async fn me(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let account = app.account(&headers)?;
    let mut user = app
        .request(&account.key, Method::GET, "users/me", &[], None)
        .await?;
    user["histerConnected"] = json!(app.hister.is_some() && account.hister.is_some());
    Ok(Json(user))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HisterSearch {
    q: String,
    cursor: Option<String>,
}

async fn search_hister(
    State(app): State<App>,
    headers: HeaderMap,
    Query(search): Query<HisterSearch>,
) -> ApiResult<Json<Value>> {
    let token = app.hister_key(&headers)?;
    if search.q.trim().is_empty() || search.q.len() > 2048 {
        return Err(invalid("Search query must be 1-2048 bytes"));
    }
    if search
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.len() > 4096)
    {
        return Err(invalid("Cursor too long"));
    }
    let base = app
        .hister
        .as_ref()
        .ok_or(ApiError(StatusCode::NOT_FOUND, "Hister is not configured"))?;
    let query = json!({
        "text": search.q,
        "page_key": search.cursor.unwrap_or_default(),
        "limit": 40,
        "include_html": false,
    });
    let data = app
        .call(
            base,
            &token,
            Method::GET,
            "search",
            &[
                ("format".into(), "json".into()),
                ("query".into(), query.to_string()),
            ],
            None,
        )
        .await
        .map_err(|error| {
            if matches!(error.0, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                ApiError(
                    StatusCode::BAD_GATEWAY,
                    "Hister rejected its token. Reconnect in Customize > Connected accounts.",
                )
            } else {
                error
            }
        })?;
    if data
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|error| !error.is_empty())
    {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "Hister could not complete this search",
        ));
    }
    let documents = match data.get("documents") {
        Some(Value::Array(documents)) => documents.as_slice(),
        Some(Value::Null) => &[],
        _ => {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "Invalid search response from Hister",
            ));
        }
    };
    let documents: Vec<Value> = documents.iter().map(|document| json!({
        "url": document.get("url").and_then(Value::as_str).unwrap_or_default(),
        "title": document.get("title").and_then(Value::as_str).unwrap_or_default(),
        "text": document.get("text").and_then(Value::as_str).unwrap_or_default().chars().take(500).collect::<String>(),
    })).collect();
    Ok(Json(
        json!({ "documents": documents, "nextCursor": data.get("page_key").and_then(Value::as_str).filter(|cursor| !cursor.is_empty()) }),
    ))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Browse {
    q: Option<String>,
    tag: Option<String>,
    list: Option<String>,
    favourited: Option<bool>,
    cursor: Option<String>,
}

fn identifier(value: &str) -> ApiResult<&str> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(invalid("Invalid resource ID"));
    }
    Ok(value)
}

fn bookmark_query(browse: Browse) -> ApiResult<(String, Vec<(String, String)>)> {
    let mut query = vec![
        ("limit".into(), "40".into()),
        ("includeContent".into(), "false".into()),
    ];
    if let Some(cursor) = browse.cursor {
        if cursor.len() > 4096 {
            return Err(invalid("Cursor too long"));
        }
        query.push(("cursor".into(), cursor));
    }
    let text = browse.q.unwrap_or_default();
    if text.len() > 2048 {
        return Err(invalid("Search query too long"));
    }
    let path = if !text.trim().is_empty() {
        if browse.tag.is_some() || browse.list.is_some() || browse.favourited.is_some() {
            return Err(invalid(
                "Search cannot be combined with a collection filter",
            ));
        }
        query.push(("q".into(), text));
        "bookmarks/search".to_owned()
    } else if let Some(list) = browse.list {
        format!("lists/{}/bookmarks", identifier(&list)?)
    } else if let Some(tag) = browse.tag {
        format!("tags/{}/bookmarks", identifier(&tag)?)
    } else {
        if let Some(favourited) = browse.favourited {
            query.push(("favourited".into(), favourited.to_string()));
        }
        "bookmarks".to_owned()
    };
    Ok((path, query))
}

async fn bookmarks(
    State(app): State<App>,
    headers: HeaderMap,
    Query(browse): Query<Browse>,
) -> ApiResult<Json<Value>> {
    let key = app.key(&headers)?;
    let (path, query) = bookmark_query(browse)?;
    app.request(&key, Method::GET, &path, &query, None)
        .await
        .map_err(|error| {
            if matches!(error.0, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                ApiError(
                    StatusCode::BAD_GATEWAY,
                    "Karakeep rejected its key or search permissions. Reconnect in Customize > Connected accounts.",
                )
            } else {
                error
            }
        })
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TagPage {
    cursor: Option<String>,
}

async fn tags(
    State(app): State<App>,
    headers: HeaderMap,
    Query(page): Query<TagPage>,
) -> ApiResult<Json<Value>> {
    let key = app.key(&headers)?;
    let mut query = vec![("limit".into(), "100".into())];
    if let Some(cursor) = page.cursor {
        if cursor.len() > 4096 {
            return Err(invalid("Cursor too long"));
        }
        query.push(("cursor".into(), cursor));
    }
    app.request(&key, Method::GET, "tags", &query, None)
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    /// true: pages you navigated to. false/absent: pages Hister indexed.
    opened: Option<bool>,
    last: Option<String>,
    last_id: Option<u64>,
    last_updated_at: Option<String>,
}

async fn history(
    State(app): State<App>,
    headers: HeaderMap,
    Query(page): Query<History>,
) -> ApiResult<Json<Value>> {
    let token = app.hister_key(&headers)?;
    let base = app.hister.clone().ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "This server is not configured for Hister. Set HISTER_URL.",
    ))?;
    let mut query = Vec::new();
    if page.opened.unwrap_or(false) {
        query.push(("opened".into(), "true".into()));
        if let Some(id) = page.last_id {
            query.push(("last_id".into(), id.to_string()));
        }
        if let Some(updated) = page.last_updated_at {
            if updated.len() > 64 {
                return Err(invalid("Cursor too long"));
            }
            query.push(("last_updated_at".into(), updated));
        }
    } else if let Some(last) = page.last {
        if last.len() > 4096 {
            return Err(invalid("Cursor too long"));
        }
        query.push(("last".into(), last));
    }
    app.call(&base, &token, Method::GET, "api/history", &query, None)
        .await
        .map(Json)
}

async fn lists(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let key = app.key(&headers)?;
    app.request(&key, Method::GET, "lists", &[], None)
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    favourited: bool,
}

async fn pin(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(pin): Json<Pin>,
) -> ApiResult<Json<Value>> {
    let key = app.key(&headers)?;
    let path = format!("bookmarks/{}", identifier(&id)?);
    app.request(
        &key,
        Method::PATCH,
        &path,
        &[],
        Some(json!({ "favourited": pin.favourited })),
    )
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TagEdit {
    name: String,
}

async fn edit_tag(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
    Json(edit): Json<TagEdit>,
) -> ApiResult<Json<Value>> {
    let key = app.key(&headers)?;
    let name = edit.name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return Err(invalid(
            "Tag must be 1-100 bytes without control characters",
        ));
    }
    let path = format!("bookmarks/{}/tags", identifier(&id)?);
    app.request(
        &key,
        method,
        &path,
        &[],
        Some(json!({ "tags": [{ "tagName": name }] })),
    )
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBookmark {
    url: String,
    title: String,
}

async fn add_bookmark(
    State(app): State<App>,
    headers: HeaderMap,
    Json(bookmark): Json<NewBookmark>,
) -> ApiResult<Json<Value>> {
    let key = app.key(&headers)?;
    if bookmark.url.len() > 8192 || bookmark.title.len() > 500 {
        return Err(invalid("URL or title too long"));
    }
    http_url(&bookmark.url).map_err(invalid)?;
    app.request(
        &key,
        Method::POST,
        "bookmarks",
        &[],
        Some(json!({ "type": "link", "url": bookmark.url, "title": bookmark.title })),
    )
    .await
    .map(Json)
}

// Browsers fetch this once to offer Karasearch as a search engine; the Url template must be
// absolute, so it is built from the request Host.
async fn config(State(app): State<App>) -> ApiResult<Json<Value>> {
    let claimed = !app
        .vault
        .lock()
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Session store unavailable",
            )
        })?
        .accounts
        .is_empty();
    Ok(Json(json!({
        "searchUrl": app.search_url,
        "karakeepUrl": app.upstream.join("../../").ok().map(|url| url.to_string()),
        "histerUrl": app.hister.as_ref().map(Url::as_str),
        "hister": app.hister.is_some(),
        // The form offers registration only when it would actually be accepted.
        "canRegister": !claimed || app.open_registration,
        // Distinguishes "first run" from "shared server": the first defaults to
        // the register form, the second to sign-in with a link to register.
        "openRegistration": app.open_registration,
    })))
}

async fn opensearch(State(app): State<App>, headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 255
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-.:[]".contains(c))
        })
        .unwrap_or("localhost");
    let scheme = if app.secure_cookie { "https" } else { "http" };
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">
  <ShortName>Karasearch</ShortName>
  <Description>Search your Karakeep library</Description>
  <InputEncoding>UTF-8</InputEncoding>
  <Url type="text/html" method="get" template="{scheme}://{host}/?q={{searchTerms}}"/>
</OpenSearchDescription>
"#
    );
    (
        [(
            header::CONTENT_TYPE,
            "application/opensearchdescription+xml",
        )],
        body,
    )
        .into_response()
}

fn router(app: App) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route(
            "/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../static/app.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/app.js"),
                )
            }),
        )
        .route(
            "/theme.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/theme.js"),
                )
            }),
        )
        .route("/opensearch.xml", get(opensearch))
        .route("/api/config", get(config))
        // Liveness only: no auth, no session counts, and no upstream call, so a Karakeep
        // outage never restarts a healthy Karasearch.
        .route(
            "/healthz",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/keys", post(update_keys))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/bookmarks", get(bookmarks).post(add_bookmark))
        .route("/api/bookmarks/{id}", patch(pin))
        .route("/api/bookmarks/{id}/tags", post(edit_tag).delete(edit_tag))
        .route("/api/tags", get(tags))
        .route("/api/lists", get(lists))
        .route("/api/history", get(history))
        .route("/api/hister/search", get(search_hister))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(security))
        .with_state(app)
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        // Display, not Debug: operators read this in `docker logs`.
        eprintln!("karasearch: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let address = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    // `karasearch healthcheck` keeps the runtime image free of curl/wget.
    if env::args().nth(1).as_deref() == Some("healthcheck") {
        let target = address
            .replace("0.0.0.0", "127.0.0.1")
            .replace("[::]", "[::1]");
        let response = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?
            .get(format!("http://{target}/healthz"))
            .send()
            .await?;
        return if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("health check returned {}", response.status()).into())
        };
    }
    let upstream = env::var("KARAKEEP_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let mut upstream =
        http_url(&upstream).map_err(|error| setting("KARAKEEP_URL", &upstream, error))?;
    if upstream.query().is_some() || upstream.fragment().is_some() {
        return Err(setting(
            "KARAKEEP_URL",
            upstream.as_str(),
            "Remove the query or fragment",
        )
        .into());
    }
    upstream.set_path(&format!(
        "{}/api/v1/",
        upstream.path().trim_end_matches('/')
    ));
    // Optional: without HISTER_URL the app behaves exactly as before.
    let hister = match env::var("HISTER_URL").ok().filter(|v| !v.is_empty()) {
        Some(value) => {
            let mut url = http_url(&value).map_err(|error| setting("HISTER_URL", &value, error))?;
            if url.query().is_some() || url.fragment().is_some() {
                return Err(
                    setting("HISTER_URL", url.as_str(), "Remove the query or fragment").into(),
                );
            }
            // Trailing slash keeps Url::join appending rather than replacing the base path.
            url.set_path(&format!("{}/", url.path().trim_end_matches('/')));
            Some(url)
        }
        None => None,
    };
    let search_url = env::var("SEARCH_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "http://localhost:8080/search?q={query}&t=general".into());
    let checked_search = http_url(&search_url.replace("{query}", "karasearchquerymarker"))
        .map_err(|error| setting("SEARCH_URL", &search_url, error))?;
    if !search_url.contains("{query}")
        || search_url.len() > 2048
        || checked_search
            .host_str()
            .is_some_and(|host| host.contains("karasearchquerymarker"))
    {
        return Err(setting(
            "SEARCH_URL",
            &search_url,
            "Use at most 2048 bytes and put {query} in the path or query, not the hostname",
        )
        .into());
    }
    let secure_cookie = match env::var("COOKIE_SECURE").as_deref() {
        Ok("true") => true,
        Ok("false") | Err(env::VarError::NotPresent) => false,
        _ => return Err("COOKIE_SECURE must be true or false".into()),
    };
    let open_registration = match env::var("ALLOW_REGISTRATION").as_deref() {
        Ok("true") => true,
        Ok("false") | Err(env::VarError::NotPresent) => false,
        _ => return Err("ALLOW_REGISTRATION must be true or false".into()),
    };
    let store = match env::var("SESSION_STORE").ok().filter(|p| !p.is_empty()) {
        Some(path) => {
            let secret = env::var("SESSION_SECRET")
                .map_err(|_| "SESSION_STORE needs SESSION_SECRET set to a random value")?;
            Some(Arc::new(Store::new(PathBuf::from(path), &secret)?))
        }
        None => None,
    };
    let vault = match &store {
        Some(store) => {
            let vault = store.load()?;
            store.save(&vault)?;
            vault
        }
        None => Vault::default(),
    };
    if !vault.accounts.is_empty() || !vault.sessions.is_empty() {
        println!(
            "Karasearch restored {} account(s) and {} session(s)",
            vault.accounts.len(),
            vault.sessions.len()
        );
    }
    if store.is_none() {
        println!(
            "Karasearch has no SESSION_STORE: accounts live in memory and are lost on restart"
        );
    }
    let app = App {
        client: Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        upstream,
        hister,
        search_url,
        secure_cookie,
        open_registration,
        vault: Arc::new(Mutex::new(vault)),
        store,
    };
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("Karasearch listening on {address}");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
