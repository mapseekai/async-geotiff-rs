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
    /// Use `with_http_client` to configure headers, authentication or proxies.
    pub fn new(
        api_url: &str,
        collections: Vec<String>,
        asset_key: impl Into<String>,
    ) -> AgResult<Self> {
        url::Url::parse(api_url).map_err(|e| AgError::Mosaic(format!("stac client: {e}")))?;
        let http = reqwest_stac::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| AgError::Mosaic(format!("stac HTTP client: {e}")))?;
        Self::with_http_client(api_url, http, collections, asset_key)
    }

    /// Use the original caller-configured reqwest 0.11 transport, retaining its
    /// default headers/authentication, proxy and timeout configuration.
    /// Pagination links must stay on the API origin to avoid leaking credentials.
    pub fn with_http_client(
        api_url: &str,
        http: reqwest_stac::Client,
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
    /// Rebuild with `with_http_client`, passing the same reqwest client instead.
    #[deprecated(
        note = "use with_http_client; opaque stac-client transport cannot enforce response budgets"
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
            "opaque stac-client cannot enforce discovery budgets; use StacSource::with_http_client with the original reqwest client".into()))?;
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
                .map_err(|e| AgError::Mosaic(format!("stac search: {e}")))?
                .error_for_status()
                .map_err(|e| AgError::Mosaic(format!("stac search: {e}")))?;
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
                .map_err(|e| AgError::Mosaic(format!("stac response: {e}")))?
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
#[path = "stac_tests.rs"]
mod discovery_tests;

#[cfg(test)]
mod tests {
    use super::*;

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
