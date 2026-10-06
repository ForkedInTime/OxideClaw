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
//!   unspecified, multicast, broadcast. No agent use case exists.
//! - **Private** (loopback, RFC 1918, CGNAT, ULA): denied unless the policy
//!   opts in. Developers do legitimately fetch `localhost:3000`, so the
//!   opt-in is a plain setting (`allowPrivateNetworkFetch`), and the
//!   user-driven CDP browser gets it by default.
//!
//! The check happens on the **resolved addresses**, not the hostname, and
//! `fetch` pins the connection to exactly those addresses so a DNS answer
//! cannot change between the check and the connect. Redirects are followed
//! by hand so every hop goes through the same check.

use anyhow::{Result, anyhow, bail};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio_stream::StreamExt;
use url::{Host, Url};

/// User agent for every model-driven fetch.
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; oxideclaw/",
    env!("CARGO_PKG_VERSION"),
    ")"
);

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
            }
            IpAddr::V6(v6) => {
                v6.is_unspecified() || v6.is_multicast() || v6.is_unicast_link_local()
            }
        };
        if always_denied {
            bail!(
                "destination {ip} is link-local or reserved and is never fetched (cloud metadata lives there)"
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
    let mut current = Url::parse(url).map_err(|e| anyhow!("invalid URL {url:?}: {e}"))?;
    for _ in 0..=MAX_REDIRECTS {
        let addrs = policy.resolve(&current).await?;
        let host = current
            .host_str()
            .ok_or_else(|| anyhow!("URL has no host: {current}"))?
            .to_string();
        // Pin the connection to the addresses that passed the check.
        // Redirects are disabled so each hop comes back through `resolve`.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&host, &addrs)
            .timeout(timeout)
            .user_agent(USER_AGENT)
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

/// Request heads larger than this are refused; no legitimate request needs it.
const MAX_PROXY_HEAD: usize = 64 * 1024;

pub async fn spawn_policy_proxy(policy: NetPolicy) -> Result<PolicyProxy> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        // Owned by this task, so aborting it on drop tears down every tunnel.
        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((sock, _)) => {
                        conns.spawn(proxy_one(sock, policy));
                    }
                    Err(_) => break,
                },
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    });
    Ok(PolicyProxy { addr, task })
}

async fn proxy_one(mut client: tokio::net::TcpStream, policy: NetPolicy) {
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
    let addrs = match policy.resolve(&url).await {
        Ok(a) => a,
        Err(e) => {
            let _ = client
                .write_all(&refusal(
                    "403 Forbidden",
                    &format!("WebBrowser refused: {e}"),
                ))
                .await;
            return;
        }
    };
    let Ok(mut upstream) = tokio::net::TcpStream::connect(&addrs[..]).await else {
        let _ = client
            .write_all(&refusal("502 Bad Gateway", "could not connect"))
            .await;
        return;
    };

    if connect {
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
    } else {
        // Origin-form for the server, and `Connection: close` so a reused
        // proxy connection can never carry a request for another host to
        // this already-checked upstream.
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path.push('?');
            path.push_str(q);
        }
        let mut out = format!("{method} {path} {version}\r\n");
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
        for a in ["169.254.169.254", "169.254.0.1", "fe80::1"] {
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
        let strict = spawn_policy_proxy(NetPolicy::STRICT).await.unwrap();
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
        let local = spawn_policy_proxy(NetPolicy::LOCAL_OK).await.unwrap();
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
        let proxy = spawn_policy_proxy(NetPolicy::LOCAL_OK).await.unwrap();

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
}
