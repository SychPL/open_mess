//! Module network access (spec 8.3): the module's host list, the address policy checked on every
//! resolved address, connections pinned to the checked addresses, no automatic redirects, and
//! size and time limits. Only method, host, status and size are logged.

use std::net::SocketAddr;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};

use crate::hosts::HostList;
use crate::ip_policy::{self, AddressPolicy};
use crate::limits::{self, Limits};

/// Mirrors `request` in `wit/module.wit`.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Mirrors `response` in `wit/module.wit`.
#[derive(Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

// Hand-written so that a `{:?}` never prints a URL, header or body: they carry tokens and messages.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request").field("method", &self.method).field("body_len", &self.body.len()).finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response").field("status", &self.status).field("body_len", &self.body.len()).finish_non_exhaustive()
    }
}

/// Mirrors `http-error` in `wit/module.wit`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("timed out")]
    Timeout,
    #[error("response too large")]
    TooLarge,
    #[error("throttled for {0} s")]
    Throttled(u32),
    #[error("network error: {0}")]
    Network(String),
}

pub(crate) fn blocked(why: impl Into<String>) -> HttpError {
    HttpError::Blocked(why.into())
}

/// A failed request, without the URL reqwest would name (it may carry tokens).
fn network(e: reqwest::Error) -> HttpError {
    if e.is_timeout() {
        return HttpError::Timeout;
    }
    let e = e.without_url();
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    HttpError::Network(message)
}

fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, HttpError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| HttpError::Network("invalid header name".to_string()))?;
        if name == header::HOST {
            return Err(blocked("the Host header comes from the URL"));
        }
        let value = HeaderValue::from_str(value).map_err(|_| HttpError::Network("invalid header value".to_string()))?;
        map.append(name, value);
    }
    Ok(map)
}

/// `Retry-After` in seconds. ponytail: the HTTP-date form is not read; such responses go to the
/// module as they are.
fn retry_after(headers: &HeaderMap) -> Option<u32> {
    let secs: u64 = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(u32::try_from(secs).unwrap_or(u32::MAX))
}

/// `url` on the test server at `addr`, its host moved to the `Host` header.
#[cfg(any(test, feature = "test-support"))]
fn fake_url(mut url: url::Url, addr: SocketAddr, headers: &mut HeaderMap) -> Result<url::Url, HttpError> {
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    headers.insert(header::HOST, HeaderValue::from_str(&host).map_err(|_| blocked("invalid host"))?);
    let moved = url.set_scheme("http").is_ok() && url.set_ip_host(addr.ip()).is_ok() && url.set_port(Some(addr.port())).is_ok();
    if moved { Ok(url) } else { Err(blocked("cannot move the URL to the test server")) }
}

/// The network as modules see it. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Net {
    policy: AddressPolicy,
    pub(crate) limits: Limits,
    /// Test support: the loopback server that serves every allowed host over plain HTTP.
    #[cfg(any(test, feature = "test-support"))]
    fake_server: Option<SocketAddr>,
}

impl Net {
    pub fn new(policy: AddressPolicy) -> Net {
        Self::build(policy, Limits::default())
    }

    /// A network with other time limits, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_limits(policy: AddressPolicy, limits: Limits) -> Net {
        Self::build(policy, limits)
    }

    fn build(policy: AddressPolicy, limits: Limits) -> Net {
        // reqwest is built without a bundled crypto provider: ring becomes the process default,
        // unless the program installed another one already.
        rustls::crypto::ring::default_provider().install_default().ok();
        Net {
            policy,
            limits,
            #[cfg(any(test, feature = "test-support"))]
            fake_server: None,
        }
    }

    /// Test support: every host on a module's list is served by the loopback server at `addr`,
    /// over plain HTTP, with the request's own host in its `Host` header. The host list is still
    /// checked, and nothing is resolved, so a module under test never reaches the real host.
    /// HTTP only; WebSockets are not redirected.
    #[cfg(any(test, feature = "test-support"))]
    pub fn serving_hosts_from(mut self, addr: SocketAddr) -> Net {
        self.fake_server = Some(addr);
        self
    }

    /// Checks the URL's host against the module's list, resolves it once and checks every
    /// address; connections go only to the addresses returned.
    pub(crate) async fn resolve(&self, hosts: &HostList, url: &url::Url) -> Result<Vec<SocketAddr>, HttpError> {
        let host = url.host_str().ok_or_else(|| blocked("the URL has no host"))?;
        if !hosts.allows(host) {
            return Err(blocked(format!("{host} is not in the module's host list")));
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Some(addr) = self.fake_server {
            return Ok(vec![addr]);
        }
        let port = url.port_or_known_default().ok_or_else(|| blocked("the URL has no port"))?;
        let addrs: Vec<SocketAddr> = match url.host() {
            Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(url::Host::Domain(domain)) => tokio::time::timeout(self.limits.connect_timeout, tokio::net::lookup_host((domain, port)))
                .await
                .map_err(|_| HttpError::Timeout)?
                .map_err(|_| HttpError::Network(format!("cannot resolve {host}")))?
                .collect(),
            None => return Err(blocked("the URL has no host")),
        };
        if addrs.is_empty() {
            return Err(HttpError::Network(format!("cannot resolve {host}")));
        }
        if addrs.iter().any(|addr| !ip_policy::is_allowed(addr.ip(), self.policy)) {
            return Err(blocked(format!("{host} resolves to an address modules may not reach")));
        }
        Ok(addrs)
    }

    /// One HTTP exchange for a module. Redirects come back as they are; the module fetches the
    /// `Location` itself, which goes through the same checks.
    pub async fn fetch(&self, hosts: &HostList, req: Request) -> Result<Response, HttpError> {
        self.fetch_up_to(hosts, req, limits::MAX_BODY).await
    }

    /// [`Net::fetch`] with a body of at most `max_body` bytes.
    pub(crate) async fn fetch_up_to(&self, hosts: &HostList, req: Request, max_body: usize) -> Result<Response, HttpError> {
        let url = url::Url::parse(&req.url).map_err(|_| blocked("not a valid URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(blocked("only http and https URLs"));
        }
        let addrs = self.resolve(hosts, &url).await?;
        let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|_| HttpError::Network("invalid method".to_string()))?;
        #[cfg_attr(not(any(test, feature = "test-support")), allow(unused_mut))]
        let mut headers = header_map(&req.headers)?;
        #[cfg(any(test, feature = "test-support"))]
        let url = match self.fake_server {
            Some(addr) => fake_url(url, addr, &mut headers)?,
            None => url,
        };
        // Through the system proxy the proxy connects; directly, only to the checked addresses.
        // The proxy comes from the static Windows settings (and the proxy environment variables):
        // reqwest does not run PAC scripts or WPAD, which spec 8.3 asks for. ponytail: PAC/WPAD
        // needs WinHTTP (WinHttpGetProxyForUrl), a Windows API binding for AGENTS.md's list.
        // ponytail: one client per request keeps each connection pinned to its own check; a
        // pool per host and address set if fetch volume ever matters.
        let mut client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.limits.connect_timeout)
            .read_timeout(self.limits.read_timeout);
        if let Some(domain) = url.domain() {
            client = client.resolve_to_addrs(domain, &addrs);
        }
        let client = client.build().map_err(network)?;
        let host = url.host_str().unwrap_or_default().to_string();
        let mut response = client.request(method.clone(), url).headers(headers).body(req.body).send().await.map_err(network)?;
        let status = response.status().as_u16();
        if matches!(status, 429 | 503)
            && let Some(secs) = retry_after(response.headers())
            && secs > limits::MAX_RETRY_AFTER
        {
            return Err(HttpError::Throttled(secs));
        }
        if response.content_length().is_some_and(|len| len > max_body as u64) {
            return Err(HttpError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network)? {
            if body.len().saturating_add(chunk.len()) > max_body {
                return Err(HttpError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        tracing::debug!(%method, host, status, bytes = body.len(), "module fetch");
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
            .collect();
        Ok(Response { status, headers, body })
    }
}
