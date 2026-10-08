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
//!   opt-in is a plain setting (`allowPrivateNetworkFetch`). The CDP
//!   browser follows the same setting; without it, an interactive session
//!   asks once per loopback service it opens ([`LoopbackGrants`]).
//!
//! The check happens on the **resolved addresses**, not the hostname, and a
//! direct connection is pinned to exactly those addresses so a DNS answer
//! cannot change between the check and the connect. With a proxy in the
//! environment (`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`, minus
//! `NO_PROXY`) the name is still resolved and checked here first; public
//! destinations then go through the proxy, which resolves the host again
//! itself, and private ones (when allowed) connect directly. A name that
//! does not resolve here goes to the proxy only if it looks public: the
//! proxy is treated as trusted egress for those. Redirects are followed by
//! hand so every hop goes through the same check.

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

/// Defaults to [`NetPolicy::STRICT`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

    /// `check_ip` for a connection to `addr`, which `host` (the URL's host)
    /// resolved to. A loopback destination also passes when `grants` covers
    /// `host` at that port ([`LoopbackGrants::covers`]). Grants only lift
    /// the private tier, so link-local and metadata addresses never pass.
    pub fn check_addr(
        &self,
        host: &str,
        addr: SocketAddr,
        grants: Option<&LoopbackGrants>,
    ) -> Result<()> {
        match self.check_ip(addr.ip()) {
            Err(_) if grants.is_some_and(|g| g.covers(host, addr)) => Ok(()),
            r => r,
        }
    }

    /// The CDP browser's check of `addrs`, what `url` resolved to: the
    /// always-denied tier and (without the setting) the LAN fail; loopback
    /// addresses refused only for want of a grant come back.
    fn loopback_need(
        &self,
        url: &Url,
        addrs: &[SocketAddr],
        grants: &LoopbackGrants,
    ) -> Result<Vec<SocketAddr>> {
        let host = url.host_str().unwrap_or("host");
        // The always-denied tier first: no grant can lift it.
        NetPolicy::LOCAL_OK.check_addrs(url, addrs, None)?;
        let mut need = Vec::new();
        for a in addrs {
            if let Err(e) = self.check_addr(host, *a, Some(grants)) {
                if !is_loopback(a.ip()) {
                    return Err(anyhow!("{host}: {e}"));
                }
                need.push(*a);
            }
        }
        Ok(need)
    }

    /// The CDP browser is about to open `url`; check it the way its policy
    /// proxy will check the connection. Returns the loopback addresses that
    /// are refused only for want of a grant (empty: go ahead), so the caller
    /// can ask the user once and [`LoopbackGrants::grant`] them. A name that
    /// does not resolve here passes when an upstream proxy would resolve it
    /// and its shape is allowed, as in the proxy.
    pub async fn check_browser_url(
        &self,
        url: &Url,
        grants: &LoopbackGrants,
    ) -> Result<Vec<SocketAddr>> {
        self.check_browser_url_with(url, grants, Upstream::from_env, &lookup_system)
            .await
    }

    async fn check_browser_url_with<L, F>(
        &self,
        url: &Url,
        grants: &LoopbackGrants,
        upstream: impl FnOnce() -> Option<Upstream>,
        lookup: &L,
    ) -> Result<Vec<SocketAddr>>
    where
        L: Fn(String, u16) -> F,
        F: Future<Output = std::io::Result<Vec<SocketAddr>>>,
    {
        let host = url.host_str().unwrap_or("host");
        let addrs = match addresses(url, lookup).await? {
            Ok(a) => a,
            Err(e) => {
                let chain = upstream().filter(|u| !u.bypasses(host));
                return match (chain, url.host()) {
                    (Some(_), Some(Host::Domain(name))) => {
                        self.check_unresolved_name(name)?;
                        Ok(Vec::new())
                    }
                    _ => Err(e),
                };
            }
        };
        self.loopback_need(url, &addrs, grants)
    }

    /// Scheme + host + DNS check. Returns every address the host resolved
    /// to, all of which passed `check_ip`, so the caller can pin them.
    /// Test-only: callers use `check_browser_url` or `fetch`, which also
    /// handle names only an upstream proxy can resolve.
    #[cfg(test)]
    pub async fn resolve(&self, url: &Url) -> Result<Vec<SocketAddr>> {
        self.resolve_with(url, &lookup_system).await
    }

    /// `resolve` with the name lookup done by `lookup`.
    #[cfg(test)]
    async fn resolve_with<L, F>(&self, url: &Url, lookup: &L) -> Result<Vec<SocketAddr>>
    where
        L: Fn(String, u16) -> F,
        F: Future<Output = std::io::Result<Vec<SocketAddr>>>,
    {
        let addrs = addresses(url, lookup).await??;
        self.check_addrs(url, &addrs, None)?;
        Ok(addrs)
    }

    /// Every answer must pass: a mixed public/private answer is the classic
    /// rebinding shape, and the connector may pick any of them.
    fn check_addrs(
        &self,
        url: &Url,
        addrs: &[SocketAddr],
        grants: Option<&LoopbackGrants>,
    ) -> Result<()> {
        let host = url.host_str().unwrap_or("host");
        for a in addrs {
            self.check_addr(host, *a, grants)
                .map_err(|e| anyhow!("{host}: {e}"))?;
        }
        Ok(())
    }

    /// Reject a hostname that did not resolve here and would be handed to
    /// a proxy to resolve. Its addresses cannot be checked, so only names
    /// that look public pass: cloud metadata names never do, and
    /// local-network names (`localhost`, single-label, `.internal`,
    /// `.local`, `.lan`, ...) only when private destinations are allowed.
    fn check_unresolved_name(&self, name: &str) -> Result<()> {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        // Exact names, plus search-domain forms such as AWS's regional
        // `instance-data.<region>.compute.internal`.
        let metadata_in_local_domain = name.split_once('.').is_some_and(|(first, rest)| {
            METADATA_LABELS.contains(&first) && has_local_suffix(rest)
        });
        if METADATA_NAMES.contains(&name.as_str()) || metadata_in_local_domain {
            bail!("{name} is a cloud metadata endpoint and is never fetched");
        }
        let local = !name.contains('.') || has_local_suffix(&name);
        if local && !self.allow_private {
            bail!(
                "{name} did not resolve here and looks like a local-network name; set allowPrivateNetworkFetch: true to let the proxy resolve it"
            );
        }
        Ok(())
    }
}

/// Loopback services the user let the CDP browser reach this session, when
/// `allowPrivateNetworkFetch` is off. `browser_navigate` asks once per
/// `host:port` and records the answer here; the browser's policy proxy reads
/// the same grants, so they also cover that service's redirects and
/// subresources, and nothing else on loopback.
///
/// A grant is for the name the user approved, not for whatever resolves to
/// the same address: the proxy resolves every connection afresh, so keyed on
/// `127.0.0.1:3000` any page could rebind its own name onto an approved dev
/// server and read it same-origin.
#[derive(Debug, Clone, Default)]
pub struct LoopbackGrants(std::sync::Arc<std::sync::Mutex<GrantState>>);

#[derive(Debug, Default)]
struct GrantState {
    /// Approved `(host, port)`, host as [`host_key`] spells it.
    hosts: std::collections::HashSet<(String, u16)>,
    /// What the approved names resolved to. An IP literal for one of these
    /// passes too: no DNS answer is involved, so nothing can rebind it.
    addrs: std::collections::HashSet<SocketAddr>,
    /// Loopback `host:port` the proxy refused for want of a grant, not yet
    /// reported to the model.
    blocked: std::collections::BTreeSet<String>,
}

/// Most refused loopback services held for one report.
const MAX_BLOCKED: usize = 16;

impl LoopbackGrants {
    fn state(&self) -> std::sync::MutexGuard<'_, GrantState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Allow `url`'s host and port, which resolved to `addrs`. Only the
    /// loopback ones among `addrs` count; with none, nothing is granted.
    pub fn grant(&self, url: &Url, addrs: &[SocketAddr]) {
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return;
        };
        let loopback: Vec<SocketAddr> = addrs
            .iter()
            .filter(|a| is_loopback(a.ip()))
            .map(|a| canonical(*a))
            .collect();
        if loopback.is_empty() {
            return;
        }
        let mut s = self.state();
        s.hosts.insert((host_key(host), port));
        s.addrs.extend(loopback);
        s.blocked.remove(&format!("{host}:{port}"));
    }

    /// A connection to `addr`, which `host` resolved to, is one the user
    /// allowed: `addr` is loopback, and `host` is an approved name at that
    /// port or an IP literal of an approved service.
    pub fn covers(&self, host: &str, addr: SocketAddr) -> bool {
        if !is_loopback(addr.ip()) {
            return false;
        }
        let s = self.state();
        s.hosts.contains(&(host_key(host), addr.port()))
            || (ip_literal(host).is_some() && s.addrs.contains(&canonical(addr)))
    }

    /// The browser's proxy refused `url` only because its loopback service
    /// was never approved; keep it for [`LoopbackGrants::take_blocked`].
    fn record_blocked(&self, url: &Url) {
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return;
        };
        let mut s = self.state();
        if s.blocked.len() < MAX_BLOCKED {
            s.blocked.insert(format!("{host}:{port}"));
        }
    }

    /// The loopback `host:port` the browser was refused for want of a grant
    /// since the last call (a page's script or subresources asking for a
    /// service the user never approved), so the model can be told.
    pub fn take_blocked(&self) -> Vec<String> {
        std::mem::take(&mut self.state().blocked)
            .into_iter()
            .collect()
    }
}

/// A host as grants compare it: lower-case, without a trailing dot.
fn host_key(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// `host` (as `Url::host_str` spells it, IPv6 bracketed) is an IP literal.
fn ip_literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// `addr` with an IPv4-mapped IPv6 address written as the IPv4 one.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// Loopback, `::ffff:127.0.0.1` included.
pub fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// Look `host` up with the system resolver.
async fn lookup_system(host: String, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host((host.as_str(), port))
        .await?
        .collect())
}

/// The addresses `url` points at. The outer error is a URL the policy
/// refuses outright (scheme, no host or port); the inner one is a name the
/// lookup could not resolve, which a proxy may still be able to.
async fn addresses<L, F>(url: &Url, lookup: &L) -> Result<Result<Vec<SocketAddr>>>
where
    L: Fn(String, u16) -> F,
    F: Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    if !matches!(url.scheme(), "http" | "https") {
        bail!(
            "only http/https URLs can be fetched (got {}:)",
            url.scheme()
        );
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("URL has no port: {url}"))?;
    Ok(match url.host() {
        Some(Host::Ipv4(ip)) => Ok(vec![SocketAddr::new(ip.into(), port)]),
        Some(Host::Ipv6(ip)) => Ok(vec![SocketAddr::new(ip.into(), port)]),
        Some(Host::Domain(name)) => match lookup(name.to_string(), port).await {
            Ok(a) if a.is_empty() => Err(anyhow!("{name} did not resolve to any address")),
            Ok(a) => Ok(a),
            Err(e) => Err(anyhow!("could not resolve {name}: {e}")),
        },
        None => bail!("URL has no host: {url}"),
    })
}

/// Cloud metadata hostnames: refused even when they cannot be resolved
/// here and a proxy would resolve them.
const METADATA_NAMES: [&str; 5] = [
    "metadata",
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
    "instance-data.ec2.internal",
];

/// First labels that name a cloud metadata service when the rest of the
/// name is a local-network domain.
const METADATA_LABELS: [&str; 2] = ["metadata", "instance-data"];

/// `name` is, or ends in, one of [`LOCAL_SUFFIXES`].
fn has_local_suffix(name: &str) -> bool {
    LOCAL_SUFFIXES.iter().any(|s| {
        name.strip_suffix(s)
            .is_some_and(|r| r.is_empty() || r.ends_with('.'))
    })
}

/// Suffixes of names that only exist on a local network (special-use and
/// de-facto private TLDs).
const LOCAL_SUFFIXES: [&str; 10] = [
    "localhost",
    "local",
    "localdomain",
    "internal",
    "intranet",
    "lan",
    "home",
    "home.arpa",
    "corp",
    "private",
];

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
    /// Set when the fetch stopped at a redirect to another host instead of
    /// following it (`follow_cross_host: false`); `body` is then empty.
    pub redirect_to: Option<Url>,
}

/// GET `url` under `policy`: every hop is resolved and checked before any
/// connection is made, redirects are followed manually (max
/// [`MAX_REDIRECTS`]), and the body is refused — before or during the read —
/// once it exceeds `max_bytes`.
///
/// With `follow_cross_host` false, a redirect to a different host is not
/// followed: the result carries it in `redirect_to`, so the caller can send
/// the new host back through the permission gate (WebFetch domain rules).
pub async fn fetch(
    url: &str,
    policy: &NetPolicy,
    max_bytes: usize,
    timeout: std::time::Duration,
    follow_cross_host: bool,
) -> Result<Fetched> {
    fetch_with_env(
        url,
        policy,
        max_bytes,
        timeout,
        follow_cross_host,
        |k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()),
        &lookup_system,
    )
    .await
}

/// The proxy the environment names for `url`, per scheme as reqwest's own
/// system-proxy lookup does, or `None` when there is none or `NO_PROXY`
/// covers the host.
fn env_chain(env: impl Fn(&str) -> Option<String>, url: &Url) -> Option<Url> {
    let host = url.host_str()?;
    let vars: &[&str] = if url.scheme() == "https" {
        &["HTTPS_PROXY", "ALL_PROXY"]
    } else {
        &["HTTP_PROXY", "ALL_PROXY"]
    };
    EnvProxy::from_vars(env, vars)
        .filter(|p| !no_proxy_covers(&p.no_proxy, host))
        .map(|p| p.url)
}

/// `fetch` with the proxy variables read through `env` and names resolved
/// by `lookup`.
async fn fetch_with_env<L, F>(
    url: &str,
    policy: &NetPolicy,
    max_bytes: usize,
    timeout: std::time::Duration,
    follow_cross_host: bool,
    env: impl Fn(&str) -> Option<String>,
    lookup: &L,
) -> Result<Fetched>
where
    L: Fn(String, u16) -> F,
    F: Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    let mut current = Url::parse(url).map_err(|e| anyhow!("invalid URL {url:?}: {e}"))?;
    let host_key = |u: &Url| {
        u.host_str()
            .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
    };
    let origin_host = host_key(&current);
    for _ in 0..=MAX_REDIRECTS {
        let host = current
            .host_str()
            .ok_or_else(|| anyhow!("URL has no host: {current}"))?
            .to_string();
        let chain = env_chain(&env, &current);
        // `no_proxy` drops reqwest's implicit HTTP(S)_PROXY, which would
        // send even a pinned, policy-checked host to a proxy that resolves
        // it again, and could not resolve a name only that proxy knows.
        // Redirects are disabled so each hop comes back through `route`.
        let builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(USER_AGENT);
        let client = match route(policy, None, &current, chain, lookup).await? {
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
                let target = current
                    .join(loc)
                    .map_err(|e| anyhow!("bad redirect target {loc:?}: {e}"))?;
                if !follow_cross_host && host_key(&target) != origin_host {
                    return Ok(Fetched {
                        final_url: current,
                        status,
                        content_type: String::new(),
                        body: Vec::new(),
                        redirect_to: Some(target),
                    });
                }
                current = target;
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
            redirect_to: None,
        });
    }
    bail!("too many redirects (more than {MAX_REDIRECTS} hops)")
}

/// Decode a fetched body the way a browser would: BOM, then the
/// Content-Type `charset=`, then an HTML `<meta charset>` in the first KiB,
/// else UTF-8. Lossy UTF-8 turned every legacy-encoded page (cp1251,
/// Shift_JIS, latin-1, ...) into replacement characters.
pub fn decode_body(content_type: &str, body: &[u8]) -> String {
    let declared = charset_param(content_type)
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .or_else(|| {
            if !(content_type.is_empty() || content_type.contains("html")) {
                return None;
            }
            let enc = meta_charset(&body[..body.len().min(1024)])
                .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))?;
            // A meta tag is read as ASCII, so it can't really mean UTF-16
            // (HTML spec: treat it as UTF-8).
            Some(
                if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
                    encoding_rs::UTF_8
                } else {
                    enc
                },
            )
        });
    // `decode` lets a BOM override the declared encoding.
    let (text, _, _) = declared.unwrap_or(encoding_rs::UTF_8).decode(body);
    text.into_owned()
}

fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches(['"', '\'']).to_string())
    })
}

/// `<meta charset="x">` or `<meta http-equiv=... content="...; charset=x">`.
fn meta_charset(head: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut rest = head.as_str();
    while let Some(i) = rest.find("<meta") {
        let tag = &rest[i..];
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        if let Some(j) = tag.find("charset=") {
            let label: String = tag[j + "charset=".len()..]
                .trim_start_matches(['"', '\''])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || "-_:.".contains(*c))
                .collect();
            if !label.is_empty() {
                return Some(label);
            }
        }
        rest = &rest[i + "<meta".len()..];
    }
    None
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
    spawn_policy_proxy_with(policy, LoopbackGrants::default(), Upstream::from_env()).await
}

/// [`spawn_policy_proxy`] that also lets through the loopback services in
/// `grants`, as they are granted (the CDP browser's proxy).
pub async fn spawn_policy_proxy_with_grants(
    policy: NetPolicy,
    grants: LoopbackGrants,
) -> Result<PolicyProxy> {
    spawn_policy_proxy_with(policy, grants, Upstream::from_env()).await
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
/// already applied) the environment names for it. The name is resolved and
/// checked here first, proxy or not. Public destinations then go through
/// the proxy; loopback/LAN targets (allowed by the policy) connect
/// directly. A name that does not resolve here may still resolve at the
/// proxy (split-horizon DNS, or a network whose only way out is that
/// proxy): it goes there only if it looks public (`check_unresolved_name`),
/// and the address pin cannot apply, since the proxy does its own DNS.
async fn route<P, L, F>(
    policy: &NetPolicy,
    grants: Option<&LoopbackGrants>,
    url: &Url,
    chain: Option<P>,
    lookup: &L,
) -> Result<Route<P>>
where
    L: Fn(String, u16) -> F,
    F: Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    match addresses(url, lookup).await? {
        Ok(addrs) => {
            if let Err(e) = policy.check_addrs(url, &addrs, grants) {
                // Refused only for want of a grant: a page's request to a
                // local service nobody approved. Recorded so the model can
                // be told, since the page itself just breaks.
                if let Some(g) = grants
                    && policy
                        .loopback_need(url, &addrs, g)
                        .is_ok_and(|n| !n.is_empty())
                {
                    g.record_blocked(url);
                }
                return Err(e);
            }
            Ok(match chain {
                Some(up)
                    if addrs
                        .iter()
                        .all(|a| NetPolicy::STRICT.check_ip(a.ip()).is_ok()) =>
                {
                    Route::Upstream(up)
                }
                _ => Route::Direct(addrs),
            })
        }
        Err(e) => match (chain, url.host()) {
            (Some(up), Some(Host::Domain(name))) => {
                policy.check_unresolved_name(name)?;
                Ok(Route::Upstream(up))
            }
            _ => Err(e),
        },
    }
}

/// A name lookup the proxy can hold: the system resolver, or a test's.
type Lookup =
    fn(
        String,
        u16,
    ) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>;

fn lookup_system_boxed(
    host: String,
    port: u16,
) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>> {
    Box::pin(lookup_system(host, port))
}

async fn spawn_policy_proxy_with(
    policy: NetPolicy,
    grants: LoopbackGrants,
    upstream: Option<Upstream>,
) -> Result<PolicyProxy> {
    spawn_policy_proxy_dns(policy, grants, upstream, lookup_system_boxed).await
}

/// [`spawn_policy_proxy_with`] resolving names with `lookup`.
async fn spawn_policy_proxy_dns(
    policy: NetPolicy,
    grants: LoopbackGrants,
    upstream: Option<Upstream>,
    lookup: Lookup,
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
                        conns.spawn(proxy_one(
                            sock,
                            policy,
                            grants.clone(),
                            upstream.clone(),
                            lookup,
                        ));
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
    grants: LoopbackGrants,
    upstream: Option<Upstream>,
    lookup: Lookup,
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
    let route = match route(&policy, Some(&grants), &url, chain, &lookup).await {
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

    // ── charset decoding ─────────────────────────────────────────────────

    #[test]
    fn body_is_decoded_by_its_declared_charset() {
        // "Привет" in windows-1251; lossy UTF-8 made it all U+FFFD.
        let cp1251: &[u8] = &[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        assert_eq!(
            decode_body("text/html; charset=windows-1251", cp1251),
            "Привет"
        );
        assert_eq!(
            decode_body("text/plain; Charset=\"CP1251\"", cp1251),
            "Привет"
        );

        let mut page = b"<html><head><META charset='windows-1251'></head><body>".to_vec();
        page.extend_from_slice(cp1251);
        assert!(decode_body("text/html", &page).ends_with("<body>Привет"));
        let mut page =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=koi8-r\">".to_vec();
        page.extend_from_slice(&[0xF0, 0xD2, 0xC9, 0xD7, 0xC5, 0xD4]);
        assert!(decode_body("", &page).ends_with("Привет"));

        // The header wins over meta; a BOM wins over both.
        assert!(
            decode_body(
                "text/html; charset=utf-8",
                "<meta charset=latin1>é".as_bytes()
            )
            .ends_with('é')
        );
        assert_eq!(
            decode_body("text/html; charset=latin1", b"\xEF\xBB\xBFh\xC3\xA9"),
            "hé"
        );

        // No or unknown label: UTF-8.
        assert_eq!(decode_body("text/html", "héllo".as_bytes()), "héllo");
        assert_eq!(
            decode_body("text/html; charset=bogus", "héllo".as_bytes()),
            "héllo"
        );
        // A meta tag can't switch to UTF-16.
        assert_eq!(
            decode_body("text/html", b"<meta charset=utf-16>hi"),
            "<meta charset=utf-16>hi"
        );
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
        let err = fetch(&base, &NetPolicy::STRICT, 1 << 20, SECS, true)
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
        let got = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS, true)
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
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "only the first hop may connect"
        );
    }

    /// WebFetch domain rules see only the first host, so a redirect to
    /// another host must come back to the caller instead of being read.
    #[tokio::test]
    async fn cross_host_redirect_stops_unless_followed() {
        let (base, hits) = scripted_server(vec![redirect("/same"), ok("first")]).await;
        let (other_base, other_hits) = scripted_server(vec![ok("other")]).await;
        let other = other_base.replace("127.0.0.1", "localhost");

        // Same host: followed even when cross-host hops are not.
        let got = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS, false)
            .await
            .unwrap();
        assert_eq!(got.body, b"first");
        assert!(got.redirect_to.is_none());

        let (base, hits2) =
            scripted_server(vec![redirect(&format!("{other}/x")), ok("unread")]).await;
        let got = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS, false)
            .await
            .unwrap();
        assert_eq!(got.status, 302);
        assert!(got.body.is_empty());
        assert_eq!(
            got.redirect_to.as_ref().map(Url::as_str),
            Some(format!("{other}/x").as_str())
        );
        assert_eq!(hits2.load(Ordering::SeqCst), 1, "the target is not fetched");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(other_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn redirect_loop_stops_after_max_hops() {
        let (base, hits) = scripted_server(vec![redirect("/again")]).await;
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1 << 20, SECS, true)
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
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1000, SECS, true)
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
        let err = fetch(&base, &NetPolicy::LOCAL_OK, 1000, SECS, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn body_within_cap_is_returned_with_content_type() {
        let (base, _) = scripted_server(vec![ok("hello")]).await;
        let got = fetch(&base, &NetPolicy::LOCAL_OK, 1000, SECS, true)
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
        let strict = spawn_policy_proxy_with(NetPolicy::STRICT, LoopbackGrants::default(), None)
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
        let local = spawn_policy_proxy_with(NetPolicy::LOCAL_OK, LoopbackGrants::default(), None)
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
        let proxy = spawn_policy_proxy_with(NetPolicy::LOCAL_OK, LoopbackGrants::default(), None)
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
        let proxy = spawn_policy_proxy_with(NetPolicy::STRICT, LoopbackGrants::default(), Some(up))
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
            "http://public.example/y",
        ] {
            let got = fetch_with_env(url, &NetPolicy::STRICT, 1024, t, true, &env, &fake_dns)
                .await
                .unwrap();
            assert_eq!(got.body, b"via-proxy", "{url}");
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "{seen:?}");
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
        // Resolved here to a public address, then named to the proxy (which
        // resolves it again): never rewritten to the checked address.
        assert!(
            seen[2].starts_with("GET http://public.example/y "),
            "{}",
            seen[2]
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

        let err = fetch_with_env(&base, &NetPolicy::STRICT, 1024, t, true, &env, &fake_dns)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private"), "{err}");
        let err = fetch_with_env(
            "http://169.254.169.254/",
            &NetPolicy::LOCAL_OK,
            1024,
            t,
            true,
            &env,
            &fake_dns,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("metadata"), "{err}");

        let got = fetch_with_env(&base, &NetPolicy::LOCAL_OK, 1024, t, true, &env, &fake_dns)
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
        let strict = spawn_policy_proxy_with(
            NetPolicy::STRICT,
            LoopbackGrants::default(),
            Some(up.clone()),
        )
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
        let local =
            spawn_policy_proxy_with(NetPolicy::LOCAL_OK, LoopbackGrants::default(), Some(up))
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

    /// Stand-in DNS: fixed answers, everything else NXDOMAIN.
    async fn fake_dns(host: String, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        let ip = match host.as_str() {
            "imds.example" => "169.254.169.254",
            "lan.example" => "10.1.2.3",
            "public.example" => "93.184.215.14",
            "dev.example" => "127.0.0.1",
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such host",
                ));
            }
        };
        Ok(vec![SocketAddr::new(ip.parse().unwrap(), port)])
    }

    /// With HTTP(S)_PROXY set, reqwest used to hand the hostname straight
    /// to the proxy, so a name pointing at the metadata service or the LAN
    /// was fetched through it. It is now resolved and checked here first.
    #[tokio::test]
    async fn proxied_fetch_refuses_names_that_resolve_to_metadata_or_lan() {
        let (up, seen) = fake_upstream("HTTP/1.1 200 OK\r\n\r\nfrom-proxy").await;
        let proxy = format!("http://127.0.0.1:{}", up.port);
        let env = vars(&[
            ("HTTP_PROXY", proxy.as_str()),
            ("HTTPS_PROXY", proxy.as_str()),
        ]);
        let t = std::time::Duration::from_secs(10);

        for policy in [NetPolicy::STRICT, NetPolicy::LOCAL_OK] {
            for url in [
                "http://imds.example/latest/meta-data/",
                "https://imds.example/latest/meta-data/",
            ] {
                let err = fetch_with_env(url, &policy, 1024, t, true, &env, &fake_dns)
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("metadata"), "{url}: {err}");
            }
        }
        let err = fetch_with_env(
            "http://lan.example/",
            &NetPolicy::STRICT,
            1024,
            t,
            true,
            &env,
            &fake_dns,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("private"), "{err}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing may reach the proxy"
        );

        // Allowed, a LAN name connects directly to the checked address and
        // never through the proxy.
        let url = Url::parse("http://lan.example:8080/").unwrap();
        match route(&NetPolicy::LOCAL_OK, None, &url, Some(proxy), &fake_dns)
            .await
            .unwrap()
        {
            Route::Direct(addrs) => assert_eq!(addrs, vec!["10.1.2.3:8080".parse().unwrap()]),
            Route::Upstream(p) => panic!("LAN host sent to the proxy {p}"),
        }
    }

    /// A name that does not resolve here can only be checked by its shape:
    /// public-looking names go to the proxy (trusted egress), metadata
    /// names never do, local-network names only with private allowed.
    #[tokio::test]
    async fn unresolvable_names_reach_the_proxy_only_when_they_look_public() {
        let up = || Some("http://proxy:3128");
        let route_of = async |policy: NetPolicy, u: &str| {
            route(&policy, None, &Url::parse(u).unwrap(), up(), &fake_dns).await
        };
        for policy in [NetPolicy::STRICT, NetPolicy::LOCAL_OK] {
            for u in [
                "http://metadata.google.internal/computeMetadata/v1/",
                "https://METADATA.google.internal./",
                "http://metadata/",
                "http://instance-data/latest/",
                "http://metadata.goog/",
                "http://instance-data.eu-west-1.compute.internal/latest/meta-data/",
                "http://metadata.us-central1-a.c.project.internal/",
            ] {
                let err = route_of(policy, u).await.err().expect(u);
                assert!(err.to_string().contains("metadata"), "{u}: {err}");
            }
            // Only a local domain makes the label a metadata name.
            for u in [
                "https://proxy-only.example.com/",
                "https://metadata.example.com/",
            ] {
                assert!(
                    matches!(route_of(policy, u).await, Ok(Route::Upstream(_))),
                    "{u}"
                );
            }
        }
        for u in [
            "http://intranet/",
            "http://localhost./",
            "http://app.localhost/",
            "http://printer.local/",
            "http://git.corp/",
            "http://db.internal:5432/",
            "http://nas.home.arpa/",
        ] {
            let err = route_of(NetPolicy::STRICT, u).await.err().expect(u);
            assert!(
                err.to_string().contains("allowPrivateNetworkFetch"),
                "{u}: {err}"
            );
            assert!(
                matches!(
                    route_of(NetPolicy::LOCAL_OK, u).await,
                    Ok(Route::Upstream(_))
                ),
                "{u}"
            );
        }
        // Without a proxy nothing changes: an unresolvable name fails.
        let url = Url::parse("https://proxy-only.example.com/").unwrap();
        let err = route(&NetPolicy::STRICT, None, &url, None::<()>, &fake_dns)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("could not resolve"), "{err}");
    }

    /// NO_PROXY hosts skip the proxy and get the normal pinned connection:
    /// the name only resolves through the injected lookup, so reaching the
    /// server proves the connection used the checked address.
    #[tokio::test]
    async fn no_proxy_hosts_connect_directly_to_the_checked_address() {
        let (up, seen) = fake_upstream("HTTP/1.1 200 OK\r\n\r\nfrom-proxy").await;
        let proxy = format!("http://127.0.0.1:{}", up.port);
        let env = vars(&[("HTTP_PROXY", proxy.as_str()), ("NO_PROXY", "dev.example")]);
        let t = std::time::Duration::from_secs(10);
        let (base, hits) = scripted_server(vec![ok("local")]).await;
        let url = base.replace("127.0.0.1", "dev.example");

        let err = fetch_with_env(&url, &NetPolicy::STRICT, 1024, t, true, &env, &fake_dns)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        let got = fetch_with_env(&url, &NetPolicy::LOCAL_OK, 1024, t, true, &env, &fake_dns)
            .await
            .unwrap();
        assert_eq!(got.body, b"local");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing may reach the proxy"
        );
    }

    /// A NO_PROXY host that resolves to a public address skips the proxy
    /// and is pinned to the checked address; other hosts keep the proxy.
    #[tokio::test]
    async fn no_proxy_public_hosts_get_the_pinned_direct_route() {
        let env = vars(&[
            ("HTTP_PROXY", "http://proxy:3128"),
            ("NO_PROXY", "public.example"),
        ]);
        let bypassed = Url::parse("http://public.example/").unwrap();
        assert_eq!(env_chain(&env, &bypassed), None);
        assert_eq!(
            env_chain(&env, &Url::parse("http://other.example/").unwrap()),
            Some(Url::parse("http://proxy:3128").unwrap())
        );
        // Without NO_PROXY the same public host goes to the proxy.
        let proxied = vars(&[("HTTP_PROXY", "http://proxy:3128")]);
        assert!(env_chain(&proxied, &bypassed).is_some());

        match route(
            &NetPolicy::STRICT,
            None,
            &bypassed,
            env_chain(&env, &bypassed),
            &fake_dns,
        )
        .await
        .unwrap()
        {
            Route::Direct(addrs) => {
                assert_eq!(addrs, vec!["93.184.215.14:80".parse().unwrap()])
            }
            Route::Upstream(p) => panic!("NO_PROXY host sent to the proxy {p}"),
        }
    }

    /// A redirect served through the proxy is checked like the first hop.
    #[tokio::test]
    async fn proxied_redirects_to_metadata_are_refused() {
        let t = std::time::Duration::from_secs(10);
        for (answer, needle) in [
            (
                "HTTP/1.1 302 Found\r\nlocation: http://imds.example/latest/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                "169.254.169.254",
            ),
            (
                "HTTP/1.1 302 Found\r\nlocation: http://metadata.google.internal/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                "metadata.google.internal",
            ),
        ] {
            let (up, seen) = fake_upstream(answer).await;
            let proxy = format!("http://127.0.0.1:{}", up.port);
            let env = vars(&[("HTTP_PROXY", proxy.as_str())]);
            let err = fetch_with_env(
                "http://public.example/",
                &NetPolicy::LOCAL_OK,
                1024,
                t,
                true,
                &env,
                &fake_dns,
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "only the first hop goes out: {seen:?}");
            assert!(
                seen[0].starts_with("GET http://public.example/ "),
                "{}",
                seen[0]
            );
        }
    }

    // ── the CDP browser: loopback grants ─────────────────────────────────

    /// The CDP browser's proxy ran under LOCAL_OK, so any page could reach
    /// every service on loopback and the LAN. Under the strict policy only
    /// a granted `ip:port` passes, and the grant takes effect on a proxy
    /// that is already running.
    #[tokio::test]
    async fn the_browser_proxy_admits_only_granted_loopback_services() {
        let (base, hits) = scripted_server(vec![ok("dev server")]).await;
        let authority = base.trim_start_matches("http://");
        let (other, other_hits) = scripted_server(vec![ok("redis")]).await;
        let grants = LoopbackGrants::default();
        let proxy = spawn_policy_proxy_with(NetPolicy::STRICT, grants.clone(), None)
            .await
            .unwrap();
        let get = |b: &str| format!("GET {b}/ HTTP/1.1\r\nHost: x\r\n\r\n");

        let got = via_proxy(&proxy, &get(&base)).await;
        assert!(got.starts_with("HTTP/1.1 403"), "{got}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        grants.grant(&Url::parse(&base).unwrap(), &[authority.parse().unwrap()]);
        let got = via_proxy(&proxy, &get(&base)).await;
        assert!(got.ends_with("dev server"), "{got}");
        let got = via_proxy(
            &proxy,
            &format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"),
        )
        .await;
        assert!(
            got.starts_with("HTTP/1.1 200 Connection Established"),
            "{got}"
        );

        let got = via_proxy(&proxy, &get(&other)).await;
        assert!(got.starts_with("HTTP/1.1 403"), "{got}");
        assert_eq!(other_hits.load(Ordering::SeqCst), 0);
    }

    /// A granted dev server (or any page) that redirects to the metadata
    /// service: Chrome's request for the next hop is refused.
    #[tokio::test]
    async fn a_granted_services_redirect_to_metadata_is_refused() {
        let (base, _) =
            scripted_server(vec![redirect("http://169.254.169.254/latest/meta-data/")]).await;
        let grants = LoopbackGrants::default();
        grants.grant(
            &Url::parse(&base).unwrap(),
            &[base.trim_start_matches("http://").parse().unwrap()],
        );
        for policy in [NetPolicy::STRICT, NetPolicy::LOCAL_OK] {
            let proxy = spawn_policy_proxy_with(policy, grants.clone(), None)
                .await
                .unwrap();
            let got = via_proxy(&proxy, &format!("GET {base}/ HTTP/1.1\r\nHost: x\r\n\r\n")).await;
            assert!(got.starts_with("HTTP/1.1 302"), "{got}");
            let hop = "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n";
            let got = via_proxy(&proxy, hop).await;
            assert!(got.starts_with("HTTP/1.1 403"), "{got}");
            assert!(got.contains("169.254.169.254"), "{got}");
        }
    }

    /// Grants lift only the loopback tier: never link-local, never the LAN.
    #[test]
    fn grants_cover_only_the_granted_loopback_address() {
        let grants = LoopbackGrants::default();
        grants.grant(
            &Url::parse("http://LocalHost:3000/").unwrap(),
            &[
                "127.0.0.1:3000".parse().unwrap(),
                "169.254.169.254:3000".parse().unwrap(),
                "10.0.0.5:3000".parse().unwrap(),
            ],
        );
        let p = NetPolicy::STRICT;
        let ok = |host: &str, addr: &str| p.check_addr(host, addr.parse().unwrap(), Some(&grants));
        assert!(ok("localhost", "127.0.0.1:3000").is_ok());
        assert!(ok("localhost.", "127.0.0.1:3000").is_ok());
        // IP literals of the approved service: nothing to rebind.
        assert!(ok("127.0.0.1", "127.0.0.1:3000").is_ok());
        assert!(ok("[::ffff:127.0.0.1]", "[::ffff:127.0.0.1]:3000").is_ok());
        assert!(ok("localhost", "127.0.0.1:3001").is_err());
        assert!(ok("127.0.0.1", "127.0.0.1:3001").is_err());
        assert!(
            p.check_addr("localhost", "127.0.0.1:3000".parse().unwrap(), None)
                .is_err()
        );
        assert!(ok("localhost", "169.254.169.254:3000").is_err());
        assert!(ok("localhost", "10.0.0.5:3000").is_err());
        // A grant needs a loopback answer.
        let lan = LoopbackGrants::default();
        lan.grant(
            &Url::parse("http://lan.example:80/").unwrap(),
            &["10.0.0.5:80".parse().unwrap()],
        );
        assert!(!lan.covers("lan.example", "127.0.0.1:80".parse().unwrap()));
    }

    /// Stand-in DNS where a second name resolves to the same loopback
    /// address as `localhost`: an attacker's name rebound onto it.
    fn rebinding_dns(
        host: String,
        port: u16,
    ) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>> {
        Box::pin(async move {
            match host.as_str() {
                "localhost" | "attacker.example" => {
                    Ok(vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)])
                }
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such host",
                )),
            }
        })
    }

    /// Grants were keyed on the resolved `ip:port`, and the proxy resolves
    /// every connection afresh, so once `localhost:3000` was approved any
    /// page could serve itself from `attacker.example:3000`, rebind that
    /// name to 127.0.0.1 and read the dev server same-origin. Only the
    /// approved name (and the service's IP literal) pass now; the refused
    /// name is recorded for the model.
    #[tokio::test]
    async fn another_name_resolving_to_a_granted_service_is_refused() {
        let (base, hits) = scripted_server(vec![ok("dev server")]).await;
        let port = Url::parse(&base).unwrap().port().unwrap();
        let grants = LoopbackGrants::default();
        grants.grant(
            &Url::parse(&format!("http://localhost:{port}/")).unwrap(),
            &[SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)],
        );
        let proxy = spawn_policy_proxy_dns(NetPolicy::STRICT, grants.clone(), None, rebinding_dns)
            .await
            .unwrap();
        let get = |host: &str| format!("GET http://{host}:{port}/ HTTP/1.1\r\nHost: x\r\n\r\n");

        let got = via_proxy(&proxy, &get("attacker.example")).await;
        assert!(got.starts_with("HTTP/1.1 403"), "{got}");
        let got = via_proxy(
            &proxy,
            &format!("CONNECT attacker.example:{port} HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(got.starts_with("HTTP/1.1 403"), "{got}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(
            grants.take_blocked(),
            vec![format!("attacker.example:{port}")]
        );
        assert!(grants.take_blocked().is_empty(), "reported once");

        for host in ["localhost", "127.0.0.1"] {
            let got = via_proxy(&proxy, &get(host)).await;
            assert!(got.ends_with("dev server"), "{host}: {got}");
        }
        // Refusals for other reasons are not "ask the user" material.
        let got = via_proxy(
            &proxy,
            "GET http://169.254.169.254/ HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(got.starts_with("HTTP/1.1 403"), "{got}");
        assert!(grants.take_blocked().is_empty());
    }

    /// The preflight refused every name it could not resolve, though the
    /// browser's proxy hands public-looking ones to the upstream proxy.
    #[tokio::test]
    async fn browser_preflight_matches_the_proxy_on_unresolved_names() {
        async fn check(url: &str, with_upstream: bool) -> Result<Vec<SocketAddr>> {
            let upstream = || {
                with_upstream
                    .then(|| Upstream::from_vars(vars(&[("HTTPS_PROXY", "http://proxy:3128")])))
                    .flatten()
            };
            NetPolicy::STRICT
                .check_browser_url_with(
                    &Url::parse(url).unwrap(),
                    &LoopbackGrants::default(),
                    upstream,
                    &fake_dns,
                )
                .await
        }
        let name = "https://only-the-proxy-knows.example/";
        assert!(check(name, true).await.unwrap().is_empty());
        assert!(check(name, false).await.is_err());
        assert!(
            check("http://metadata.google.internal/", true)
                .await
                .is_err()
        );
        assert!(check("http://intranet/", true).await.is_err());
        // Resolved names: metadata and the LAN refused, loopback needs a grant.
        assert!(check("http://imds.example/", true).await.is_err());
        assert!(check("http://lan.example/", true).await.is_err());
        assert_eq!(
            check("http://dev.example:5173/", true).await.unwrap(),
            vec!["127.0.0.1:5173".parse::<SocketAddr>().unwrap()]
        );
    }
}
