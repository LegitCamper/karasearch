use super::*;
use axum::body::{Body, to_bytes};
use tower::ServiceExt;

fn app(upstream: &str) -> App {
    App {
        client: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        upstream: Url::parse(upstream).unwrap(),
        search_url: "http://localhost:8080/search?q={query}".into(),
        secure_cookie: false,
        sessions: Arc::new(Mutex::new(HashMap::new())),
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
    assert!(
        store.load().unwrap().is_empty(),
        "missing file is not an error"
    );

    let mut sessions = HashMap::new();
    sessions.insert(
        "token-live".to_owned(),
        Session {
            key: "secret-api-key".to_owned(),
            expires: now() + 600,
        },
    );
    sessions.insert(
        "token-stale".to_owned(),
        Session {
            key: "expired-api-key".to_owned(),
            expires: now() - 1,
        },
    );
    store.save(&sessions).unwrap();

    let sealed = std::fs::read(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    store.save(&sessions).unwrap();
    assert_ne!(
        sealed,
        std::fs::read(&path).unwrap(),
        "each save uses a fresh nonce"
    );
    assert!(
        !String::from_utf8_lossy(&sealed).contains("secret-api-key"),
        "API key must not be readable on disk"
    );

    let restored = Store::new(path.clone(), secret).unwrap().load().unwrap();
    assert_eq!(restored.len(), 1, "expired sessions are dropped on load");
    assert_eq!(restored["token-live"].key, "secret-api-key");

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
async fn persistent_sessions_keep_users_separate_and_logout_survives_restart() {
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
    for key in ["alice-key", "bob-key"] {
        let response = router(state.clone())
            .oneshot(request("POST", "/api/login", "", json!({"api_key": key})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        cookies.push(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .to_owned(),
        );
    }
    let before = store.load().unwrap();
    state.sessions = Arc::new(Mutex::new(store.load().unwrap()));
    for (cookie, key) in cookies.iter().zip(["alice-key", "bob-key"]) {
        let response = router(state.clone())
            .oneshot(request("GET", "/api/me", cookie, Value::Null))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["id"], format!("Bearer {key}"));
    }
    for (token, session) in state.sessions.lock().unwrap().iter() {
        assert_eq!(
            session.expires, before[token].expires,
            "restart must not extend expiry"
        );
    }
    let mut failing = state.clone();
    failing.store = Some(Arc::new(
        Store::new(path.join("missing/sessions.enc"), secret).unwrap(),
    ));
    for (route, body) in [
        ("logout", Value::Null),
        ("login", json!({"api_key": "replacement-key"})),
    ] {
        let response = router(failing.clone())
            .oneshot(request("POST", &format!("/api/{route}"), &cookies[0], body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(state.sessions.lock().unwrap().len(), 2);
        assert_eq!(store.load().unwrap().len(), 2);
    }
    let response = router(state.clone())
        .oneshot(request("POST", "/api/logout", &cookies[0], Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    state.sessions = Arc::new(Mutex::new(store.load().unwrap()));
    let response = router(state.clone())
        .oneshot(request("GET", "/api/me", &cookies[0], Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router(state)
        .oneshot(request("GET", "/api/me", &cookies[1], Value::Null))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    fs::remove_file(path).unwrap();
    server.abort();
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
    let mut missing_header = request("POST", "/api/login", "", json!({"api_key":"test"}));
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
    let mut cross_site = request("POST", "/api/login", "", json!({"api_key":"test"}));
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
    state.sessions.lock().unwrap().insert(
        "expired".into(),
        Session {
            key: "secret".into(),
            expires: now() - 1,
        },
    );
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
    assert!(state.sessions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn validates_login_and_reports_unreachable_upstream() {
    let router = router(app("http://127.0.0.1:1/api/v1/"));
    let response = router
        .clone()
        .oneshot(request("POST", "/api/login", "", json!({"api_key":"\n"})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = router
        .oneshot(request(
            "POST",
            "/api/login",
            "",
            json!({"api_key":"valid-but-unreachable"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
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
    let denied = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/login",
            "",
            json!({"api_key":"wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert!(state.sessions.lock().unwrap().is_empty());
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/login",
            "",
            json!({"api_key":"test-key"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    for attribute in ["HttpOnly", "SameSite=Strict", "Max-Age=43200", "Secure"] {
        assert!(cookie.contains(attribute));
    }
    assert!(!cookie.contains("test-key"));
    assert_eq!(
        json_body(response).await,
        json!({"id":"user-1","name":"Test reader"})
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
