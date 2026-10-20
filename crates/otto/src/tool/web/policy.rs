//! Destination policy for the web tools.
//!
//! The tools run inside the Otto process, so the Seatbelt profile that
//! confines `bash` does not apply to them. This module is the boundary
//! instead: it decides which URLs may be requested and which resolved
//! addresses may be dialled.
//!
//! Two checks cooperate. [`parse_url`] validates the text the model supplied
//! (scheme, userinfo, literal IP hosts). [`PublicResolver`] filters every
//! address a host name resolves to at connect time, so a name that resolves
//! to a private address, or one that changes its answer between a check and
//! the connection, never reaches `connect`.
//!
//! Answers in 198.18.0.0/15 that the kernel routes from a local address in
//! that range are fake IPs from a local TUN interface's DNS, and are accepted.
//! On such machines the real address is resolved behind the TUN interface, so
//! the public-address check cannot see it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// The longest URL the tools accept.
pub(super) const MAX_URL_BYTES: usize = 2048;

/// Parses `text` into a request URL.
///
/// Only `http` and `https` are accepted, userinfo is refused (it is a
/// credential channel), and a literal IP host must be public.
pub(super) fn parse_url(text: &str) -> Result<Url, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("url is required".to_string());
    }
    if text.len() > MAX_URL_BYTES {
        return Err(format!("url is longer than {MAX_URL_BYTES} bytes"));
    }
    let url = Url::parse(text).map_err(|error| format!("invalid url: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "unsupported url scheme {:?}; only http and https are allowed",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("urls with embedded credentials are not allowed".to_string());
    }
    match url.host() {
        None => return Err("url has no host".to_string()),
        Some(Host::Ipv4(ip)) if !public_ip(IpAddr::V4(ip)) => {
            return Err(blocked_message(&ip.to_string()));
        }
        Some(Host::Ipv6(ip)) if !public_ip(IpAddr::V6(ip)) => {
            return Err(blocked_message(&ip.to_string()));
        }
        Some(_) => {}
    }
    Ok(url)
}

fn blocked_message(host: &str) -> String {
    format!("{host} is not a public address; local and private network addresses are blocked")
}

/// The lowercase `host` of `url`, used to check redirect destinations.
pub(super) fn host_key(url: &Url) -> Option<String> {
    url.host_str().map(|host| host.to_ascii_lowercase())
}

/// Whether `ip` is an address a web tool may connect to.
///
/// Rejects loopback, unspecified, private, link-local (which includes the
/// `169.254.169.254` metadata address), carrier-grade NAT, multicast,
/// broadcast, documentation and unique-local ranges, and the IPv4-mapped and
/// IPv4-compatible IPv6 forms of those.
pub(super) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // 100.64.0.0/10, carrier-grade NAT.
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24, IETF protocol assignments.
        || (a == 192 && b == 0 && c == 0)
        // 198.18.0.0/15, benchmarking.
        || benchmarking_v4(ip)
        // 240.0.0.0/4, reserved.
        || a >= 240
        // 0.0.0.0/8, "this network".
        || a == 0)
}

/// 198.18.0.0/15, the benchmarking range a TUN interface can use for fake IPs.
fn benchmarking_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 198 && (b == 18 || b == 19)
}

/// True when `answer` is a fake IP and the kernel routes it from the local
/// address `local`, which is in the same range: a local TUN interface holds
/// that address. The interface's own address stays blocked.
fn fake_ip_route(answer: Ipv4Addr, local: Ipv4Addr) -> bool {
    benchmarking_v4(answer) && benchmarking_v4(local) && answer != local
}

/// Detects a fake IP handed out by a local TUN interface. The route lookup
/// uses a UDP `connect`, which selects the source address and sends no packet.
/// Any I/O error counts as not a fake IP.
fn tun_fake_ip(ip: IpAddr) -> bool {
    let IpAddr::V4(answer) = ip else {
        return false;
    };
    if !benchmarking_v4(answer) {
        return false;
    }
    let local = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|socket| {
        socket.connect((answer, 443))?;
        socket.local_addr()
    });
    match local {
        Ok(SocketAddr::V4(local)) => fake_ip_route(answer, *local.ip()),
        _ => false,
    }
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_v4(mapped);
    }
    let segments = ip.segments();
    // IPv4-compatible (::a.b.c.d), deprecated but still parsed by some stacks.
    if segments[..6] == [0; 6] {
        let [a, b] = [segments[6], segments[7]];
        return public_v4(Ipv4Addr::new(
            (a >> 8) as u8,
            a as u8,
            (b >> 8) as u8,
            b as u8,
        ));
    }
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7, unique local.
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10, link-local.
        || (segments[0] & 0xffc0) == 0xfe80
        // fec0::/10, deprecated site-local.
        || (segments[0] & 0xffc0) == 0xfec0
        // 2001:db8::/32, documentation.
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // 64:ff9b::/96, NAT64: the embedded IPv4 decides.
        || (segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
            && !public_v4(Ipv4Addr::new(
                (segments[6] >> 8) as u8,
                segments[6] as u8,
                (segments[7] >> 8) as u8,
                segments[7] as u8,
            ))))
}

/// A resolution function: host name and port to socket addresses. A trait
/// object so tests can substitute a deterministic answer for DNS.
pub(super) type Lookup = Arc<
    dyn Fn(
            String,
            u16,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>,
        > + Send
        + Sync,
>;

fn system_lookup() -> Lookup {
    Arc::new(|host, port| {
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), port))
                .await
                .map(|addrs| addrs.collect())
        })
    })
}

/// A DNS resolver that only returns public addresses.
///
/// reqwest asks it for every connection, including each redirect target and
/// each retry, and dials only what it returns. A name whose answers are all
/// non-public fails with an error naming the block, not a connect timeout.
#[derive(Clone)]
pub(super) struct PublicResolver {
    lookup: Lookup,
    allow_private: bool,
    fake_ip: fn(IpAddr) -> bool,
}

impl PublicResolver {
    pub(super) fn system() -> Self {
        Self {
            lookup: system_lookup(),
            allow_private: false,
            fake_ip: tun_fake_ip,
        }
    }

    /// A resolver over `lookup` that still filters, for tests.
    #[cfg(test)]
    pub(super) fn with_lookup(lookup: Lookup) -> Self {
        Self {
            lookup,
            allow_private: false,
            fake_ip: |_| false,
        }
    }

    /// Replaces the fake-IP check, for tests.
    #[cfg(test)]
    fn with_fake_ip(self, fake_ip: fn(IpAddr) -> bool) -> Self {
        Self { fake_ip, ..self }
    }

    /// A resolver that sends every name to `127.0.0.1` and accepts it, so a
    /// test can reach a listener on a made-up host name. There is no
    /// production constructor for this.
    #[cfg(test)]
    pub(super) fn loopback_for_tests() -> Self {
        Self {
            lookup: Arc::new(|_host, port| {
                Box::pin(async move { Ok(vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)]) })
            }),
            allow_private: true,
            fake_ip: |_| false,
        }
    }

    pub(super) async fn resolve_public(
        &self,
        host: &str,
        port: u16,
    ) -> std::io::Result<Vec<SocketAddr>> {
        let addrs = (self.lookup)(host.to_string(), port).await?;
        if addrs.is_empty() {
            return Err(std::io::Error::other(format!("{host} did not resolve")));
        }
        let allowed: Vec<SocketAddr> = addrs
            .iter()
            .copied()
            .filter(|addr| self.allow_private || public_ip(addr.ip()) || (self.fake_ip)(addr.ip()))
            .collect();
        if allowed.is_empty() {
            return Err(std::io::Error::other(blocked_message(host)));
        }
        Ok(allowed)
    }
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.clone();
        Box::pin(async move {
            // The port is filled in by the connector; 0 is the placeholder
            // reqwest documents for resolvers.
            let addrs = resolver
                .resolve_public(name.as_str(), 0)
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test address parses")
    }

    #[test]
    fn blocks_every_non_public_address_class() {
        for blocked in [
            "127.0.0.1",
            "127.8.8.8",
            "0.0.0.0",
            "10.0.0.1",
            "172.16.5.5",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "198.18.0.1",
            "192.0.0.8",
            "192.0.2.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
        ] {
            assert!(!public_ip(ip(blocked)), "{blocked} must be blocked");
        }
    }

    #[test]
    fn fake_ip_route_requires_both_addresses_in_range_and_differing() {
        let parse_ipv4 = |text: &str| -> Ipv4Addr { text.parse().unwrap() };
        for (answer, local, expected) in [
            ("198.18.0.141", "198.18.0.1", true),
            ("198.19.255.1", "198.18.0.1", true),
            ("198.18.0.141", "192.168.1.5", false),
            ("198.18.0.1", "198.18.0.1", false),
            ("8.8.8.8", "198.18.0.1", false),
            ("10.0.0.1", "198.18.0.1", false),
        ] {
            assert_eq!(
                fake_ip_route(parse_ipv4(answer), parse_ipv4(local)),
                expected,
                "{answer} via {local}"
            );
        }
    }

    #[test]
    fn tun_fake_ip_is_false_outside_the_range() {
        for outside in ["8.8.8.8", "127.0.0.1", "10.0.0.1", "::1"] {
            assert!(!tun_fake_ip(ip(outside)), "{outside}");
        }
    }

    #[tokio::test]
    async fn resolver_accepts_fake_ip_only_when_the_check_says_so() {
        let lookup = || -> Lookup {
            Arc::new(|_host, port| {
                Box::pin(async move { Ok(vec![SocketAddr::new(ip("198.18.0.141"), port)]) })
            })
        };
        let accepted = PublicResolver::with_lookup(lookup())
            .with_fake_ip(|_| true)
            .resolve_public("api.example.com", 443)
            .await
            .expect("fake IP accepted");
        assert_eq!(accepted, vec![SocketAddr::new(ip("198.18.0.141"), 443)]);
        let refused = PublicResolver::with_lookup(lookup())
            .resolve_public("api.example.com", 443)
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("not a public address"),
            "{refused}"
        );
    }

    #[test]
    fn allows_ordinary_public_addresses() {
        for allowed in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(public_ip(ip(allowed)), "{allowed} must be allowed");
        }
    }

    #[test]
    fn url_validation_rejects_unsafe_forms() {
        for (text, fragment) in [
            ("", "required"),
            ("file:///etc/passwd", "scheme"),
            ("ftp://example.com/x", "scheme"),
            ("javascript:alert(1)", "scheme"),
            ("https://user:pw@example.com/", "credentials"),
            ("https://user@example.com/", "credentials"),
            ("http://127.0.0.1/", "not a public address"),
            ("http://[::1]/", "not a public address"),
            (
                "http://169.254.169.254/latest/meta-data",
                "not a public address",
            ),
            ("http://[::ffff:10.0.0.1]/", "not a public address"),
            ("http://2130706433/", "not a public address"),
            ("http://0x7f.1/", "not a public address"),
            ("not a url", "invalid url"),
        ] {
            let error = parse_url(text).expect_err(text);
            assert!(error.contains(fragment), "{text}: {error}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(parse_url(&long).unwrap_err().contains("longer"));
    }

    #[test]
    fn url_validation_accepts_public_urls_and_keys_by_lowercase_host() {
        let url = parse_url("  https://Example.COM:8443/a?b=1#frag ").expect("valid url");
        assert_eq!(host_key(&url).as_deref(), Some("example.com"));
        assert!(parse_url("http://8.8.8.8/").is_ok());
    }

    fn fixed(addrs: &'static [&'static str]) -> Lookup {
        Arc::new(move |_host, port| {
            Box::pin(async move {
                Ok(addrs
                    .iter()
                    .map(|text| SocketAddr::new(ip(text), port))
                    .collect())
            })
        })
    }

    #[tokio::test]
    async fn resolver_drops_private_answers_and_keeps_public_ones() {
        let resolver = PublicResolver::with_lookup(fixed(&["10.0.0.5", "93.184.216.34"]));
        let addrs = resolver.resolve_public("mixed.example", 443).await.unwrap();
        assert_eq!(addrs, vec![SocketAddr::new(ip("93.184.216.34"), 443)]);
    }

    #[tokio::test]
    async fn resolver_refuses_a_name_that_only_resolves_privately() {
        for private in [
            &["127.0.0.1"][..],
            &["169.254.169.254"][..],
            &["::1"][..],
            &["::ffff:192.168.0.1"][..],
        ] {
            let private: &'static [&'static str] = Box::leak(private.to_vec().into_boxed_slice());
            let resolver = PublicResolver::with_lookup(fixed(private));
            let error = resolver
                .resolve_public("internal.example", 80)
                .await
                .expect_err("private answers are refused");
            assert!(
                error.to_string().contains("not a public address"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn resolver_rechecks_every_lookup_so_rebinding_is_caught() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let lookup: Lookup = Arc::new(move |_host, port| {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let answer = if call == 0 {
                    "93.184.216.34"
                } else {
                    "127.0.0.1"
                };
                Ok(vec![SocketAddr::new(ip(answer), port)])
            })
        });
        let resolver = PublicResolver::with_lookup(lookup);
        assert!(resolver.resolve_public("rebind.example", 80).await.is_ok());
        assert!(resolver.resolve_public("rebind.example", 80).await.is_err());
    }

    #[tokio::test]
    async fn resolver_rejects_an_empty_answer() {
        let resolver = PublicResolver::with_lookup(fixed(&[]));
        assert!(
            resolver
                .resolve_public("nothing.example", 80)
                .await
                .is_err()
        );
    }
}
