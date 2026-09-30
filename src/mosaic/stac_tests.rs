use super::*;
use crate::resource::WorkBudget;
use axum::{
    extract::{OriginalUri, State},
    http::{HeaderMap, Method},
    response::IntoResponse,
    routing::any,
    Router,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

type RecordedRequest = (Method, String, Value, bool);

#[derive(Clone)]
struct Fixture {
    pages: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}
async fn page(
    State(fixture): State<Fixture>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> impl IntoResponse {
    fixture.requests.lock().unwrap().push((
        method,
        uri.to_string(),
        serde_json::from_slice(&body).unwrap_or(Value::Null),
        headers.get("x-auth").is_some_and(|v| v == "secret"),
    ));
    let value = fixture.pages.lock().unwrap().pop_front().unwrap();
    axum::Json(value)
}
async fn source(pages: Vec<Value>) -> (StacSource, Fixture, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        pages: Arc::new(Mutex::new(pages.into())),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/search", any(page))
        .route("/page", any(page))
        .with_state(fixture.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut headers = reqwest_stac::header::HeaderMap::new();
    headers.insert(
        "x-auth",
        reqwest_stac::header::HeaderValue::from_static("secret"),
    );
    let http = reqwest_stac::Client::builder()
        .no_proxy()
        .default_headers(headers)
        .build()
        .unwrap();
    (
        StacSource::with_http_client(&api_url, http, vec!["test".into()], "data").unwrap(),
        fixture,
        task,
    )
}
fn item(href: &str) -> Value {
    serde_json::json!({"assets":{"data":{"href":href}},"bbox":[-180,-90,180,90]})
}
fn coord() -> TileCoord {
    TileCoord {
        tms: crate::tms::TileMatrixSet::WebMercatorQuad,
        z: 0,
        x: 0,
        y: 0,
    }
}
fn options() -> DiscoveryOptions {
    DiscoveryOptions {
        max_response_bytes: 4096,
        max_total_response_bytes: 16384,
        work_budget: WorkBudget::new(1024 * 1024),
        ..DiscoveryOptions::default()
    }
}

#[tokio::test]
async fn pagination_preserves_order_auth_and_post_merge_then_get() {
    let pages = vec![
        serde_json::json!({"features":[item("a")],"links":[{"rel":"next","href":"/page","method":"POST","merge":true,"body":{"token":1}}]}),
        serde_json::json!({"features":[item("a"), item("b")],"links":[{"rel":"next","href":"/page?token=2"}]}),
        serde_json::json!({"features":[item("c")],"links":[]}),
    ];
    let (source, fixture, task) = source(pages).await;
    let options = options();
    let out = source
        .assets_for_tile_with_options(coord(), &options)
        .await
        .unwrap();
    assert_eq!(
        out.assets()
            .iter()
            .map(|a| a.url.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert!(options.work_budget.used_bytes() > 0);
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r.3));
    assert_eq!(requests[1].2["collections"], serde_json::json!(["test"]));
    assert_eq!(requests[1].2["token"], 1);
    assert_eq!(requests[2].0, Method::GET);
    assert_eq!(requests[2].1, "/page?token=2");
    drop(out);
    assert_eq!(options.work_budget.used_bytes(), 0);
    task.abort();
}

#[tokio::test]
async fn pages_without_selected_asset_still_follow_next() {
    let (source, fixture, task) = source(vec![
        serde_json::json!({"features":[{"assets":{}}],"links":[{"rel":"next","href":"/page"}]}),
        serde_json::json!({"features":[item("later")]}),
    ])
    .await;
    assert_eq!(
        source
            .assets_for_tile_with_options(coord(), &options())
            .await
            .unwrap()
            .assets()[0]
            .url,
        "later"
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    task.abort();
}

#[tokio::test]
async fn count_byte_page_and_cycle_limits_fail_without_partial_results() {
    let cases = [
        (
            vec![
                serde_json::json!({"features":[item("a"),item("b")],"links":[{"rel":"next","href":"/page"}]}),
            ],
            "count",
        ),
        (
            vec![serde_json::json!({"features":[],"padding":"x".repeat(8192)})],
            "bytes",
        ),
        (
            vec![
                serde_json::json!({"features":[],"links":[{"rel":"next","href":"/page"}]}),
                serde_json::json!({"features":[],"links":[{"rel":"next","href":"/page"}]}),
            ],
            "cycle",
        ),
        (
            vec![serde_json::json!({"features":[],"links":[{"rel":"next","href":"/page"}]})],
            "pages",
        ),
    ];
    for (pages, kind) in cases {
        let (source, fixture, task) = source(pages).await;
        let mut options = options();
        if kind == "count" {
            options.max_assets = 1;
        }
        if kind == "pages" {
            options.max_pages = 1;
        }
        let error = match source.assets_for_tile_with_options(coord(), &options).await {
            Err(e) => e,
            Ok(_) => panic!("limit should reject {kind}"),
        };
        assert!(
            matches!(error, AgError::ResourceLimit(_) | AgError::Mosaic(_)),
            "{error}"
        );
        assert!(fixture.requests.lock().unwrap().len() <= 2);
        assert_eq!(options.work_budget.used_bytes(), 0);
        task.abort();
    }
}

#[tokio::test]
async fn opaque_client_fails_closed_before_network_io() {
    let http = reqwest_stac::Client::builder().no_proxy().build().unwrap();
    let client = Client::with_client("http://127.0.0.1:1", http).unwrap();
    #[allow(deprecated)]
    let source = StacSource::with_client(client, vec![], "data");
    let error = source.assets_for_tile(coord()).await.unwrap_err();
    assert!(error.to_string().contains("with_http_client"));
}

#[tokio::test]
async fn working_budget_and_total_response_limit_release_leases() {
    for low_memory in [true, false] {
        let first =
            serde_json::json!({"features":[item("a")],"links":[{"rel":"next","href":"/page"}]});
        let second = serde_json::json!({"features":[item("b")],"padding":"x".repeat(200)});
        let first_bytes = serde_json::to_vec(&first).unwrap().len();
        let (source, _, task) = source(vec![first, second]).await;
        let mut options = options();
        if low_memory {
            options.work_budget = WorkBudget::new(1);
        } else {
            options.max_total_response_bytes = first_bytes + 100;
        }
        assert!(matches!(
            source.assets_for_tile_with_options(coord(), &options).await,
            Err(AgError::ResourceLimit(_))
        ));
        assert_eq!(options.work_budget.used_bytes(), 0);
        task.abort();
    }
}

#[tokio::test]
async fn cancelled_discovery_does_not_leave_cpu_or_request_permits() {
    use crate::request::{RequestLimiter, RequestOptions};
    let (source, fixture, task) = source(vec![]).await;
    let options = DiscoveryOptions {
        cpu_limiter: crate::blocking::CpuLimiter::new(1),
        ..options()
    };
    let permit = options.cpu_limiter.acquire().await.unwrap();
    let requests = RequestLimiter::new(1);
    let request_options = RequestOptions::default();
    let cancellation = request_options.cancellation.clone();
    let spawned_requests = requests.clone();
    let spawned_options = options.clone();
    let worker = tokio::spawn(async move {
        spawned_requests
            .run(
                std::time::Duration::from_secs(5),
                &request_options,
                |_| async move {
                    source
                        .assets_for_tile_with_options(coord(), &spawned_options)
                        .await
                },
            )
            .await
    });
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert!(matches!(worker.await.unwrap(), Err(AgError::Cancelled)));
    assert!(fixture.requests.lock().unwrap().is_empty());
    assert_eq!(requests.available_permits(), 1);
    assert_eq!(options.work_budget.used_bytes(), 0);
    drop(permit);
    assert_eq!(options.cpu_limiter.available_permits(), 1);
    task.abort();
}
