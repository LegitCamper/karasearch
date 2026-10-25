use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const SESSION_SECONDS: u64 = 43_200;

#[derive(Clone)]
struct App {
    client: Client,
    upstream: Url,
    search_url: String,
    secure_cookie: bool,
    // ponytail: in-memory sessions; add a shared store only for restart persistence or replicas.
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

struct Session {
    key: String,
    expires: Instant,
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
        sessions.retain(|_, session| session.expires > Instant::now());
        session_id(headers)
            .and_then(|id| sessions.get(id))
            .map(|session| session.key.clone())
            .ok_or(ApiError(
                StatusCode::UNAUTHORIZED,
                "Sign in with your Karakeep API key",
            ))
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
    let mut sessions = app.sessions.lock().map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Session store unavailable",
        )
    })?;
    sessions.retain(|_, session| session.expires > Instant::now());
    if let Some(previous) = session_id(&headers) {
        sessions.remove(previous);
    }
    if sessions.len() >= 1024 {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Session capacity reached",
        ));
    }
    sessions.insert(
        token.clone(),
        Session {
            key: key.to_owned(),
            expires: Instant::now() + Duration::from_secs(SESSION_SECONDS),
        },
    );
    Ok((
        [(header::SET_COOKIE, app.cookie(&token, SESSION_SECONDS))],
        Json(user),
    )
        .into_response())
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = session_id(&headers) {
        app.sessions
            .lock()
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Session store unavailable",
                )
            })?
            .remove(token);
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
    let app = App {
        client: Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        upstream,
        search_url,
        secure_cookie,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    let address = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
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
