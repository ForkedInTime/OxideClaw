//! Outbound network policy for model-driven fetches.
//!
//! `WebFetch` and `WebBrowser` run without an approval prompt, so a
//! prompt-injected turn could point them at the cloud metadata service
//! (`169.254.169.254`), a service bound to loopback, or an intranet host, and
//! read the response straight into the model's context. This module is the
//! single place that decides which destinations those tools may reach.
//!
//! Two tiers:
//!
//! - **Always denied**: link-local (the metadata service lives there),
//!   unspecified, multicast, broadcast, plus the metadata endpoints that sit
//!   outside link-local: Alibaba Cloud's `100.100.100.200` (inside CGNAT)
//!   and AWS's IPv6 IMDS `fd00:ec2::254` (inside ULA). No agent use case
//!   exists.
//! - **Private** (loopback, RFC 1918, CGNAT, ULA): denied unless the policy
//!   opts in. Developers do legitimately fetch `localhost:3000`, so the
//!   opt-in is a plain setting (`allowPrivateNetworkFetch`), and the
//!   user-driven CDP browser gets it by default.
//!
//! The check happens on the **resolved addresses**, not the hostname, and a
//! direct connection is pinned to exactly those addresses so a DNS answer
//! cannot change between the check and the connect. Public destinations go
//! through the proxy `HTTP(S)_PROXY` names, if any, which resolves the host
//! itself; private ones never do. Redirects are followed by hand so every
//! hop goes through the same check.

use anyhow::{Result, anyhow, bail};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio_stream::StreamExt;
use url::{Host, Url};

/// User agent for every model-driven fetch.
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; oxideclaw/",
    env!("CARGO_PKG_VERSION"),
    ")"
);

/// Cloud metadata endpoints that fall inside a *private* range rather than
/// link-local, so the private opt-in would otherwise let them through.
const METADATA_V4: [Ipv4Addr; 1] = [Ipv4Addr::new(100, 100, 100, 200)];
const METADATA_V6: [Ipv6Addr; 1] = [Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)];

/// Hard cap on redirect hops `fetch` will follow.
pub const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPolicy {
    /// Allow loopback, RFC 1918, CGNAT and ULA destinations.
    pub allow_private: bool,
}

impl NetPolicy {
    /// Policy for the unprompted fetch tools, from `allowPrivateNetworkFetch`.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        if cfg.allow_private_network_fetch {
            NetPolicy::LOCAL_OK
        } else {
            NetPolicy::STRICT
        }
    }

    /// Public internet only. The default for unprompted tools.
    pub const STRICT: NetPolicy = NetPolicy {
        allow_private: false,
    };
    /// Private networks allowed; link-local / metadata still denied.
    pub const LOCAL_OK: NetPolicy = NetPolicy {
        allow_private: true,
    };

    /// Reject an address this policy must not connect to.
    pub fn check_ip(&self, ip: IpAddr) -> Result<()> {
        // `::ffff:a.b.c.d` is the same wire destination as `a.b.c.d`;
        // classify the embedded v4 so the mapping is not an escape hatch.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        let always_denied = match ip {
            IpAddr::V4(v4) => {
                v4.is_unspecified()
                    || v4.is_link_local()
                    || v4.is_multicast()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || METADATA_V4.contains(&v4)
            }
            IpAddr::V6(v6) => {
                v6.is_unspecified()
                    || v6.is_multicast()
                    || v6.is_unicast_link_local()
                    || METADATA_V6.contains(&v6)
            }
        };
        if always_denied {
            bail!(
                "destination {ip} is link-local, reserved or a cloud metadata endpoint and is never fetched"
            );
        }
        let private = match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || is_cgnat(v4),
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local(),
        };
        if private && !self.allow_private {
            bail!(
                "destination {ip} is a private or loopback address; set allowPrivateNetworkFetch: true to permit it"
            );
        }
        Ok(())
    }

    /// Scheme + host + DNS check. Returns every address the host resolved
    /// to, all of which passed `check_ip`, so the caller can pin them.
    pub async fn resolve(&self, url: &Url) -> Result<Vec<SocketAddr>> {
        if !matches!(url.scheme(), "http" | "https") {
            bail!(
                "only http/https URLs can be fetched (got {}:)",
                url.scheme()
            );
        }
        let port = url
            .port_or_known_default()
            .ok_or_else(|| anyhow!("URL has no port: {url}"))?;
        let addrs: Vec<SocketAddr> = match url.host() {
            Some(Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(Host::Domain(name)) => tokio::net::lookup_host((name, port))
                .await
                .map_err(|e| anyhow!("could not resolve {name}: {e}"))?
                .collect(),
            None => bail!("URL has no host: {url}"),
        };
        if addrs.is_empty() {
            bail!(
                "{} did not resolve to any address",
                url.host_str().unwrap_or("host")
            );
        }
        // Every answer must pass: a mixed public/private answer is the
        // classic rebinding shape, and the connector may pick any of them.
        for a in &addrs {
            self.check_ip(a.ip())
                .map_err(|e| anyhow!("{}: {e}", url.host_str().unwrap_or("host")))?;
        }
        Ok(addrs)
    }
}

/// 100.64.0.0/10 (RFC 6598, carrier-grade NAT). Treated as private.
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// A response body read under the policy, with the URL it was finally
/// served from (after redirects).
#[derive(Debug)]
pub struct Fetched {
    pub final_url: Url,
    pub status: reqwest::StatusCode,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// GET `url` under `policy`: every hop is resolved and checked before any
/// connection is made, redirects are followed manually (max
/// [`MAX_REDIRECTS`]), and the body is refused — before or during the read —
/// once it exceeds `max_bytes`.
pub async fn fetch(
    url: &str,
    policy: &NetPolicy,
    max_bytes: usize,
    timeout: std::time::Duration,
) -> Result<Fetched> {
    fetch_with_env(url, policy, max_bytes, timeout, |k| {
        std::env::var(k).ok().filter(|v| !v.trim().is_empty())
    })
    .await
}

/// `fetch` with the proxy variables read through `env`.
async fn fetch_with_env(
    url: &str,
    policy: &NetPolicy,
    max_bytes: usize,
    timeout: std::time::Duration,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Fetched> {
    let mut current = Url::parse(url).map_err(|e| anyhow!("invalid URL {url:?}: {e}"))?;
    for _ in 0..=MAX_REDIRECTS {
        let host = current
            .host_str()
            .ok_or_else(|| anyhow!("URL has no host: {current}"))?
            .to_string();
        // Per scheme, as reqwest's own system-proxy lookup does.
        let vars: &[&str] = if current.scheme() == "https" {
            &["HTTPS_PROXY", "ALL_PROXY"]
        } else {
            &["HTTP_PROXY", "ALL_PROXY"]
        };
        let chain = EnvProxy::from_vars(&env, vars)
            .filter(|p| !no_proxy_covers(&p.no_proxy, &host))
            .map(|p| p.url);
        // `no_proxy` drops reqwest's implicit HTTP(S)_PROXY, which would
        // send even a pinned, policy-checked host to a proxy that resolves
        // it again, and could not resolve a name only that proxy knows.
        // Redirects are disabled so each hop comes back through `route`.
        let builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(USER_AGENT);
        let client = match route(policy, &current, chain).await? {
            Route::Direct(addrs) => builder.resolve_to_addrs(&host, &addrs),
            Route::Upstream(proxy) => builder.proxy(reqwest::Proxy::all(proxy.as_str())?),
        }
        .build()?;
        let resp = client.get(current.clone()).send().await?;
        let status = resp.status();

        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok());
            if let Some(loc) = location {
                current = current
                    .join(loc)
                    .map_err(|e| anyhow!("bad redirect target {loc:?}: {e}"))?;
                continue;
            }
            // A 3xx with no Location is just a response; fall through.
        }

        if let Some(len) = resp.content_length()
            && len > max_bytes as u64
        {
            bail!("response too large: {len} bytes exceeds the {max_bytes}-byte limit");
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > max_bytes {
                bail!("response too large: exceeds the {max_bytes}-byte limit");
            }
            body.extend_from_slice(&chunk);
        }
        return Ok(Fetched {
            final_url: current,
            status,
            content_type,
            body,
        });
    }
    bail!("too many redirects (more than {MAX_REDIRECTS} hops)")
}

/// A loopback forward proxy that applies a [`NetPolicy`] to every connection
/// a child process makes. Headless Chromium follows redirects, meta refresh
/// and JS navigation and resolves DNS itself, so checking only the URL it is
/// launched with lets a public page bounce it to `169.254.169.254`. Routed
/// through this proxy, every hop and subresource is resolved and checked here
/// and the connection is pinned to the addresses that passed.
///
/// Dropping it stops the listener and every open tunnel.
pub struct PolicyProxy {
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PolicyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Chromium flags that force every connection through `proxy`.
pub fn chromium_proxy_args(proxy: SocketAddr) -> [String; 3] {
    [
        format!("--proxy-server=http://{proxy}"),
        // Chromium implicitly bypasses proxies for localhost; `<-loopback>`
        // removes that, so loopback goes through the policy too.
        "--proxy-bypass-list=<-loopback>".into(),
        // WebRTC would otherwise send UDP straight past the proxy.
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into(),
    ]
}

/// Request heads larger than this are refused; no legitimate request needs it.
const MAX_PROXY_HEAD: usize = 64 * 1024;

///
/// Chromium launched without this proxy follows `HTTP(S)_PROXY` (Linux) or
/// the system proxy; pinned to it, it would lose all access on a network
/// that requires an egress proxy. So public destinations are chained
/// through the proxy the environment names (see [`Upstream::from_env`]),
/// after the same policy check.
pub async fn spawn_policy_proxy(policy: NetPolicy) -> Result<PolicyProxy> {
    spawn_policy_proxy_with(policy, Upstream::from_env()).await
}

/// An HTTP proxy the policy proxy forwards public destinations through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    host: String,
    port: u16,
    /// `Proxy-Authorization` value from the URL's userinfo.
    auth: Option<String>,
    /// `NO_PROXY` entries, lower-case, without a leading `.` or `*.`.
    no_proxy: Vec<String>,
}

/// The proxy the environment names and its `NO_PROXY` list.
struct EnvProxy {
    /// The proxy URL, `http://` added to a bare `host:port`.
    url: Url,
    /// `NO_PROXY` entries, lower-case, without a leading `.` or `*.`.
    no_proxy: Vec<String>,
}

impl EnvProxy {
    /// The first of `vars` (either case) that is set, with `NO_PROXY`.
    fn from_vars(get: impl Fn(&str) -> Option<String>, vars: &[&str]) -> Option<Self> {
        let either = |k: &str| get(k).or_else(|| get(&k.to_ascii_lowercase()));
        let raw = vars.iter().find_map(|k| either(k))?;
        let raw = raw.trim();
        let with_scheme = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("http://{raw}")
        };
        let Ok(url) = Url::parse(&with_scheme) else {
            tracing::warn!("proxy {raw:?} is not a valid URL; connecting directly");
            return None;
        };
        let no_proxy = either("NO_PROXY")
            .unwrap_or_default()
            .split(',')
            .map(|e| {
                let e = e.trim().to_ascii_lowercase();
                let e = e.trim_start_matches("*.").trim_start_matches('.');
                // `host:port` entries: the host part is what is compared.
                match e.rsplit_once(':') {
                    Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => h.to_string(),
                    _ => e.to_string(),
                }
            })
            .filter(|e| !e.is_empty())
            .collect();
        Some(Self { url, no_proxy })
    }
}

/// `no_proxy` covers `host` (itself or a parent domain, or `*`).
fn no_proxy_covers(no_proxy: &[String], host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    no_proxy.iter().any(|e| {
        e == "*"
            || host == *e
            || host
                .strip_suffix(e.as_str())
                .is_some_and(|r| r.ends_with('.'))
    })
}

impl Upstream {
    /// The proxy `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` (either case,
    /// in that order) names, with `NO_PROXY`. Only `http://` proxies (or a
    /// bare `host:port`) can be chained; anything else is ignored.
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    fn from_vars(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let EnvProxy { url, no_proxy } =
            EnvProxy::from_vars(get, &["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"])?;
        if url.scheme() != "http" {
            tracing::warn!(
                "browser: proxy {url} is not an http:// proxy; launched Chrome connects directly"
            );
            return None;
        }
        let host = url.host_str()?.to_string();
        let port = url.port_or_known_default()?;
        let auth = (!url.username().is_empty()).then(|| {
            use base64::Engine as _;
            let creds = format!(
                "{}:{}",
                percent_decode(url.username()),
                percent_decode(url.password().unwrap_or(""))
            );
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(creds)
            )
        });
        Some(Self {
            host,
            port,
            auth,
            no_proxy,
        })
    }

    /// `NO_PROXY` covers `host` (itself or a parent domain, or `*`).
    fn bypasses(&self, host: &str) -> bool {
        no_proxy_covers(&self.no_proxy, host)
    }
}

/// `%XX` decoding for proxy userinfo.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where one connection goes after the policy check.
enum Route<P> {
    /// Straight to these checked addresses.
    Direct(Vec<SocketAddr>),
    /// Through the environment's proxy, which resolves the host itself.
    Upstream(P),
}

/// Route a connection to `url` under `policy`, given the proxy (NO_PROXY
/// already applied) the environment names for it. The policy check always
/// runs first. Public destinations then go through the proxy; loopback/LAN
/// targets (allowed by the policy) connect directly. A name that does not
/// resolve here may still resolve at the proxy (a network whose only way
/// out is that proxy): the hostname checks applied, but the address pin
/// cannot, since the proxy does its own DNS.
async fn route<P>(policy: &NetPolicy, url: &Url, chain: Option<P>) -> Result<Route<P>> {
    match policy.resolve(url).await {
        Ok(addrs) => Ok(match chain {
            Some(up)
                if addrs
                    .iter()
                    .all(|a| NetPolicy::STRICT.check_ip(a.ip()).is_ok()) =>
            {
                Route::Upstream(up)
            }
            _ => Route::Direct(addrs),
        }),
        Err(e) => {
            let unresolvable = matches!(url.scheme(), "http" | "https")
                && match (url.host(), url.port_or_known_default()) {
                    (Some(Host::Domain(d)), Some(port)) => {
                        tokio::net::lookup_host((d, port)).await.is_err()
                    }
                    _ => false,
                };
            match chain {
                Some(up) if unresolvable => Ok(Route::Upstream(up)),
                _ => Err(e),
            }
        }
    }
}

async fn spawn_policy_proxy_with(
    policy: NetPolicy,
    upstream: Option<Upstream>,
) -> Result<PolicyProxy> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        // Owned by this task, so aborting it on drop tears down every tunnel.
        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((sock, _)) => {
                        conns.spawn(proxy_one(sock, policy, upstream.clone()));
                    }
                    Err(_) => break,
                },
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    });
    Ok(PolicyProxy { addr, task })
}

async fn proxy_one(
    mut client: tokio::net::TcpStream,
    policy: NetPolicy,
    upstream: Option<Upstream>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_PROXY_HEAD {
            return;
        }
        let mut chunk = [0u8; 4096];
        match client.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let rest = buf[head_end..].to_vec();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        let _ = client
            .write_all(&refusal("400 Bad Request", "malformed request"))
            .await;
        return;
    };
    let connect = method.eq_ignore_ascii_case("CONNECT");
    // CONNECT carries authority-form `host:port`; everything else must be
    // absolute-form, which is what a browser sends to an HTTP proxy.
    let url = if connect {
        Url::parse(&format!("https://{target}/"))
    } else {
        Url::parse(target)
    };
    let url = match url {
        Ok(u) if connect || u.scheme() == "http" => u,
        _ => {
            let _ = client
                .write_all(&refusal("400 Bad Request", "unsupported request target"))
                .await;
            return;
        }
    };
    let host = url.host_str().unwrap_or("").to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    // NO_PROXY hosts connect directly, after the same check.
    let chain = upstream.filter(|u| !u.bypasses(&host));
    let route = match route(&policy, &url, chain).await {
        Ok(r) => r,
        Err(e) => {
            let _ = client
                .write_all(&refusal(
                    "403 Forbidden",
                    &format!("Blocked by OxideClaw network policy: {e}"),
                ))
                .await;
            return;
        }
    };
    let (mut upstream, via) = match route {
        Route::Direct(addrs) => match tokio::net::TcpStream::connect(&addrs[..]).await {
            Ok(s) => (s, None),
            Err(_) => {
                let _ = client
                    .write_all(&refusal("502 Bad Gateway", "could not connect"))
                    .await;
                return;
            }
        },
        Route::Upstream(up) => {
            match tokio::net::TcpStream::connect((up.host.as_str(), up.port)).await {
                Ok(s) => (s, Some(up)),
                Err(_) => {
                    let _ = client
                        .write_all(&refusal("502 Bad Gateway", "could not reach the proxy"))
                        .await;
                    return;
                }
            }
        }
    };

    if connect && let Some(up) = &via {
        // Open the tunnel at the upstream proxy first; only its 2xx is
        // passed on as ours.
        let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
        if let Some(a) = &up.auth {
            req.push_str(&format!("Proxy-Authorization: {a}\r\n"));
        }
        req.push_str("\r\n");
        if upstream.write_all(req.as_bytes()).await.is_err() {
            return;
        }
        let mut head = Vec::new();
        let extra = loop {
            if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                break head.split_off(i + 4);
            }
            if head.len() > MAX_PROXY_HEAD {
                return;
            }
            let mut chunk = [0u8; 4096];
            match upstream.read(&mut chunk).await {
                Ok(0) | Err(_) => {
                    let _ = client
                        .write_all(&refusal("502 Bad Gateway", "the proxy closed the tunnel"))
                        .await;
                    return;
                }
                Ok(n) => head.extend_from_slice(&chunk[..n]),
            }
        };
        let status = String::from_utf8_lossy(&head);
        let ok = status
            .split_whitespace()
            .nth(1)
            .is_some_and(|c| c.starts_with('2'));
        if !ok {
            let line = status.lines().next().unwrap_or("").to_string();
            let _ = client
                .write_all(&refusal(
                    "502 Bad Gateway",
                    &format!("the upstream proxy refused the tunnel: {line}"),
                ))
                .await;
            return;
        }
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
            || (!extra.is_empty() && client.write_all(&extra).await.is_err())
        {
            return;
        }
    } else if connect {
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
    } else {
        // Origin-form for the server (absolute-form for an upstream proxy),
        // and `Connection: close` so a reused proxy connection can never
        // carry a request for another host to this already-checked upstream.
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path.push('?');
            path.push_str(q);
        }
        let target = if via.is_some() {
            url.as_str().to_string()
        } else {
            path
        };
        let mut out = format!("{method} {target} {version}\r\n");
        if let Some(a) = via.as_ref().and_then(|u| u.auth.as_ref()) {
            out.push_str(&format!("Proxy-Authorization: {a}\r\n"));
        }
        for line in lines.filter(|l| !l.is_empty()) {
            let name = line.split(':').next().unwrap_or("").trim();
            if name.eq_ignore_ascii_case("connection")
                || name.eq_ignore_ascii_case("proxy-connection")
                || name.eq_ignore_ascii_case("proxy-authorization")
                || name.eq_ignore_ascii_case("keep-alive")
            {
                continue;
            }
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.push_str("Connection: close\r\n\r\n");
        if upstream.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
    if !rest.is_empty() && upstream.write_all(&rest).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

fn refusal(status: &str, msg: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{msg}",
        msg.len()
    )
    .into_bytes()
}

/// A scripted loopback HTTP server for the fetch tools' tests.
#[cfg(test)]
pub mod test_support {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves `script[i]` verbatim for the i-th connection, repeating the
    /// last entry once exhausted. Returns the base URL and a hit counter.
    pub async fn scripted_server(script: Vec<String>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let i = counter.fetch_add(1, Ordering::SeqCst);
                let body = script
                    .get(i)
                    .unwrap_or_else(|| script.last().unwrap())
                    .clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    pub fn ok_with(content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    pub fn ok(body: &str) -> String {
        ok_with("text/plain", body)
    }

    pub fn redirect(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::atomic::Ordering;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    // ── address classification ───────────────────────────────────────────

    #[test]
    fn link_local_and_metadata_are_denied_even_when_private_is_allowed() {
        for a in [
            "169.254.169.254",
            "169.254.0.1",
            "fe80::1",
            // Alibaba Cloud (inside CGNAT) and AWS IPv6 IMDS (inside ULA).
            "100.100.100.200",
            "fd00:ec2::254",
            "::ffff:100.100.100.200",
        ] {
            assert!(NetPolicy::LOCAL_OK.check_ip(ip(a)).is_err(), "{a}");
            assert!(NetPolicy::STRICT.check_ip(ip(a)).is_err(), "{a}");
        }
    }

    #[test]
    fn unspecified_multicast_and_broadcast_are_always_denied() {
        for a in ["0.0.0.0", "::", "224.0.0.1", "ff02::1", "255.255.255.255"] {
            assert!(NetPolicy::LOCAL_OK.check_ip(ip(a)).is_err(), "{a}");
        }
    }

    #[test]
    fn private_ranges_are_denied_by_strict_and_allowed_by_local_ok() {
        for a in [
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "fd12:3456::1",
        ] {
            assert!(NetPolicy::STRICT.check_ip(ip(a)).is_err(), "{a}");
            assert!(NetPolicy::LOCAL_OK.check_ip(ip(a)).is_ok(), "{a}");
        }
    }

    #[test]
    fn ipv4_mapped_ipv6_is_classified_as_the_embedded_v4() {
        assert!(
            NetPolicy::LOCAL_OK
                .check_ip(ip("::ffff:169.254.169.254"))
                .is_err()
        );
        assert!(NetPolicy::STRICT.check_ip(ip("::ffff:127.0.0.1")).is_err());
        assert!(NetPolicy::LOCAL_OK.check_ip(ip("::ffff:127.0.0.1")).is_ok());
        assert!(
            NetPolicy::STRICT
                .check_ip(ip("::ffff:93.184.216.34"))
                .is_ok()
        );
    }

    #[test]
    fn public_addresses_pass_strict() {
        for a in ["93.184.216.34", "8.8.8.8", "2606:4700::1111"] {
            assert!(NetPolicy::STRICT.check_ip(ip(a)).is_ok(), "{a}");
        }
    }

    // ── URL resolution ───────────────────────────────────────────────────

    #[tokio::test]
    async fn non_http_schemes_are_rejected() {
        for u in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "javascript:alert(1)",
            "data:text/html,hi",
        ] {
            let url = Url::parse(u).unwrap();
            assert!(NetPolicy::LOCAL_OK.resolve(&url).await.is_err(), "{u}");
        }
    }

    #[tokio::test]
    async fn literal_metadata_ip_is_rejected_without_dns() {
        let url = Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        let err = NetPolicy::LOCAL_OK.resolve(&url).await.unwrap_err();
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
    }

    #[tokio::test]
    async fn localhost_hostname_resolves_to_a_private_address() {
        let url = Url::parse("http://localhost:1/").unwrap();
        assert!(NetPolicy::STRICT.resolve(&url).await.is_err());
        let addrs = NetPolicy::LOCAL_OK.resolve(&url).await.unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback() && a.port() == 1));
    }

    // ── fetch ────────────────────────────────────────────────────────────

    const SECS: std::time::Duration = std::time::Duration::from_secs(5);

    #[tokio::test]
    async fn strict_policy_never_connects_to_a_loopback_server() {
        let (base, hits) = scripted_server(vec![ok("secret")]).await;
        let err = fetch(&base, &NetPolicy::STRICT, 1 << 20, SECS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private"), "{err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "must be refused before connecting"
        );
    }

    #[tokio::test]
    async fn allowed_redirect_is_followed_and_final_url_reported() {
        let (base, hits) = scripted_server(vec![redirect("/final"), ok("done")]).await;
        let got = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS)
            .await
            .unwrap();
        assert_eq!(got.body, b"done");
        assert_eq!(got.status, 200);
        assert!(
            got.final_url.as_str().ends_with("/final"),
            "{}",
            got.final_url
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn redirect_to_a_denied_address_is_refused() {
        let (base, hits) = scripted_server(vec![redirect("http://169.254.169.254/latest/")]).await;
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "only the first hop may connect"
        );
    }

    #[tokio::test]
    async fn redirect_loop_stops_after_max_hops() {
        let (base, hits) = scripted_server(vec![redirect("/again")]).await;
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("redirect"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), MAX_REDIRECTS + 1);
    }

    #[tokio::test]
    async fn oversized_content_length_is_refused_before_reading_the_body() {
        let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                    content-length: 10000000\r\nconnection: close\r\n\r\nx";
        let (base, _) = scripted_server(vec![resp.to_string()]).await;
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1000, SECS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn body_without_content_length_is_capped_while_streaming() {
        let big = "y".repeat(5000);
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n{big}"
        );
        let (base, _) = scripted_server(vec![resp]).await;
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1000, SECS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn body_within_cap_is_returned_with_content_type() {
        let (base, _) = scripted_server(vec![ok("hello")]).await;
        let got = fetch(&base, &NetPolicy::LOCAL_OK, 1000, SECS)
            .await
            .unwrap();
        assert_eq!(got.body, b"hello");
        assert_eq!(got.content_type, "text/plain");
    }

    // ── policy proxy (WebBrowser's Chromium path) ────────────────────────

    /// Send one raw request through the proxy and return everything it
    /// answers until the connection closes.
    async fn via_proxy(proxy: &PolicyProxy, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(proxy.addr).await.unwrap();
        sock.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(SECS, sock.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Chromium followed a public page's redirect to the metadata service or
    /// loopback unchecked. Through the proxy, each hop is refused before any
    /// connection is made, for plain HTTP and CONNECT tunnels alike.
    #[tokio::test]
    async fn proxy_refuses_hops_the_policy_denies() {
        let (base, hits) = scripted_server(vec![ok("secret")]).await;
        let authority = base.trim_start_matches("http://");
        let strict = spawn_policy_proxy_with(NetPolicy::STRICT, None)
            .await
            .unwrap();
        for req in [
            format!("GET {base}/ HTTP/1.1\r\nHost: {authority}\r\n\r\n"),
            format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"),
            "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\n\r\n".to_string(),
            "CONNECT 169.254.169.254:443 HTTP/1.1\r\n\r\n".to_string(),
        ] {
            let got = via_proxy(&strict, &req).await;
            assert!(got.starts_with("HTTP/1.1 403"), "{req:?} -> {got}");
            assert!(!got.contains("secret"));
        }
        let local = spawn_policy_proxy_with(NetPolicy::LOCAL_OK, None)
            .await
            .unwrap();
        let got = via_proxy(
            &local,
            "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\n\r\n",
        )
        .await;
        assert!(got.starts_with("HTTP/1.1 403"), "{got}");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing may connect");
    }

    #[tokio::test]
    async fn proxy_forwards_allowed_requests_and_tunnels() {
        let (base, hits) = scripted_server(vec![ok("hello")]).await;
        let authority = base.trim_start_matches("http://");
        let proxy = spawn_policy_proxy_with(NetPolicy::LOCAL_OK, None)
            .await
            .unwrap();

        let got = via_proxy(
            &proxy,
            &format!("GET {base}/x?y=1 HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n\r\n"),
        )
        .await;
        assert!(got.starts_with("HTTP/1.1 200"), "{got}");
        assert!(got.ends_with("hello"), "{got}");

        let got = via_proxy(
            &proxy,
            &format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n\
                 GET / HTTP/1.1\r\nHost: {authority}\r\n\r\n"
            ),
        )
        .await;
        assert!(
            got.starts_with("HTTP/1.1 200 Connection Established"),
            "{got}"
        );
        assert!(got.ends_with("hello"), "{got}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    // ── upstream proxy chaining ──────────────────────────────────────────

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn upstream_comes_from_the_proxy_variables() {
        let up = Upstream::from_vars(vars(&[
            ("https_proxy", "http://u%40corp:p%3Ass@proxy.corp:3128"),
            ("HTTP_PROXY", "http://other:1"),
            ("NO_PROXY", "localhost, .internal.corp,*.lan,10.0.0.1:8080"),
        ]))
        .unwrap();
        assert_eq!((up.host.as_str(), up.port), ("proxy.corp", 3128));
        use base64::Engine as _;
        let creds = base64::engine::general_purpose::STANDARD.encode("u@corp:p:ss");
        assert_eq!(up.auth, Some(format!("Basic {creds}")));
        assert!(up.bypasses("localhost"));
        assert!(up.bypasses("git.internal.corp"));
        assert!(up.bypasses("printer.lan"));
        assert!(up.bypasses("10.0.0.1"));
        assert!(!up.bypasses("example.com"));
        assert!(!up.bypasses("notinternal.corp"));

        let bare = Upstream::from_vars(vars(&[("ALL_PROXY", "proxy:8080")])).unwrap();
        assert_eq!(
            (bare.host.as_str(), bare.port, bare.auth),
            ("proxy", 8080, None)
        );
        assert!(Upstream::from_vars(vars(&[("ALL_PROXY", "socks5://proxy:1080")])).is_none());
        assert!(Upstream::from_vars(vars(&[])).is_none());
    }

    /// A stand-in egress proxy: records each request head and answers it.
    async fn fake_upstream(answer: &'static str) -> (Upstream, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let _ = sock.write_all(answer.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let up = Upstream {
            host: "127.0.0.1".into(),
            port,
            auth: Some("Basic dTpw".into()),
            no_proxy: Vec::new(),
        };
        (up, seen)
    }

    use std::sync::Arc;

    /// Launched Chrome was pinned to the policy proxy, which connected
    /// directly: behind an egress proxy every page failed. Public hosts now
    /// go through the environment's proxy, after the policy check.
    #[tokio::test]
    async fn public_destinations_are_chained_through_the_upstream_proxy() {
        let (up, seen) =
            fake_upstream("HTTP/1.1 200 Connection established\r\n\r\ntunnel-data").await;
        let proxy = spawn_policy_proxy_with(NetPolicy::STRICT, Some(up))
            .await
            .unwrap();
        // 93.184.215.14 is a public literal: no DNS needed for the check.
        let got = via_proxy(&proxy, "CONNECT 93.184.215.14:443 HTTP/1.1\r\n\r\n").await;
        assert!(
            got.starts_with("HTTP/1.1 200 Connection Established"),
            "{got}"
        );
        assert!(got.ends_with("tunnel-data"), "{got}");
        let got = via_proxy(
            &proxy,
            "GET http://93.184.215.14/a?b=1 HTTP/1.1\r\nHost: 93.184.215.14\r\n\r\n",
        )
        .await;
        assert!(got.contains("tunnel-data"), "{got}");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[0].starts_with("CONNECT 93.184.215.14:443 HTTP/1.1\r\n"),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].contains("Proxy-Authorization: Basic dTpw"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("GET http://93.184.215.14/a?b=1 HTTP/1.1\r\n"),
            "{}",
            seen[1]
        );
        assert!(
            seen[1].contains("Proxy-Authorization: Basic dTpw"),
            "{}",
            seen[1]
        );
    }

    /// reqwest applied HTTP(S)_PROXY on its own, so `fetch` sent pinned
    /// hosts to the proxy anyway, and a name only the proxy can resolve
    /// failed the local lookup. Public and locally unresolvable hosts now go
    /// through the proxy (with its credentials); private ones never do.
    #[tokio::test]
    async fn fetch_routes_public_hosts_through_the_env_proxy() {
        let (up, seen) = fake_upstream(
            "HTTP/1.1 200 OK\r\ncontent-length: 9\r\nconnection: close\r\n\r\nvia-proxy",
        )
        .await;
        let proxy = format!("http://u:p@127.0.0.1:{}", up.port);
        let env = vars(&[("HTTP_PROXY", proxy.as_str())]);
        let t = std::time::Duration::from_secs(10);
        for url in [
            "http://oxideclaw-proxy-only.invalid/page",
            "http://93.184.215.14/x",
        ] {
            let got = fetch_with_env(url, &NetPolicy::STRICT, 1024, t, &env)
                .await
                .unwrap();
            assert_eq!(got.body, b"via-proxy", "{url}");
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[0].starts_with("GET http://oxideclaw-proxy-only.invalid/page HTTP/1.1\r\n"),
            "{}",
            seen[0]
        );
        assert!(
            seen[0]
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic dtpw"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("GET http://93.184.215.14/x "),
            "{}",
            seen[1]
        );
    }

    #[tokio::test]
    async fn fetch_keeps_private_hosts_off_the_env_proxy() {
        let (up, seen) = fake_upstream("HTTP/1.1 200 OK\r\n\r\nfrom-proxy").await;
        let proxy = format!("127.0.0.1:{}", up.port);
        let env = vars(&[
            ("HTTP_PROXY", proxy.as_str()),
            ("ALL_PROXY", proxy.as_str()),
        ]);
        let t = std::time::Duration::from_secs(10);
        let (base, hits) = scripted_server(vec![ok("local")]).await;

        let err = fetch_with_env(&base, &NetPolicy::STRICT, 1024, t, &env)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private"), "{err}");
        let err = fetch_with_env(
            "http://169.254.169.254/",
            &NetPolicy::LOCAL_OK,
            1024,
            t,
            &env,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("metadata"), "{err}");

        let got = fetch_with_env(&base, &NetPolicy::LOCAL_OK, 1024, t, &env)
            .await
            .unwrap();
        assert_eq!(got.body, b"local");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing may reach the proxy"
        );
    }

    /// The upstream proxy must not become a way around the policy, and
    /// loopback targets the policy allows still connect directly.
    #[tokio::test]
    async fn the_policy_still_applies_and_lan_targets_stay_direct() {
        let (up, seen) = fake_upstream("HTTP/1.1 200 OK\r\n\r\nfrom-proxy").await;
        let strict = spawn_policy_proxy_with(NetPolicy::STRICT, Some(up.clone()))
            .await
            .unwrap();
        for req in [
            "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\n\r\n",
            "CONNECT 169.254.169.254:443 HTTP/1.1\r\n\r\n",
            "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n",
        ] {
            let got = via_proxy(&strict, req).await;
            assert!(got.starts_with("HTTP/1.1 403"), "{req:?} -> {got}");
        }

        let (base, hits) = scripted_server(vec![ok("local")]).await;
        let local = spawn_policy_proxy_with(NetPolicy::LOCAL_OK, Some(up))
            .await
            .unwrap();
        let got = via_proxy(&local, &format!("GET {base}/ HTTP/1.1\r\n\r\n")).await;
        assert!(got.ends_with("local"), "{got}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing may reach the proxy"
        );
    }
}
