//! Connect-time endpoint policy. Only these resolved addresses reach the connector.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

const BLOCKED: &str = "provider endpoint DNS contains forbidden metadata/link-local addresses";

type Lookup = Arc<dyn Fn(String) -> Resolving + Send + Sync>;

#[derive(Clone)]
struct EndpointResolver {
    lookup: Lookup,
}

impl Default for EndpointResolver {
    fn default() -> Self {
        Self {
            lookup: Arc::new(|host| {
                Box::pin(async move {
                    let addresses = tokio::net::lookup_host((host.as_str(), 0)).await?;
                    Ok(Box::new(addresses.collect::<Vec<_>>().into_iter()) as Addrs)
                })
            }),
        }
    }
}

impl Resolve for EndpointResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let lookup = Arc::clone(&self.lookup);
        Box::pin(async move {
            let addresses: Vec<SocketAddr> = lookup(name.as_str().to_owned()).await?.collect();
            // Reject the entire answer, not only the bad address. No connector
            // fallback or alternate DNS lookup can select a prohibited answer.
            if addresses.is_empty() || addresses.iter().any(|address| forbidden(address.ip())) {
                return Err(std::io::Error::other(BLOCKED).into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

fn forbidden_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 0
        || address.is_link_local()
        || address == Ipv4Addr::new(100, 100, 100, 200)
        || address == Ipv4Addr::new(168, 63, 129, 16)
}

fn forbidden(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => forbidden_v4(address),
        IpAddr::V6(address) => {
            let segments = address.segments();
            address.is_unspecified()
                || (segments[0] & 0xffc0 == 0xfe80)
                || segments == [0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254]
                || (!address.is_loopback() && address.to_ipv4().is_some_and(forbidden_v4))
                || (segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
                    && forbidden_v4(Ipv4Addr::new(
                        (segments[6] >> 8) as u8,
                        segments[6] as u8,
                        (segments[7] >> 8) as u8,
                        segments[7] as u8,
                    )))
        }
    }
}

/// Proxies are disabled: an environment proxy would otherwise resolve the
/// destination outside this resolver. Caller HTTP clients are not accepted.
pub(super) fn http_client_builder() -> reqwest::ClientBuilder {
    otto_platform::tls_provider::http_client_builder()
        .no_proxy()
        .dns_resolver(Arc::new(EndpointResolver::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn resolver(addresses: Vec<SocketAddr>, calls: Arc<AtomicUsize>) -> EndpointResolver {
        EndpointResolver {
            lookup: Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                let addresses = addresses.clone();
                Box::pin(async move { Ok(Box::new(addresses.into_iter()) as Addrs) })
            }),
        }
    }

    #[tokio::test]
    async fn fake_dns_denies_mixed_answers_before_any_request_content() {
        let target = otto_faults::http::spawn(|_| Vec::<u8>::new()).await;
        for denied in [
            "169.254.169.254",
            "100.100.100.200",
            "168.63.129.16",
            "fe80::1",
            "::ffff:169.254.169.254",
            "::169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "fd00:ec2::254",
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let dns = resolver(
                vec![
                    format!("127.0.0.1:{}", target.port()).parse().unwrap(),
                    SocketAddr::new(denied.parse().unwrap(), target.port()),
                ],
                Arc::clone(&calls),
            );
            let http = http_client_builder()
                .dns_resolver(Arc::new(dns))
                .build()
                .unwrap();
            let result = http
                .post(format!("http://fixture.example:{}/messages", target.port()))
                .header("Authorization", "Bearer fixture-secret")
                .body("private fixture content")
                .send()
                .await;
            assert!(result.is_err(), "{denied}");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(target.count(), 0);
        }
    }

    #[tokio::test]
    async fn fake_dns_allows_explicit_local_transport_without_fallback() {
        let target =
            otto_faults::http::spawn(|_| otto_faults::http::status_response(200, "ok")).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let dns = resolver(
            vec![format!("127.0.0.1:{}", target.port()).parse().unwrap()],
            Arc::clone(&calls),
        );
        let http = http_client_builder()
            .dns_resolver(Arc::new(dns))
            .build()
            .unwrap();
        assert!(
            http.get(format!("http://fixture.example:{}/", target.port()))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(target.count(), 1);
    }

    #[test]
    fn private_is_not_metadata() {
        for address in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "::1", "fd12::1"] {
            assert!(!forbidden(address.parse().unwrap()), "{address}");
        }
    }
    #[tokio::test]
    async fn empty_dns_answers_fail_closed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let dns = resolver(Vec::new(), Arc::clone(&calls));
        assert!(
            dns.resolve("fixture.example".parse().unwrap())
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
