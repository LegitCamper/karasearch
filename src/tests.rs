use super::*;
use axum::body::{Body, to_bytes};
use std::collections::HashMap;
use tower::ServiceExt;

#[tokio::test]
async fn hister_search_uses_account_token_and_preserves_query_and_cursor() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let captured = observed.clone();
    let mock = Router::new()
        .route("/api/v1/users/me", get(|| async { Json(json!({"id": "reader"})) }))
        .route("/api/v1/bookmarks/search", get(|| async { StatusCode::UNAUTHORIZED }))
        .route("/hister/search", get(move |headers: HeaderMap, Query(params): Query<HashMap<String, String>>| {
            let captured = captured.clone();
            async move {
                assert_eq!(params["format"], "json");
                let query: Value = serde_json::from_str(&params["query"]).unwrap();
                captured.lock().unwrap().push(query.clone());
                match query["text"].as_str().unwrap() {
                    "denied" => return StatusCode::UNAUTHORIZED.into_response(),
                    "down" => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    "malformed" => return Json(json!({"wrong": []})).into_response(),
                    "error" => return Json(json!({"error":"internal upstream detail", "documents":[]})).into_response(),
                    "empty" => return Json(json!({"documents":null,"page_key":""})).into_response(),
                    _ => (),
                }
                Json(json!({
                    "documents": [{"url":"https://example.com/history", "title":headers[header::AUTHORIZATION].to_str().unwrap(), "text":"<script>untrusted</script>", "html":"must not forward", "metadata":{"private":true}}],
                    "page_key":"next/+&=?",
                })).into_response()
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let mut state = app(&format!("http://{address}/api/v1/"));
    state.hister = Some(Url::parse(&format!("http://{address}/hister/")).unwrap());
    for name in ["alice", "bob"] {
        let account = Account::new(
            name.into(),
            "fixture-password",
            "karakeep-fixture".into(),
            Some(format!("hister-{name}")),
        )
        .unwrap();
        let mut vault = state.vault.lock().unwrap();
        vault.accounts.push(account);
        vault.sessions.push((name.into(), name.into(), now() + 600));
    }
    let query_text = "rust & web/#? + 日本語";
    let cursor = "cursor/+&=?";
    let mut url = Url::parse("http://local/api/hister/search").unwrap();
    url.query_pairs_mut()
        .append_pair("q", query_text)
        .append_pair("cursor", cursor);
    let path = format!("{}?{}", url.path(), url.query().unwrap());
    let denied = router(state.clone())
        .oneshot(request(
            "GET",
            "/api/bookmarks?q=rust",
            "karasearch_session=alice",
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(
        denied.status(),
        StatusCode::BAD_GATEWAY,
        "upstream auth failure is not dashboard logout"
    );
    for name in ["alice", "bob"] {
        let cookie = format!("karasearch_session={name}");
        let response = router(state.clone())
            .oneshot(request("GET", &path, &cookie, Value::Null))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let data = json_body(response).await;
        assert_eq!(
            data["documents"][0]["title"],
            format!("Bearer hister-{name}")
        );
        assert_eq!(data["nextCursor"], "next/+&=?");
        assert!(data["documents"][0].get("html").is_none());
        assert!(data["documents"][0].get("metadata").is_none());
        let response = router(state.clone())
            .oneshot(request("GET", "/api/me", &cookie, Value::Null))
            .await
            .unwrap();
        assert_eq!(json_body(response).await["histerConnected"], true);
    }
    assert_eq!(
        observed.lock().unwrap()[0],
        json!({"text":query_text,"page_key":cursor,"limit":40,"include_html":false})
    );
    for path in [
        "/api/hister/search?q=",
        "/api/hister/search?q=%20",
        "/api/hister/search?q=x&extra=bad",
    ] {
        let response = router(state.clone())
            .oneshot(request(
                "GET",
                path,
                "karasearch_session=alice",
                Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for path in [
        format!("/api/hister/search?q={}", "x".repeat(2049)),
        format!("/api/hister/search?q=x&cursor={}", "x".repeat(4097)),
    ] {
        let response = router(state.clone())
            .oneshot(request(
                "GET",
                &path,
                "karasearch_session=alice",
                Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        observed.lock().unwrap().len(),
        2,
        "invalid requests must not reach upstream"
    );
    for query in ["denied", "down", "malformed", "error"] {
        let response = router(state.clone())
            .oneshot(request(
                "GET",
                &format!("/api/hister/search?q={query}"),
                "karasearch_session=alice",
                Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }
    let response = router(state.clone())
        .oneshot(request(
            "GET",
            "/api/hister/search?q=empty",
            "karasearch_session=alice",
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(
        json_body(response).await,
        json!({"documents":[],"nextCursor":null})
    );
    let response = router(state.clone())
        .oneshot(request("GET", &path, "", Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    state.vault.lock().unwrap().accounts[0].hister = None;
    let response = router(state.clone())
        .oneshot(request(
            "GET",
            &path,
            "karasearch_session=alice",
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    state.hister = None;
    let response = router(state)
        .oneshot(request("GET", &path, "karasearch_session=bob", Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    server.abort();
}

fn app(upstream: &str) -> App {
    App {
        client: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        upstream: Url::parse(upstream).unwrap(),
        hister: None,
        search_url: "http://localhost:8080/search?q={query}".into(),
        secure_cookie: false,
        open_registration: true,
        vault: Arc::new(Mutex::new(Vault::default())),
        store: None,
    }
}

fn request(method: &str, path: &str, cookie: &str, body: Value) -> Request {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-karasearch-request", "1")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn json_body(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap()
}

#[test]
fn reject_unsafe_urls_and_path_traversal() {
    for value in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "https://user:pass@example.com",
        "/relative",
    ] {
        assert!(http_url(value).is_err(), "accepted {value}");
    }
    assert!(http_url("http://karakeep:3000").is_ok());
    for value in ["..", "foo/bar", "foo?query", "", "%2e%2e"] {
        assert!(identifier(value).is_err());
    }
    assert!(identifier("bookmark-123_abc").is_ok());
}

#[test]
fn encrypted_store_survives_restart_and_rejects_a_changed_secret() {
    let path = env::temp_dir().join(format!("karasearch-test-{}.bin", Uuid::new_v4()));
    let secret = "0123456789abcdef0123456789abcdef";
    assert!(Store::new(path.clone(), "too-short").is_err());
    let store = Store::new(path.clone(), secret).unwrap();
    let empty = store.load().unwrap();
    assert!(
        empty.accounts.is_empty() && empty.sessions.is_empty(),
        "missing file is not an error"
    );

    let vault = Vault {
        accounts: vec![
            Account::new(
                "reader".to_owned(),
                "correct horse battery",
                "secret-api-key".to_owned(),
                Some("secret-hister-token".to_owned()),
            )
            .unwrap(),
        ],
        sessions: vec![
            ("token-live".to_owned(), "reader".to_owned(), now() + 600),
            ("token-stale".to_owned(), "reader".to_owned(), now() - 1),
            // A session whose account is gone must not survive a reload.
            ("token-orphan".to_owned(), "deleted".to_owned(), now() + 600),
        ],
    };
    store.save(&vault).unwrap();

    let sealed = std::fs::read(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    store.save(&vault).unwrap();
    assert_ne!(
        sealed,
        std::fs::read(&path).unwrap(),
        "each save uses a fresh nonce"
    );
    for secret_value in [
        "secret-api-key",
        "secret-hister-token",
        "correct horse battery",
    ] {
        assert!(
            !String::from_utf8_lossy(&sealed).contains(secret_value),
            "{secret_value} must not be readable on disk"
        );
    }

    let restored = Store::new(path.clone(), secret).unwrap().load().unwrap();
    assert_eq!(
        restored.sessions.len(),
        1,
        "expired and orphaned sessions are dropped on load"
    );
    assert_eq!(restored.sessions[0].0, "token-live");
    assert_eq!(restored.accounts[0].key, "secret-api-key");
    assert!(restored.accounts[0].verify("correct horse battery"));
    assert!(!restored.accounts[0].verify("wrong password"));

    let other = Store::new(path.clone(), "fedcba9876543210fedcba9876543210").unwrap();
    assert!(other.load().is_err(), "a different secret must not decrypt");

    let mut tampered = sealed.clone();
    *tampered.last_mut().unwrap() ^= 1;
    std::fs::write(&path, &tampered).unwrap();
    assert!(
        Store::new(path.clone(), secret).unwrap().load().is_err(),
        "tampering must fail authentication"
    );
    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn persistent_accounts_keep_users_separate_and_logout_survives_restart() {
    let mock = Router::new().route(
        "/api/v1/users/me",
        get(|headers: HeaderMap| async move {
            Json(json!({"id": headers[header::AUTHORIZATION].to_str().unwrap()}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let path = env::temp_dir().join(format!("karasearch-test-{}.enc", Uuid::new_v4()));
    let secret = "0123456789abcdef0123456789abcdef";
    let store = Arc::new(Store::new(path.clone(), secret).unwrap());
    let mut state = app(&format!("http://{address}/api/v1/"));
    state.store = Some(store.clone());
    let mut cookies = Vec::new();
    for (name, key) in [("alice", "alice-key"), ("bob", "bob-key")] {
        let response = router(state.clone())
            .oneshot(request(
                "POST",
                "/api/register",
                "",
                json!({"username": name, "password": "a-long-test-password", "api_key": key}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        cookies.push(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .to_owned(),
        );
    }
    let before = store.load().unwrap();
    state.vault = Arc::new(Mutex::new(store.load().unwrap()));
    // Each cookie still resolves to its own account's stored key.
    for (cookie, key) in cookies.iter().zip(["alice-key", "bob-key"]) {
        let response = router(state.clone())
            .oneshot(request("GET", "/api/me", cookie, Value::Null))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["id"], format!("Bearer {key}"));
    }
    for (token, _, expires) in state.vault.lock().unwrap().sessions.iter() {
        let original = before
            .sessions
            .iter()
            .find(|(existing, _, _)| existing == token)
            .unwrap();
        assert_eq!(*expires, original.2, "restart must not extend expiry");
    }
    // An unwritable store must fail the request instead of reporting success.
    let mut failing = state.clone();
    failing.store = Some(Arc::new(
        Store::new(path.join("missing/sessions.enc"), secret).unwrap(),
    ));
    for (route, body) in [
        ("logout", Value::Null),
        (
            "login",
            json!({"username": "alice", "password": "a-long-test-password"}),
        ),
    ] {
        let response = router(failing.clone())
            .oneshot(request("POST", &format!("/api/{route}"), &cookies[0], body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(state.vault.lock().unwrap().sessions.len(), 2);
        assert_eq!(store.load().unwrap().sessions.len(), 2);
    }
    let response = router(state.clone())
        .oneshot(request("POST", "/api/logout", &cookies[0], Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    state.vault = Arc::new(Mutex::new(store.load().unwrap()));
    // Logout removed the session but kept the account and its stored keys.
    assert_eq!(state.vault.lock().unwrap().accounts.len(), 2);
    let response = router(state.clone())
        .oneshot(request("GET", "/api/me", &cookies[0], Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router(state.clone())
        .oneshot(request("GET", "/api/me", &cookies[1], Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Signing back in needs only the password; the key stays server-side.
    let response = router(state.clone())
        .oneshot(request(
            "POST",
            "/api/login",
            "",
            json!({"username": "alice", "password": "a-long-test-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    let response = router(state.clone())
        .oneshot(request("GET", "/api/me", &cookie, Value::Null))
        .await
        .unwrap();
    assert_eq!(json_body(response).await["id"], "Bearer alice-key");
    fs::remove_file(path).unwrap();
    server.abort();
}

#[test]
fn setting_errors_name_the_variable_and_the_rejected_value() {
    let message = setting("KARAKEEP_URL", "karakeep:3000", "Expected an HTTP(S) URL");
    assert!(message.contains("KARAKEEP_URL"), "{message}");
    assert!(message.contains("karakeep:3000"), "{message}");
    assert!(message.contains("http://host:3000"), "{message}");
    assert!(
        setting("SEARCH_URL", "bad", "Bad").contains("{query}"),
        "SEARCH_URL example must show the placeholder"
    );
}

#[tokio::test]
async fn health_endpoint_needs_no_session_and_no_upstream() {
    // Upstream intentionally unreachable: liveness must not depend on Karakeep.
    let router = router(app("http://127.0.0.1:1/api/v1/"));
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["status"], "ok");

    let private = router
        .oneshot(
            Request::builder()
                .uri("/api/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(private.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn routes_collections_to_upstream_paths() {
    let (path, _) = bookmark_query(Browse {
        list: Some("list-read".into()),
        ..Browse::default()
    })
    .unwrap_or_else(|_| panic!("valid list rejected"));
    assert_eq!(path, "lists/list-read/bookmarks");
    assert!(
        bookmark_query(Browse {
            list: Some("../secrets".into()),
            ..Browse::default()
        })
        .is_err()
    );
    assert!(
        bookmark_query(Browse {
            q: Some("rust".into()),
            list: Some("list-read".into()),
            ..Browse::default()
        })
        .is_err()
    );
}

#[test]
fn routes_queries_without_changing_search_text() {
    let (path, query) = bookmark_query(Browse {
        q: Some("rust & tag:\"read later\"".into()),
        cursor: Some("40".into()),
        ..Browse::default()
    })
    .unwrap_or_else(|_| panic!("valid query rejected"));
    assert_eq!(path, "bookmarks/search");
    assert!(query.contains(&("q".into(), "rust & tag:\"read later\"".into())));
    assert!(query.contains(&("cursor".into(), "40".into())));
    let (path, _) = bookmark_query(Browse {
        tag: Some("tag123".into()),
        ..Browse::default()
    })
    .unwrap_or_else(|_| panic!("valid tag rejected"));
    assert_eq!(path, "tags/tag123/bookmarks");
    assert!(
        bookmark_query(Browse {
            q: Some("query".into()),
            tag: Some("tag".into()),
            ..Browse::default()
        })
        .is_err()
    );
}

#[tokio::test]
async fn protects_private_routes_and_mutations() {
    let router = router(app("http://127.0.0.1:1/api/v1/"));
    let response = router
        .clone()
        .oneshot(request("GET", "/api/bookmarks", "", Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    let mut missing_header = request(
        "POST",
        "/api/login",
        "",
        json!({"username":"reader","password":"a-long-test-password"}),
    );
    missing_header.headers_mut().remove("x-karasearch-request");
    assert_eq!(
        router
            .clone()
            .oneshot(missing_header)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut cross_site = request(
        "POST",
        "/api/login",
        "",
        json!({"username":"reader","password":"a-long-test-password"}),
    );
    cross_site
        .headers_mut()
        .insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
    assert_eq!(
        router.oneshot(cross_site).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn expired_session_is_removed() {
    let state = app("http://127.0.0.1:1/api/v1/");
    {
        let mut vault = state.vault.lock().unwrap();
        vault.accounts.push(
            Account::new(
                "reader".into(),
                "a-long-test-password",
                "secret".into(),
                None,
            )
            .unwrap(),
        );
        vault
            .sessions
            .push(("expired".into(), "reader".into(), now() - 1));
    }
    let response = router(state.clone())
        .oneshot(request(
            "GET",
            "/api/me",
            "other=value; karasearch_session=expired",
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // The expired session is dropped; the account it pointed at is not.
    assert!(state.vault.lock().unwrap().sessions.is_empty());
    assert_eq!(state.vault.lock().unwrap().accounts.len(), 1);
}

#[tokio::test]
async fn validates_credentials_and_reports_unreachable_upstream() {
    let router = router(app("http://127.0.0.1:1/api/v1/"));
    // Bad usernames, short passwords, and unusable keys are all caught locally.
    for body in [
        json!({"username":"reader","password":"a-long-test-password","api_key":"\n"}),
        json!({"username":"","password":"a-long-test-password","api_key":"key"}),
        json!({"username":"bad name!","password":"a-long-test-password","api_key":"key"}),
        json!({"username":"reader","password":"short","api_key":"key"}),
        json!({"username":"reader","password":"a-long-test-password"}),
    ] {
        let response = router
            .clone()
            .oneshot(request("POST", "/api/register", "", body.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    // A well-formed request reports the unreachable upstream, not a local error.
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/register",
            "",
            json!({"username":"reader","password":"a-long-test-password","api_key":"valid-but-unreachable"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    // Signing in as an account that does not exist must not reveal which half was wrong.
    let response = router
        .oneshot(request(
            "POST",
            "/api/login",
            "",
            json!({"username":"ghost","password":"a-long-test-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_proxy_tagging_pinning_and_logout_match_karakeep_contract() {
    let mock = Router::new().fallback(|request: Request| async move {
        if request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            != Some("Bearer test-key")
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"Unauthorized"})),
            )
                .into_response();
        }
        if request.uri().path() == "/api/v1/users/me" {
            return Json(json!({"id":"user-1","name":"Test reader"})).into_response();
        }
        let method = request.method().to_string();
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or_default().to_owned();
        let bytes = to_bytes(request.into_body(), 16_384).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Json(json!({"method":method,"path":path,"query":query,"body":body})).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let mut state = app(&format!("http://{address}/api/v1/"));
    state.secure_cookie = true;
    let router = router(state.clone());
    // A key the upstream rejects must not create an account.
    let denied = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/register",
            "",
            json!({"username":"reader","password":"a-long-test-password","api_key":"wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert!(state.vault.lock().unwrap().accounts.is_empty());
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/register",
            "",
            json!({"username":"reader","password":"a-long-test-password","api_key":"test-key"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    for attribute in ["HttpOnly", "SameSite=Strict", "Max-Age=43200", "Secure"] {
        assert!(cookie.contains(attribute));
    }
    // Neither the upstream key nor the password may appear in the cookie.
    assert!(!cookie.contains("test-key") && !cookie.contains("a-long-test-password"));
    assert_eq!(
        json_body(response).await,
        json!({"id":"user-1","name":"Test reader","histerConnected":false})
    );
    let cookie = cookie.split(';').next().unwrap();
    let response = router
        .clone()
        .oneshot(request(
            "GET",
            "/api/bookmarks?q=rust%20%26%20web&cursor=40",
            cookie,
            Value::Null,
        ))
        .await
        .unwrap();
    let data = json_body(response).await;
    assert_eq!(data["path"], "/api/v1/bookmarks/search");
    let query: HashMap<String, String> = Url::parse(&format!(
        "http://localhost/?{}",
        data["query"].as_str().unwrap()
    ))
    .unwrap()
    .query_pairs()
    .into_owned()
    .collect();
    assert_eq!(query["q"], "rust & web");
    assert_eq!(query["cursor"], "40");
    for (method, path, input, expected) in [
        (
            "PATCH",
            "/api/bookmarks/bookmark123",
            json!({"favourited":true}),
            json!({"favourited":true}),
        ),
        (
            "POST",
            "/api/bookmarks/bookmark123/tags",
            json!({"name":"read later"}),
            json!({"tags":[{"tagName":"read later"}]}),
        ),
        (
            "DELETE",
            "/api/bookmarks/bookmark123/tags",
            json!({"name":"read later"}),
            json!({"tags":[{"tagName":"read later"}]}),
        ),
        (
            "POST",
            "/api/bookmarks",
            json!({"url":"https://example.com","title":"Example"}),
            json!({"type":"link","url":"https://example.com","title":"Example"}),
        ),
    ] {
        let response = router
            .clone()
            .oneshot(request(method, path, cookie, input))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let data = json_body(response).await;
        assert_eq!(data["method"], method);
        assert_eq!(data["body"], expected);
    }
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/bookmarks",
            cookie,
            json!({"url":"javascript:alert(1)","title":"Bad"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = router
        .clone()
        .oneshot(request("POST", "/api/logout", cookie, Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let response = router
        .oneshot(request("GET", "/api/me", cookie, Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    server.abort();
}

#[tokio::test]
async fn opensearch_descriptor_uses_the_request_host_and_rejects_junk() {
    for (host, expected) in [
        (
            "kara.example:8787",
            "http://kara.example:8787/?q={searchTerms}",
        ),
        ("evil\"><x", "http://localhost/?q={searchTerms}"),
    ] {
        let response = router(app("http://127.0.0.1:1"))
            .oneshot(
                Request::builder()
                    .uri("/opensearch.xml")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 100_000).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(expected), "{host} produced {body}");
    }
}

#[tokio::test]
async fn one_password_unlocks_both_upstream_keys() {
    let mock = Router::new()
        .route(
            "/api/v1/users/me",
            get(|| async { Json(json!({"id": "user-1", "name": "Reader"})) }),
        )
        .route(
            "/api/profile",
            get(|headers: HeaderMap| async move {
                if headers[header::AUTHORIZATION] == "Bearer hister-token" {
                    Ok(Json(json!({"user_id": 7, "username": "reader"})))
                } else {
                    Err(StatusCode::FORBIDDEN)
                }
            }),
        )
        .route(
            "/api/history",
            get(
                |headers: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    Json(json!({
                        "auth": headers[header::AUTHORIZATION].to_str().unwrap(),
                        "opened": q.get("opened"),
                        "documents": [{"url": "https://example.com/read", "title": "Read"}],
                    }))
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let mut state = app(&format!("{base}/api/v1/"));
    state.hister = Some(Url::parse(&format!("{base}/")).unwrap());

    // A Hister token the server rejects must not create a half-configured account.
    let rejected = router(state.clone())
        .oneshot(request(
            "POST",
            "/api/register",
            "",
            json!({"username":"reader","password":"a-long-test-password","api_key":"kara-key","hister_token":"wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert!(state.vault.lock().unwrap().accounts.is_empty());

    let response = router(state.clone())
        .oneshot(request(
            "POST",
            "/api/register",
            "",
            json!({"username":"reader","password":"a-long-test-password","api_key":"kara-key","hister_token":"hister-token"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let profile = json_body(response).await;
    assert_eq!(profile["hister"]["username"], "reader");

    // Registration is closed once an account exists.
    let mut closed = state.clone();
    closed.open_registration = false;
    let second = router(closed)
        .oneshot(request(
            "POST",
            "/api/register",
            "",
            json!({"username":"intruder","password":"a-long-test-password","api_key":"kara-key"}),
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::FORBIDDEN);

    // The whole point: sign in with the password alone, reach both upstreams.
    let wrong = router(state.clone())
        .oneshot(request(
            "POST",
            "/api/login",
            "",
            json!({"username":"reader","password":"not-the-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let response = router(state.clone())
        .oneshot(request(
            "POST",
            "/api/login",
            "",
            json!({"username":"reader","password":"a-long-test-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    let karakeep = router(state.clone())
        .oneshot(request("GET", "/api/me", &cookie, Value::Null))
        .await
        .unwrap();
    assert_eq!(karakeep.status(), StatusCode::OK);

    let history = router(state.clone())
        .oneshot(request(
            "GET",
            "/api/history?opened=true",
            &cookie,
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(history.status(), StatusCode::OK);
    let body = json_body(history).await;
    assert_eq!(body["auth"], "Bearer hister-token");
    assert_eq!(body["opened"], "true", "opened must reach Hister");

    // Dropping the Hister token leaves history unavailable but Karakeep working.
    let updated = router(state.clone())
        .oneshot(request(
            "POST",
            "/api/keys",
            &cookie,
            json!({"api_key":"kara-key"}),
        ))
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    let denied = router(state.clone())
        .oneshot(request("GET", "/api/history", &cookie, Value::Null))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    let still_fine = router(state.clone())
        .oneshot(request("GET", "/api/me", &cookie, Value::Null))
        .await
        .unwrap();
    assert_eq!(still_fine.status(), StatusCode::OK);

    // No session at all is unauthorized, not a 404.
    let anonymous = router(state.clone())
        .oneshot(request("GET", "/api/history", "", Value::Null))
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    server.abort();
}

#[tokio::test]
async fn open_registration_admits_several_users_with_their_own_keys() {
    let mock = Router::new().route(
        "/api/v1/users/me",
        get(|headers: HeaderMap| async move {
            Json(json!({"id": headers[header::AUTHORIZATION].to_str().unwrap()}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let state = app(&format!("http://{address}/api/v1/"));

    let mut cookies = Vec::new();
    for (name, key) in [
        ("alice", "alice-key"),
        ("bob", "bob-key"),
        ("carol", "carol-key"),
    ] {
        let response = router(state.clone())
            .oneshot(request(
                "POST",
                "/api/register",
                "",
                json!({"username": name, "password": "a-long-test-password", "api_key": key}),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "{name} could not register"
        );
        cookies.push(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(state.vault.lock().unwrap().accounts.len(), 3);

    // Concurrent sessions each proxy with their owner's key, never a shared one.
    for (cookie, key) in cookies.iter().zip(["alice-key", "bob-key", "carol-key"]) {
        let response = router(state.clone())
            .oneshot(request("GET", "/api/me", cookie, Value::Null))
            .await
            .unwrap();
        assert_eq!(json_body(response).await["id"], format!("Bearer {key}"));
    }

    // An open server still keeps usernames unique and still offers registration.
    let taken = router(state.clone())
        .oneshot(request(
            "POST",
            "/api/register",
            "",
            json!({"username":"alice","password":"[API_KEY_c62b2c84d2a0]","api_key":"other-key"}),
        ))
        .await
        .unwrap();
    assert_eq!(taken.status(), StatusCode::CONFLICT);

    let config = router(state.clone())
        .oneshot(request("GET", "/api/config", "", Value::Null))
        .await
        .unwrap();
    let config = json_body(config).await;
    assert_eq!(config["canRegister"], true);
    assert_eq!(config["openRegistration"], true);

    // Alice's key must not have been replaced by the rejected attempt.
    let response = router(state.clone())
        .oneshot(request("GET", "/api/me", &cookies[0], Value::Null))
        .await
        .unwrap();
    assert_eq!(json_body(response).await["id"], "Bearer alice-key");
    server.abort();
}
