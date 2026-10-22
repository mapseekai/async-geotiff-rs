//! STAC `/search` discovery with bounded pagination and response parsing.
//! Pages retain server order; duplicate hrefs retain their first paint priority.
//! Parsing runs under the same CPU and working-memory budgets as mosaic rendering.

use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::mosaic::source::{
    AssetDiscovery, AssetRef, DiscoveredAssets, DiscoveryOptions, MosaicSource,
};
use crate::request::RequestContext;
use crate::tms::{TileCoord, Tms};
use serde::Deserialize;
use serde_json::Value;
use stac_client::Client;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct StacSource {
    http: Option<reqwest_stac::Client>,
    api_url: url::Url,
    collections: Vec<String>,
    asset_key: String,
}

impl StacSource {
    /// Create a bounded source. Automatic proxy discovery is disabled.
    /// Use `with_http_client_builder` to configure headers, authentication or proxies.
    pub fn new(
        api_url: &str,
        collections: Vec<String>,
        asset_key: impl Into<String>,
    ) -> AgResult<Self> {
        Self::with_http_client_builder(
            api_url,
            reqwest_stac::Client::builder().no_proxy(),
            collections,
            asset_key,
        )
    }

    /// A built reqwest client has an opaque redirect policy that cannot be
    /// overridden. This compatibility entry point fails before any network I/O.
    #[deprecated(
        note = "use with_http_client_builder; built clients cannot enforce same-origin redirects"
    )]
    pub fn with_http_client(
        _api_url: &str,
        _http: reqwest_stac::Client,
        _collections: Vec<String>,
        _asset_key: impl Into<String>,
    ) -> AgResult<Self> {
        Err(AgError::Mosaic(
            "built HTTP clients cannot enforce STAC redirect origin; use StacSource::with_http_client_builder".into(),
        ))
    }

    /// Build a reqwest 0.11 transport retaining default headers, authentication,
    /// proxies and timeouts. Its redirect policy is replaced with a same-origin
    /// policy, limited to ten hops, before any request can carry credentials.
    pub fn with_http_client_builder(
        api_url: &str,
        builder: reqwest_stac::ClientBuilder,
        collections: Vec<String>,
        asset_key: impl Into<String>,
    ) -> AgResult<Self> {
        let api_url =
            url::Url::parse(api_url).map_err(|e| AgError::Mosaic(format!("stac client: {e}")))?;
        if !matches!(api_url.scheme(), "http" | "https") || api_url.cannot_be_a_base() {
            return Err(AgError::Mosaic(
                "stac client requires an HTTP(S) base URL".into(),
            ));
        }
        let origin = api_url.origin();
        let http = builder
            .redirect(reqwest_stac::redirect::Policy::custom(move |attempt| {
                if attempt.url().origin() != origin {
                    attempt.error("STAC redirect cannot leave the configured API origin")
                } else if attempt.previous().len() >= 10 {
                    attempt.error("STAC redirect limit exceeded")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| AgError::Mosaic(format!("stac HTTP client: {}", e.without_url())))?;
        Ok(Self {
            http: Some(http),
            api_url,
            collections,
            asset_key: asset_key.into(),
        })
    }

    /// Compatibility constructor for an opaque stac-client 0.3 transport.
    /// Discovery returns an error because that client's public methods buffer
    /// responses without a byte bound and do not expose its configured transport.
    /// Rebuild with `with_http_client_builder`, carrying over its HTTP settings.
    #[deprecated(
        note = "use with_http_client_builder; opaque stac-client transport cannot enforce response budgets"
    )]
    pub fn with_client(
        client: Client,
        collections: Vec<String>,
        asset_key: impl Into<String>,
    ) -> Self {
        Self {
            http: None,
            api_url: client.base_url().clone(),
            collections,
            asset_key: asset_key.into(),
        }
    }
}

#[derive(Deserialize)]
struct Page<'a> {
    #[serde(borrow)]
    features: Vec<Item<'a>>,
    #[serde(default)]
    links: Vec<Link>,
}
#[derive(Deserialize)]
struct Item<'a> {
    #[serde(borrow)]
    assets: HashMap<std::borrow::Cow<'a, str>, Asset<'a>>,
    bbox: Option<Vec<f64>>,
}
#[derive(Deserialize)]
struct Asset<'a> {
    #[serde(borrow)]
    href: std::borrow::Cow<'a, str>,
}
#[derive(Deserialize)]
struct Link {
    rel: String,
    href: String,
    method: Option<String>,
    body: Option<Value>,
    #[serde(default)]
    merge: bool,
    #[serde(default)]
    headers: HashMap<String, String>,
}
#[derive(Debug)]
struct PageRequest {
    url: url::Url,
    method: reqwest_stac::Method,
    body: Value,
    headers: HashMap<String, String>,
    _work: Option<crate::resource::WorkReservation>,
}

impl PageRequest {
    fn next(
        link: Link,
        previous: &Self,
        response_url: &url::Url,
        origin: &url::Url,
    ) -> AgResult<Self> {
        let url = response_url
            .join(&link.href)
            .map_err(|e| AgError::Mosaic(format!("stac next URL: {e}")))?;
        if url.origin() != origin.origin() {
            return Err(AgError::Mosaic(
                "stac pagination cannot leave the configured API origin".into(),
            ));
        }
        let method =
            reqwest_stac::Method::from_bytes(link.method.as_deref().unwrap_or("GET").as_bytes())
                .map_err(|e| AgError::Mosaic(format!("stac next method: {e}")))?;
        if method != reqwest_stac::Method::GET && method != reqwest_stac::Method::POST {
            return Err(AgError::Mosaic(
                "stac pagination only supports GET and POST".into(),
            ));
        }
        if method == reqwest_stac::Method::GET && (link.body.is_some() || link.merge) {
            return Err(AgError::Mosaic(
                "stac GET pagination body/merge is unsupported; provide query parameters in href"
                    .into(),
            ));
        }
        let mut body = if link.merge {
            previous.body.clone()
        } else {
            serde_json::json!({})
        };
        if let Some(next) = link.body {
            if link.merge {
                let target = body
                    .as_object_mut()
                    .ok_or_else(|| AgError::Mosaic("stac merge requires an object body".into()))?;
                let updates = next
                    .as_object()
                    .ok_or_else(|| AgError::Mosaic("stac next body must be an object".into()))?;
                target.extend(updates.clone());
            } else {
                body = next;
            }
        }
        if !body.is_object() {
            return Err(AgError::Mosaic("stac next body must be an object".into()));
        }
        Ok(Self {
            url,
            method,
            body,
            headers: link.headers,
            _work: None,
        })
    }
}

#[async_trait::async_trait]
impl MosaicSource for StacSource {
    async fn assets_for_tile(&self, coord: TileCoord) -> AgResult<Vec<AssetRef>> {
        Ok(self
            .assets_for_tile_with_options(coord, &DiscoveryOptions::default())
            .await?
            .into_assets())
    }

    async fn assets_for_tile_with_options(
        &self,
        coord: TileCoord,
        options: &DiscoveryOptions,
    ) -> AgResult<DiscoveredAssets> {
        let http = self.http.as_ref().ok_or_else(|| AgError::Mosaic(
            "opaque stac-client cannot enforce discovery budgets; use StacSource::with_http_client_builder with the original HTTP settings".into()))?;
        let options = options.clone();
        let request = RequestContext::current();
        let config_bytes = self
            .collections
            .iter()
            .try_fold(
                self.api_url
                    .as_str()
                    .len()
                    .saturating_add(self.asset_key.len())
                    .saturating_add(256),
                |n, collection| {
                    n.checked_add(collection.len())
                        .and_then(|n| n.checked_add(128))
                },
            )
            .filter(|n| *n <= options.max_asset_bytes)
            .ok_or_else(|| {
                AgError::ResourceLimit("stac search metadata exceeds byte limit".into())
            })?;
        let plan_work = options.work_budget.try_reserve(
            (config_bytes as u64)
                .checked_mul(32)
                .ok_or_else(|| AgError::ResourceLimit("stac plan estimate overflow".into()))?,
        )?;
        let job_work = plan_work.clone();
        let coord_request = request.clone().map(|r| r.with_work(plan_work.clone()));
        let collections = self.collections.clone();
        let permit = options.cpu_limiter.acquire().await?;
        let body = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _work = job_work;
            RequestContext::run_blocking(coord_request, || {
                let tms = Tms::cached(coord.tms)?;
                let tb = tms.tile_bbox(coord.z, coord.x, coord.y)?;
                let mut corners = [(tb.xmin, tb.ymin), (tb.xmax, tb.ymin), (tb.xmax, tb.ymax), (tb.xmin, tb.ymax)];
                crate::warp::projection::convert_batch(&tms.crs(), &crate::crs::Crs::epsg(4326), &mut corners)?;
                let wb = Bbox::from_corners(&corners);
                Ok((serde_json::json!({"collections": collections, "bbox": [wb.xmin, wb.ymin, wb.xmax, wb.ymax]}), wb))
            })
        }).await.map_err(|e| AgError::Mosaic(format!("stac plan join: {e}")))??;
        let (body, wb) = body;
        let mut search = self.api_url.clone();
        search
            .path_segments_mut()
            .map_err(|_| AgError::Mosaic("invalid STAC base URL".into()))?
            .pop_if_empty()
            .push("search");
        let mut next = Some(PageRequest {
            url: search,
            method: reqwest_stac::Method::POST,
            body,
            headers: HashMap::new(),
            _work: Some(plan_work),
        });
        let mut seen_pages = HashSet::new();
        let mut page_keys_work = Vec::new();
        let mut discovery = AssetDiscovery::new(options.clone());
        let mut total = 0usize;
        let mut pages = 0usize;
        while let Some(page_request) = next {
            if let Some(request) = &request {
                request.check()?;
            }
            pages += 1;
            if pages > options.max_pages {
                return Err(AgError::ResourceLimit(
                    "stac pagination exceeds page limit".into(),
                ));
            }
            let key = format!(
                "{} {} {} {}",
                page_request.method,
                page_request.url,
                page_request.body,
                serde_json::to_string(&std::collections::BTreeMap::from_iter(
                    page_request.headers.iter()
                ))
                .map_err(|e| AgError::Mosaic(format!("stac pagination headers: {e}")))?
            );
            page_keys_work.push(options.work_budget.try_reserve((key.len() + 128) as u64)?);
            if !seen_pages.insert(key) {
                return Err(AgError::Mosaic("stac pagination cycle detected".into()));
            }
            let mut builder = http.request(page_request.method.clone(), page_request.url.clone());
            for (name, value) in &page_request.headers {
                builder = builder.header(name, value);
            }
            if page_request.method == reqwest_stac::Method::POST {
                builder = builder.json(&page_request.body);
            }
            let mut response = builder
                .send()
                .await
                .map_err(|e| AgError::Mosaic(format!("stac search: {}", e.without_url())))?
                .error_for_status()
                .map_err(|e| AgError::Mosaic(format!("stac search: {}", e.without_url())))?;
            let remaining = options.max_total_response_bytes.saturating_sub(total);
            let cap = options.max_response_bytes.min(remaining);
            if response.content_length().is_some_and(|n| n > cap as u64) {
                return Err(AgError::ResourceLimit(
                    "stac response exceeds byte limit".into(),
                ));
            }
            let response_work = options.work_budget.try_reserve(cap as u64)?;
            let response_url = response.url().clone();
            let mut bytes = Vec::with_capacity(cap);
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| AgError::Mosaic(format!("stac response: {}", e.without_url())))?
            {
                if let Some(request) = &request {
                    request.check()?;
                }
                if chunk.len() > cap.saturating_sub(bytes.len()) {
                    return Err(AgError::ResourceLimit(
                        "stac response exceeds byte limit".into(),
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            total += bytes.len();
            // Reserve a conservative bound for serde containers and escaped strings.
            // Unknown item properties/geometry are skipped, never materialized.
            let parse_work = options.work_budget.try_reserve(
                (bytes.len() as u64)
                    .checked_mul(32)
                    .ok_or_else(|| AgError::ResourceLimit("stac parse estimate overflow".into()))?,
            )?;
            let permit = options.cpu_limiter.acquire().await?;
            let parse_request = request.clone().map(|r| {
                r.with_work(response_work.clone())
                    .with_work(parse_work.clone())
            });
            let asset_key = self.asset_key.clone();
            let origin = self.api_url.clone();
            let page_options = options.clone();
            let (out, following) = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _response_work = response_work;
                let _parse_work = parse_work;
                RequestContext::run_blocking(parse_request.clone(), || {
                    let page: Page<'_> = serde_json::from_slice(&bytes)
                        .map_err(|e| AgError::Mosaic(format!("stac parse: {e}")))?;
                    for item in page.features {
                        if let Some(request) = &parse_request {
                            request.check()?;
                        }
                        if let Some(asset) = item.assets.get(asset_key.as_str()) {
                            let bbox = match item.bbox.as_deref() {
                                Some([w, s, e, n]) => Bbox::new(*w, *s, *e, *n),
                                Some([w, s, _, e, n, _]) => Bbox::new(*w, *s, *e, *n),
                                _ => wb,
                            };
                            discovery.push(&asset.href, &asset.href, bbox)?;
                        }
                    }
                    let mut links = page.links.into_iter().filter(|link| link.rel == "next");
                    let mut next = links
                        .next()
                        .map(|link| PageRequest::next(link, &page_request, &response_url, &origin))
                        .transpose()?;
                    if links.next().is_some() {
                        return Err(AgError::Mosaic(
                            "stac response contains multiple next links".into(),
                        ));
                    }
                    if let Some(next) = &mut next {
                        let body_bytes = serde_json::to_vec(&next.body)
                            .map_err(|e| AgError::Mosaic(format!("stac next body: {e}")))?
                            .len();
                        let header_bytes = next
                            .headers
                            .iter()
                            .try_fold(0usize, |n, (k, v)| {
                                n.checked_add(k.len())
                                    .and_then(|n| n.checked_add(v.len()))
                                    .and_then(|n| n.checked_add(128))
                            })
                            .ok_or_else(|| {
                                AgError::ResourceLimit("stac next header estimate overflow".into())
                            })?;
                        let metadata = body_bytes
                            .checked_add(header_bytes)
                            .and_then(|n| n.checked_add(next.url.as_str().len()))
                            .filter(|n| *n <= page_options.max_asset_bytes)
                            .ok_or_else(|| {
                                AgError::ResourceLimit(
                                    "stac pagination metadata exceeds byte limit".into(),
                                )
                            })?;
                        next._work = Some(page_options.work_budget.try_reserve(
                            (metadata as u64).checked_mul(32).ok_or_else(|| {
                                AgError::ResourceLimit(
                                    "stac next metadata estimate overflow".into(),
                                )
                            })?,
                        )?);
                    }
                    Ok((discovery, next))
                })
            })
            .await
            .map_err(|e| AgError::Mosaic(format!("stac parse join: {e}")))??;
            discovery = out;
            next = following;
        }
        Ok(discovery.finish())
    }
}

#[cfg(test)]
mod tests {
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
            .default_headers(headers);
        (
            StacSource::with_http_client_builder(&api_url, http, vec!["test".into()], "data")
                .unwrap(),
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
    async fn transport_failures_do_not_expose_url_credentials() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for response in [
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 4096];
                assert!(stream.read(&mut buffer).await.unwrap() > 0);
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            let url = format!("http://user:review-password@{address}/?api_key=review-token");
            let source = StacSource::new(&url, vec![], "data").unwrap();
            let error = source
                .assets_for_tile_with_options(coord(), &options())
                .await
                .err()
                .expect("request must fail");
            let message = format!("{error:?} {error}");
            for secret in ["review-password", "review-token", "api_key", "http://"] {
                assert!(!message.contains(secret), "{message}");
            }
            server.await.unwrap();
            // The listener is now closed, exercising the send-error path too.
            let error = source
                .assets_for_tile_with_options(coord(), &options())
                .await
                .err()
                .expect("request must fail");
            assert!(!format!("{error:?} {error}").contains("review-token"));
        }
    }

    #[tokio::test]
    async fn redirects_cannot_send_default_headers_to_another_origin() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let target_hits = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = format!("http://{}/target", listener.local_addr().unwrap());
        let target_app = Router::new().route(
            "/target",
            any(move || {
                let hits = target_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"features":[]}))
                }
            }),
        );
        let target_server =
            tokio::spawn(async move { axum::serve(listener, target_app).await.unwrap() });
        for pagination in [false, true] {
            let destination = target.clone();
            let app = Router::new()
                .route("/search", any(move || {
                    let destination = destination.clone();
                    async move {
                        if pagination {
                            axum::Json(serde_json::json!({"features":[],"links":[{"rel":"next","href":"/page"}]})).into_response()
                        } else {
                            axum::response::Redirect::temporary(&destination).into_response()
                        }
                    }
                }))
                .route("/page", any({
                    let destination = target.clone();
                    move || {
                        let destination = destination.clone();
                        async move { axum::response::Redirect::temporary(&destination) }
                    }
                }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut headers = reqwest_stac::header::HeaderMap::new();
            headers.insert(
                "x-api-key",
                reqwest_stac::header::HeaderValue::from_static("review-token"),
            );
            let configured = StacSource::with_http_client_builder(
                &base,
                reqwest_stac::Client::builder()
                    .no_proxy()
                    .default_headers(headers),
                vec![],
                "data",
            )
            .unwrap();
            for source in [StacSource::new(&base, vec![], "data").unwrap(), configured] {
                let error = source
                    .assets_for_tile_with_options(coord(), &options())
                    .await
                    .err()
                    .expect("request must fail");
                assert!(error.to_string().contains("origin"), "{error}");
                assert_eq!(hits.load(Ordering::SeqCst), 0);
            }
            server.abort();
        }
        target_server.abort();
    }

    #[tokio::test]
    async fn same_origin_redirects_preserve_authentication() {
        let app = Router::new()
            .route(
                "/search",
                any(|| async { axum::response::Redirect::temporary("/result") }),
            )
            .route(
                "/result",
                any(|headers: HeaderMap| async move {
                    assert_eq!(headers.get("x-api-key").unwrap(), "review-token");
                    axum::Json(serde_json::json!({"features":[item("scene")]}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut headers = reqwest_stac::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            reqwest_stac::header::HeaderValue::from_static("review-token"),
        );
        let source = StacSource::with_http_client_builder(
            &base,
            reqwest_stac::Client::builder()
                .no_proxy()
                .default_headers(headers),
            vec![],
            "data",
        )
        .unwrap();
        let assets = source
            .assets_for_tile_with_options(coord(), &options())
            .await
            .unwrap();
        assert_eq!(assets.assets()[0].url, "scene");
        server.abort();
    }

    #[test]
    #[allow(deprecated)]
    fn built_client_is_rejected_before_discovery() {
        let result = StacSource::with_http_client(
            "http://127.0.0.1:1",
            reqwest_stac::Client::new(),
            vec![],
            "data",
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("with_http_client_builder"));
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

    #[test]
    fn constructor_rejects_invalid_url() {
        let result = StacSource::new("not-a-url", vec![], "data");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("stac client"), "unexpected error: {msg}");
    }

    #[test]
    fn constructor_accepts_valid_url() {
        let src = StacSource::new(
            "https://planetarycomputer.microsoft.com/api/stac/v1",
            vec!["sentinel-2-l2a".to_string()],
            "B04",
        );
        assert!(src.is_ok());
    }
}
