use std::{
    collections::HashMap,
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
    hkdf,
    rand::{SecureRandom, SystemRandom},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const SESSION_SECONDS: u64 = 43_200;
const NONCE_LEN: usize = 12;

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
        hkdf::Salt::new(hkdf::HKDF_SHA256, b"karasearch session store v1")
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

    fn load(&self) -> Result<HashMap<String, Session>, String> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
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
        let records: Vec<(String, String, u64)> =
            serde_json::from_slice(plain).map_err(|_| "Session store is corrupt".to_owned())?;
        if records.len() > 1024 {
            return Err("Session store exceeds session capacity".into());
        }
        Ok(records
            .into_iter()
            .filter(|(_, _, expires)| *expires > now())
            .map(|(token, key, expires)| (token, Session { key, expires }))
            .collect())
    }

    fn save(&self, sessions: &HashMap<String, Session>) -> Result<(), String> {
        let records: Vec<(&String, &String, u64)> = sessions
            .iter()
            .map(|(token, session)| (token, &session.key, session.expires))
            .collect();
        let mut buffer = serde_json::to_vec(&records).map_err(|_| "Could not encode sessions")?;
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
    search_url: String,
    secure_cookie: bool,
    // ponytail: single-process store; add Redis/Postgres only when running replicas.
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    store: Option<Arc<Store>>,
}

#[derive(Clone)]
struct Session {
    key: String,
    expires: u64,
}

struct ApiError(StatusCode, &'static str);
type ApiResult<T> = Result<T, ApiError>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

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
    fn key(&self, headers: &HeaderMap) -> ApiResult<String> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Session store unavailable",
            )
        })?;
        sessions.retain(|_, session| session.expires > now());
        session_id(headers)
            .and_then(|id| sessions.get(id))
            .map(|session| session.key.clone())
            .ok_or(ApiError(
                StatusCode::UNAUTHORIZED,
                "Sign in with your Karakeep API key",
            ))
    }

    async fn update_sessions(
        &self,
        change: impl FnOnce(&mut HashMap<String, Session>) -> ApiResult<()> + Send + 'static,
    ) -> ApiResult<()> {
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut sessions = app.sessions.lock().map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Session store unavailable",
                )
            })?;
            let mut updated = sessions.clone();
            updated.retain(|_, session| session.expires > now());
            change(&mut updated)?;
            if let Some(store) = &app.store {
                store.save(&updated).map_err(|error| {
                    eprintln!("karasearch: {error}");
                    ApiError(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not save session changes; retry",
                    )
                })?;
            }
            *sessions = updated;
            Ok(())
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
        let url = self
            .upstream
            .join(path)
            .map_err(|_| invalid("Invalid API path"))?;
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(key)
            .query(query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                "Cannot reach Karakeep. Check KARAKEEP_URL and container connectivity.",
            )
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status {
                StatusCode::UNAUTHORIZED => {
                    ApiError(status, "Karakeep rejected this API key. Sign in again.")
                }
                StatusCode::FORBIDDEN => {
                    ApiError(status, "API key lacks permission for this action.")
                }
                StatusCode::NOT_FOUND => ApiError(status, "Karakeep resource not found."),
                StatusCode::TOO_MANY_REQUESTS => {
                    ApiError(status, "Karakeep rate limit reached. Try again shortly.")
                }
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => ApiError(
                    StatusCode::BAD_REQUEST,
                    "Karakeep rejected this request. Check your query or input.",
                ),
                _ => ApiError(
                    StatusCode::BAD_GATEWAY,
                    "Karakeep could not complete this request.",
                ),
            });
        }
        response.json().await.map_err(|_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                "Invalid JSON response from Karakeep",
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
struct Login {
    api_key: String,
}

async fn login(
    State(app): State<App>,
    headers: HeaderMap,
    Json(login): Json<Login>,
) -> ApiResult<Response> {
    let key = login.api_key.trim();
    if key.is_empty()
        || key.len() > 4096
        || !key.is_ascii()
        || key.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid("Enter a valid Karakeep API key"));
    }
    let user = app.request(key, Method::GET, "users/me", &[], None).await?;
    if user.get("id").and_then(Value::as_str).is_none() {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "Karakeep returned no user ID",
        ));
    }
    let token = Uuid::new_v4().to_string();
    let cookie = app.cookie(&token, SESSION_SECONDS);
    let previous = session_id(&headers).map(str::to_owned);
    let key = key.to_owned();
    app.update_sessions(move |sessions| {
        if let Some(previous) = previous {
            sessions.remove(&previous);
        }
        if sessions.len() >= 1024 {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Session capacity reached",
            ));
        }
        sessions.insert(
            token,
            Session {
                key,
                expires: now() + SESSION_SECONDS,
            },
        );
        Ok(())
    })
    .await?;
    Ok(([(header::SET_COOKIE, cookie)], Json(user)).into_response())
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = session_id(&headers) {
        let token = token.to_owned();
        app.update_sessions(move |sessions| {
            sessions.remove(&token);
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
    let key = app.key(&headers)?;
    app.request(&key, Method::GET, "users/me", &[], None)
        .await
        .map(Json)
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

fn router(app: App) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../static/app.css")) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../static/app.js")) }))
        .route("/api/config", get(|State(app): State<App>| async move { Json(json!({ "searchUrl": app.search_url })) }))
        // Liveness only: no auth, no session counts, and no upstream call, so a Karakeep
        // outage never restarts a healthy Karasearch.
        .route("/healthz", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/bookmarks", get(bookmarks).post(add_bookmark))
        .route("/api/bookmarks/{id}", patch(pin))
        .route("/api/bookmarks/{id}/tags", post(edit_tag).delete(edit_tag))
        .route("/api/tags", get(tags))
        .route("/api/lists", get(lists))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(security))
        .with_state(app)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let mut upstream = http_url(&upstream)?;
    if upstream.query().is_some() || upstream.fragment().is_some() {
        return Err("KARAKEEP_URL must not contain a query or fragment".into());
    }
    upstream.set_path(&format!(
        "{}/api/v1/",
        upstream.path().trim_end_matches('/')
    ));
    let search_url = env::var("SEARCH_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "http://localhost:8080/search?q={query}&t=general".into());
    let checked_search = http_url(&search_url.replace("{query}", "karasearchquerymarker"))?;
    if !search_url.contains("{query}")
        || search_url.len() > 2048
        || checked_search
            .host_str()
            .is_some_and(|host| host.contains("karasearchquerymarker"))
    {
        return Err("SEARCH_URL must be at most 2048 bytes and contain {query} in its path or query, not its hostname".into());
    }
    let secure_cookie = match env::var("COOKIE_SECURE").as_deref() {
        Ok("true") => true,
        Ok("false") | Err(env::VarError::NotPresent) => false,
        _ => return Err("COOKIE_SECURE must be true or false".into()),
    };
    let store = match env::var("SESSION_STORE").ok().filter(|p| !p.is_empty()) {
        Some(path) => {
            let secret = env::var("SESSION_SECRET")
                .map_err(|_| "SESSION_STORE needs SESSION_SECRET set to a random value")?;
            Some(Arc::new(Store::new(PathBuf::from(path), &secret)?))
        }
        None => None,
    };
    let sessions = match &store {
        Some(store) => {
            let sessions = store.load()?;
            store.save(&sessions)?;
            sessions
        }
        None => HashMap::new(),
    };
    if !sessions.is_empty() {
        println!("Karasearch restored {} session(s)", sessions.len());
    }
    let app = App {
        client: Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        upstream,
        search_url,
        secure_cookie,
        sessions: Arc::new(Mutex::new(sessions)),
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
