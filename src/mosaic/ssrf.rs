//! SSRF guard for the mosaic [`ReaderPool`](super::ReaderPool).
//!
//! Asset URLs come from potentially untrusted MosaicJSON `tiles` entries and
//! STAC `asset.href` values, so before the pool opens one over HTTP we validate
//! its scheme and resolve its host, rejecting requests that would reach the
//! cloud-metadata endpoint (`169.254.169.254`), loopback, or — optionally —
//! private networks.
//!
//! Residual risk, by design:
//! - The mosaic **document** fetch and the STAC **search** endpoint are
//!   operator-configured, not per-asset, so they are NOT guarded here — only
//!   asset opens go through the pool.
//! - Asset connections pin the validated addresses and do not follow redirects.
//!   DNS work has a shared concurrency bound and a deadline.

use std::net::{IpAddr, ToSocketAddrs};

use crate::error::{AgError, AgResult};

/// Controls which HTTP(S) hosts a mosaic's asset readers will open,
/// to prevent SSRF via attacker-controlled mosaic asset URLs.
///
/// The [`Default`] blocks the crown-jewel SSRF targets (loopback and
/// link-local, which includes the `169.254.169.254` cloud-metadata endpoint)
/// while still allowing private ranges (internal object stores such as MinIO)
/// and plaintext HTTP. Use [`permissive`](Self::permissive) when every asset
/// URL is operator-controlled, or [`strict`](Self::strict) for fully untrusted
/// documents that should only reach public HTTPS assets.
#[derive(Clone, Debug)]
pub struct HttpAccessPolicy {
    allow_http: bool,
    block_loopback: bool,
    block_link_local: bool,
    block_private: bool,
    allow_hosts: Vec<String>,
}

impl Default for HttpAccessPolicy {
    fn default() -> Self {
        Self {
            allow_http: true,
            block_loopback: true,
            block_link_local: true,
            block_private: false,
            allow_hosts: Vec::new(),
        }
    }
}

impl HttpAccessPolicy {
    /// No restrictions — the pre-guard behavior. Use only when every asset URL
    /// is operator-controlled (e.g. a trusted internal mosaic index).
    pub fn permissive() -> Self {
        Self {
            allow_http: true,
            block_loopback: false,
            block_link_local: false,
            block_private: false,
            allow_hosts: Vec::new(),
        }
    }

    /// HTTPS-only; blocks loopback, link-local **and** private ranges. For
    /// fully untrusted mosaic/STAC documents that should reach only public
    /// assets.
    pub fn strict() -> Self {
        Self {
            allow_http: false,
            block_loopback: true,
            block_link_local: true,
            block_private: true,
            allow_hosts: Vec::new(),
        }
    }

    /// Permit (or forbid) plaintext `http://` asset URLs.
    pub fn allow_http(mut self, yes: bool) -> Self {
        self.allow_http = yes;
        self
    }

    /// Also block RFC-1918 / unique-local private ranges (internal networks).
    pub fn block_private(mut self, yes: bool) -> Self {
        self.block_private = yes;
        self
    }

    /// Add a hostname (exact, case-insensitive) that bypasses the IP checks —
    /// an explicit allowlist entry for a known-good internal host.
    pub fn allow_host(mut self, host: impl Into<String>) -> Self {
        self.allow_hosts.push(host.into().to_ascii_lowercase());
        self
    }

    /// Returns a reason string if `ip` is blocked by this policy, else `None`.
    fn ip_blocked(&self, ip: IpAddr) -> Option<&'static str> {
        // Normalize v4-mapped v6 (`::ffff:a.b.c.d`) to v4 so the v4 checks
        // can't be dodged by wrapping the address.
        let ip = match ip {
            IpAddr::V6(a) => a.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(a)),
            v4 => v4,
        };
        if ip.is_unspecified() {
            return Some("unspecified address");
        }
        match ip {
            IpAddr::V4(a) => {
                if self.block_loopback && a.is_loopback() {
                    return Some("loopback address");
                }
                if self.block_link_local && a.is_link_local() {
                    return Some("link-local address");
                }
                if self.block_private && a.is_private() {
                    return Some("private address");
                }
            }
            IpAddr::V6(a) => {
                if self.block_loopback && a.is_loopback() {
                    return Some("loopback address");
                }
                let seg = a.segments()[0];
                // fe80::/10 unicast link-local.
                if self.block_link_local && (seg & 0xffc0) == 0xfe80 {
                    return Some("link-local address");
                }
                // fc00::/7 unique-local.
                if self.block_private && (seg & 0xfe00) == 0xfc00 {
                    return Some("unique-local address");
                }
            }
        }
        None
    }
}

/// Reject `url` if its scheme or (resolved) host violates `policy`. Runs on the
/// pool's open (miss) path only, so it never adds latency to cached-reader
/// hits. Error text carries only the reason, never the URL, so presigned
/// credentials in a query string cannot leak.
pub(crate) async fn validate_http_url(
    url: &str,
    policy: &HttpAccessPolicy,
) -> AgResult<Vec<std::net::SocketAddr>> {
    let u = url::Url::parse(url).map_err(|e| AgError::Mosaic(format!("invalid asset url: {e}")))?;
    match u.scheme() {
        "https" => {}
        "http" if policy.allow_http => {}
        "http" => {
            return Err(AgError::Mosaic(
                "plaintext http asset url rejected by access policy".into(),
            ))
        }
        other => {
            return Err(AgError::Mosaic(format!(
                "asset url scheme {other:?} rejected by access policy"
            )))
        }
    }

    let ips: Vec<IpAddr> = match u.host() {
        Some(url::Host::Ipv4(a)) => vec![IpAddr::V4(a)],
        Some(url::Host::Ipv6(a)) => vec![IpAddr::V6(a)],
        Some(url::Host::Domain(d)) => {
            let port = u.port_or_known_default().unwrap_or(443);
            let hostname = d.to_string();
            // Fail closed, and pass these exact addresses to the connector.
            static DNS_LIMITER: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
                std::sync::OnceLock::new();
            let limiter = DNS_LIMITER
                .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(8)))
                .clone();
            resolve_bounded(limiter, std::time::Duration::from_secs(10), move || {
                (hostname.as_str(), port)
                    .to_socket_addrs()
                    .map(|it| it.map(|s| s.ip()).collect::<Vec<_>>())
                    .map_err(|_| AgError::Mosaic("asset host DNS resolution failed".into()))
            })
            .await?
        }
        None => return Err(AgError::Mosaic("asset url missing host".into())),
    };

    if ips.is_empty() {
        return Err(AgError::Mosaic(
            "asset host DNS returned no addresses".into(),
        ));
    }
    let allowlisted = u.host_str().is_some_and(|host| {
        policy
            .allow_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
    });
    for &ip in &ips {
        if allowlisted {
            continue;
        }
        if let Some(reason) = policy.ip_blocked(ip) {
            return Err(AgError::Mosaic(format!(
                "asset host resolves to blocked {reason}"
            )));
        }
    }
    Ok(ips
        .into_iter()
        .map(|ip| std::net::SocketAddr::new(ip, u.port_or_known_default().unwrap_or(443)))
        .collect())
}

/// Transport for one validated URL. DNS is pinned for the reader lifetime;
/// redirects and environment proxies cannot bypass the destination policy.
#[derive(Debug)]
pub(crate) struct PinnedConnector(pub reqwest::Client);

impl object_store::client::HttpConnector for PinnedConnector {
    fn connect(
        &self,
        _: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        Ok(object_store::client::HttpClient::new(self.0.clone()))
    }
}

pub(crate) async fn checked_http_store(
    url: &str,
    policy: &HttpAccessPolicy,
) -> AgResult<std::sync::Arc<dyn object_store::ObjectStore>> {
    let addresses = validate_http_url(url, policy).await?;
    let parsed = url::Url::parse(url).map_err(|_| AgError::Mosaic("invalid asset URL".into()))?;
    let url = url.to_owned();
    crate::io::cog::build_http_transport(move || {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(parsed.host_str().unwrap(), &addresses)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| AgError::Mosaic("HTTP client construction failed".into()))?;
        let store = object_store::http::HttpBuilder::new()
            .with_url(url)
            .with_http_connector(PinnedConnector(client))
            .build()
            .map_err(|_| AgError::Mosaic("HTTP store construction failed".into()))?;
        Ok(std::sync::Arc::new(store) as std::sync::Arc<dyn object_store::ObjectStore>)
    })
    .await
}

async fn resolve_bounded(
    limiter: std::sync::Arc<tokio::sync::Semaphore>,
    timeout: std::time::Duration,
    resolve: impl FnOnce() -> AgResult<Vec<IpAddr>> + Send + 'static,
) -> AgResult<Vec<IpAddr>> {
    tokio::time::timeout(timeout, async move {
        let permit = limiter
            .acquire_owned()
            .await
            .map_err(|_| AgError::Mosaic("DNS resolver closed".into()))?;
        let request = crate::request::RequestContext::current();
        tokio::task::spawn_blocking(move || {
            // A timed-out OS resolver cannot be stopped. Keep its capacity
            // owned by the blocking job until it really returns.
            let _permit = permit;
            crate::request::RequestContext::run_blocking(request, resolve)
        })
        .await
        .map_err(|_| AgError::Mosaic("DNS resolver task failed".into()))?
    })
    .await
    .map_err(|_| AgError::Mosaic("DNS resolution deadline exceeded".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_blocks_loopback_and_link_local() {
        let p = HttpAccessPolicy::default();
        assert!(validate_http_url("http://127.0.0.1/x.tif", &p)
            .await
            .is_err());
        assert!(validate_http_url("http://[::1]/x.tif", &p).await.is_err());
        // Cloud-metadata endpoint (IMDS) is link-local.
        assert!(
            validate_http_url("http://169.254.169.254/latest/meta-data/", &p)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn default_allows_private_and_public() {
        let p = HttpAccessPolicy::default();
        // Internal object store over http on a private IP stays reachable.
        assert!(validate_http_url("http://192.168.1.10:9000/b/x.tif", &p)
            .await
            .is_ok());
        assert!(validate_http_url("http://10.0.0.5/x.tif", &p).await.is_ok());
        assert!(validate_http_url("https://example.com/x.tif", &p)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn strict_blocks_private_and_plaintext() {
        let p = HttpAccessPolicy::strict();
        assert!(validate_http_url("http://example.com/x.tif", &p)
            .await
            .is_err()); // plaintext
        assert!(validate_http_url("https://10.0.0.5/x.tif", &p)
            .await
            .is_err()); // private
        assert!(validate_http_url("https://192.168.0.1/x.tif", &p)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn permissive_allows_loopback() {
        let p = HttpAccessPolicy::permissive();
        assert!(validate_http_url("http://127.0.0.1:9000/x.tif", &p)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn error_text_excludes_query_string() {
        let p = HttpAccessPolicy::default();
        let msg = validate_http_url("http://127.0.0.1/x.tif?X-Amz-Signature=s3cr3t", &p)
            .await
            .unwrap_err()
            .to_string();
        assert!(!msg.contains("s3cr3t"), "credentials leaked: {msg}");
    }

    #[tokio::test]
    async fn allow_host_bypasses_ip_checks() {
        // `localhost` resolves to loopback (blocked by default) but an explicit
        // allowlist entry lets it through.
        let p = HttpAccessPolicy::default().allow_host("localhost");
        assert!(validate_http_url("http://localhost:9000/x.tif", &p)
            .await
            .is_ok());
    }

    mod dns_failure {
        use super::*;
        #[tokio::test]
        async fn audit_dns_failure_is_closed() {
            assert!(validate_http_url(
                "https://no-such-host.invalid/x.tif",
                &HttpAccessPolicy::default()
            )
            .await
            .is_err());
        }
    }

    mod dns_limit {
        use super::*;
        #[tokio::test]
        async fn dns_timeout_keeps_capacity_until_blocking_resolver_finishes() {
            let limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                resolve_bounded(
                    limiter.clone(),
                    std::time::Duration::from_millis(10),
                    move || {
                        let _ = wait.recv();
                        Ok(vec!["127.0.0.1".parse().unwrap()])
                    },
                ),
            )
            .await;
            let available = limiter.available_permits();
            let _ = release.send(());
            assert!(
                matches!(result, Ok(Err(_))),
                "resolver must return its own bounded timeout"
            );
            assert_eq!(
                available, 0,
                "timeout must not release still-running blocking DNS capacity"
            );
            let permit = tokio::time::timeout(std::time::Duration::from_secs(1), limiter.acquire())
                .await
                .unwrap()
                .unwrap();
            drop(permit);
        }
    }
}
