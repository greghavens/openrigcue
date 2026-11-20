//! The UI routes, driven through the router without a server.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;

async fn get(uri: &str) -> (StatusCode, String, String, Option<String>) {
    let backend = crate::app::tests::FakeBackend::new();
    let host = host::UiHost::new(move || app::MyApp::new(backend as Arc<dyn backend::Backend>));
    let response = router::<()>(host).oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let kind = response.headers().get("content-type").map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
    let cache = response.headers().get("cache-control").map(|value| value.to_str().unwrap().to_owned());
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, kind, body, cache)
}

/// The first request contains the loading label and the client script; onload constructs the application frame.
#[tokio::test]
async fn page_is_served() {
    let (status, kind, body, cache) = get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(cache.is_none());
    assert!(kind.starts_with("text/html"));
    assert!(body.starts_with("<!DOCTYPE html>"));
    assert!(body.contains("var Remi = function()"));
    for page in ["orc-nav", "nav-item", "Tuning", "Games", "Setup"] {
        assert!(!body.contains(page), "frame was constructed before onload: {page}");
    }
    assert!(body.contains("loading..."));
    assert!(body.contains("onload=\"remi.sendCallback("));
    assert!(body.contains("<link href='/res:css/openrigcue-ui.css' rel='stylesheet' />"));
    assert!(body.contains("<script src=\"/res:js/openrigcue-ui.js\"></script>"));
    assert!(!body.contains("style-openrigcue.css"), "the old style sheet is not linked");
    assert!(body.contains("id=\"loading-message\""));
    let (status, kind, _, cache) = get("/res:css/openrigcue-ui.css").await;
    assert_eq!((status, kind.as_str()), (StatusCode::OK, "text/css"));
    assert_eq!(cache.as_deref(), Some("public, max-age=86400"));
    let (status, kind, _, _) = get("/res:js/openrigcue-ui.js").await;
    assert_eq!((status, kind.as_str()), (StatusCode::OK, "text/javascript"));
    let (status, kind, body, _) = get("/res:images/openrigcue/mark.svg").await;
    assert_eq!((status, kind.as_str()), (StatusCode::OK, "image/svg+xml"));
    assert!(!body.is_empty());
    // The old interface's resources are gone: a missing file is answered with an empty body.
    for old in ["/res:css/style-openrigcue.css", "/res:fonts/openrigcue-icons.css", "/res:js/imask.js"] {
        let (_, _, body, _) = get(old).await;
        assert!(body.is_empty(), "{old}");
    }
    let (status, kind, body, cache) = get("/res:css/style.css").await;
    assert_eq!((status, kind.as_str()), (StatusCode::OK, "text/css"));
    assert!(body.is_empty());
    assert_eq!(cache.as_deref(), Some("public, max-age=86400"));
}

/// The first onload starts the loop, which then runs without further browser requests.
#[tokio::test]
async fn main_loop_starts_on_page_load_and_runs_without_further_browser_requests() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let backend = crate::app::tests::FakeBackend::new();
    let source = backend.clone();
    let host = host::UiHost::new(move || app::MyApp::new(source));
    let page = host.page().await.unwrap();
    assert_eq!(backend.synchronized.load(Ordering::SeqCst), 0);
    let body = page.split("<body").nth(1).unwrap().split("id=\"").nth(1).unwrap().split('"').next().unwrap();
    assert!(host.message(format!("callback/{body}/onload/")).await);
    let initialized = host.page().await.unwrap();
    for page in ["orc-nav", "nav-item", "Tuning", "Games", "Hardware", "Settings", "orc-act"] {
        if page == "orc-act" {
            assert!(!initialized.contains(page), "the action receiver is not part of the page");
        } else {
            assert!(initialized.contains(page), "missing {page} after onload");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.synchronized.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(backend.synchronized.load(Ordering::SeqCst) >= 2);
}
