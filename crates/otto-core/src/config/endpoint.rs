//! Provider endpoint syntax and user/managed origin authorization.
use url::{Host, Url};

const INVALID: &str =
    "invalid provider endpoint: expected HTTP(S), host, no userinfo/query/fragment";
const UNTRUSTED: &str = concat!(
    "provider endpoint is not trusted; explicitly configure trusted_origins in user/managed ",
    "provider configuration (HTTP also needs allow_http_origins); ",
    "see docs/manual/getting-started.md#endpoint-trust"
);

/// Request routing determined at the endpoint trust boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointRoute {
    Standard,
    OfficialClaude,
    OfficialDeepSeek,
}

/// A syntax-checked endpoint authorized by independent user/managed policy.
/// Paths are not an authority boundary; a grant covers the entire origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedEndpoint {
    base_url: String,
    provider_type: String,
    route: EndpointRoute,
}

impl TrustedEndpoint {
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    pub fn provider_type(&self) -> &str {
        &self.provider_type
    }

    pub fn route(&self) -> EndpointRoute {
        self.route
    }

    /// `trusted_origins` and HTTP acknowledgements must come only from trusted
    /// application/user/managed authority, never from the model or URL itself.
    pub fn authorize(
        base_url: &str,
        provider_type: &str,
        trusted_origins: &[String],
        allow_http_origins: &[String],
    ) -> Result<Self, &'static str> {
        Self::authorize_for_provider(
            base_url,
            provider_type,
            provider_type,
            trusted_origins,
            allow_http_origins,
        )
    }

    /// Authorizes an endpoint using its binding name and existing API type.
    pub fn authorize_for_provider(
        base_url: &str,
        provider_name: &str,
        provider_type: &str,
        trusted_origins: &[String],
        allow_http_origins: &[String],
    ) -> Result<Self, &'static str> {
        use super::providers::{DEEPSEEK_DEFAULT_BASE_URL, PROVIDER_TYPES};
        use super::{PROVIDER_DEEPSEEK, PROVIDER_OPENAI_COMPATIBLE};

        if !PROVIDER_TYPES.contains(&provider_type) {
            return Err("unsupported provider endpoint type");
        }
        let parsed = parse(base_url)?;
        let origin = parsed.origin().ascii_serialization();
        let deepseek_binding =
            provider_name == PROVIDER_DEEPSEEK && provider_type == PROVIDER_OPENAI_COMPATIBLE;
        let official = if deepseek_binding {
            origin == DEEPSEEK_DEFAULT_BASE_URL
        } else {
            match provider_type {
                "claude" => origin == "https://api.anthropic.com",
                "openai-compatible" => origin == "https://api.openai.com",
                "chatgpt" => origin == "https://chatgpt.com",
                _ => false,
            }
        };
        let granted = |grants: &[String]| {
            grants.iter().any(|grant| {
                parse(grant).is_ok_and(|url| {
                    url.path() == "/" && url.origin().ascii_serialization() == origin
                })
            })
        };
        if (!official && !granted(trusted_origins))
            || (parsed.scheme() == "http" && !granted(allow_http_origins))
        {
            return Err(UNTRUSTED);
        }
        Ok(Self {
            base_url: parsed.as_str().trim_end_matches('/').to_string(),
            provider_type: provider_type.to_string(),
            route: match (official, deepseek_binding, provider_type) {
                (true, true, _) => EndpointRoute::OfficialDeepSeek,
                (true, false, "claude") => EndpointRoute::OfficialClaude,
                _ => EndpointRoute::Standard,
            },
        })
    }
}

pub(super) fn is_deepseek_origin(base_url: &str) -> bool {
    parse(base_url).is_ok_and(|url| {
        url.origin().ascii_serialization() == super::providers::DEEPSEEK_DEFAULT_BASE_URL
    })
}

/// Checks a grant entry is an origin, not a URL path or secret-bearing URL.
pub fn validate_origin_grant(raw: &str) -> Result<(), &'static str> {
    let parsed = parse(raw)?;
    if parsed.path() != "/" {
        return Err("provider origin grants must not contain a path");
    }
    Ok(())
}

/// Validates syntax without authorizing credential or network use.
pub fn normalize_base_url(raw: &str) -> Result<String, &'static str> {
    Ok(parse(raw)?.as_str().trim_end_matches('/').to_string())
}

fn parse(raw: &str) -> Result<Url, &'static str> {
    // Inspect raw authority as WHATWG parsing erases empty userinfo and repairs
    // missing authority, whitespace and backslashes.
    let (_, tail) = raw.split_once("://").ok_or(INVALID)?;
    let authority = tail.split(['/', '?', '#']).next().ok_or(INVALID)?;
    if authority.is_empty()
        || authority.contains('@')
        || raw.contains('\\')
        || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(INVALID);
    }
    let parsed = Url::parse(raw).map_err(|_| INVALID)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(INVALID);
    }
    let forbidden = match parsed.host() {
        Some(Host::Ipv4(address)) => {
            let octets = address.octets();
            octets[0] == 0
                || (octets[0] == 169 && octets[1] == 254)
                || octets == [100, 100, 100, 200]
                || octets == [168, 63, 129, 16]
        }
        Some(Host::Ipv6(address)) => {
            let segments = address.segments();
            address.is_unspecified()
                || (segments[0] & 0xffc0 == 0xfe80)
                || segments == [0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254]
                || (segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
                    && forbidden_embedded_v4(segments[6], segments[7]))
                || (!address.is_loopback()
                    && address.to_ipv4().is_some_and(|v4| {
                        let octets = v4.octets();
                        octets[0] == 0
                            || (octets[0] == 169 && octets[1] == 254)
                            || octets == [100, 100, 100, 200]
                            || octets == [168, 63, 129, 16]
                    }))
        }
        Some(Host::Domain(host)) => matches!(
            host.trim_end_matches('.'),
            "metadata.google.internal" | "metadata.goog" | "instance-data"
        ),
        None => true,
    };
    if forbidden {
        return Err("provider metadata/link-local endpoints are forbidden");
    }
    Ok(parsed)
}

fn forbidden_embedded_v4(high: u16, low: u16) -> bool {
    let octets = [(high >> 8) as u8, high as u8, (low >> 8) as u8, low as u8];
    octets[0] == 0
        || (octets[0] == 169 && octets[1] == 254)
        || octets == [100, 100, 100, 200]
        || octets == [168, 63, 129, 16]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_parser_and_redacted_errors() {
        for raw in [
            "https://@example.com",
            "https://:@example.com",
            "http:///v1",
            "https://example.com?",
            "https://example.com#",
            "ftp://example.com",
            "https://secret@example.com",
            "https://example.com\\secret",
            "https://example.com\n",
        ] {
            let error = normalize_base_url(raw).unwrap_err();
            assert!(!error.contains("secret"), "{raw}");
        }
    }
    #[test]
    fn grants_are_independent_exact_origins_and_http_is_separate() {
        let auth = |url: &str, grants: &[String], http: &[String]| {
            TrustedEndpoint::authorize(url, "openai-compatible", grants, http)
        };
        assert!(auth(&format!("https://{}/v1", "api.openai.com"), &[], &[]).is_ok());
        assert!(auth("https://api.openai.com.evil/v1", &[], &[]).is_err());
        assert!(auth("https://api.openai.com:444/v1", &[], &[]).is_err());
        assert!(
            auth(
                "https://EXAMPLE.com:443/v1",
                &["https://example.com".into()],
                &[]
            )
            .is_ok()
        );
        let grants = ["http://127.0.0.1:8080".into()];
        assert!(auth("http://127.0.0.1:8080/v1", &grants, &[]).is_err());
        assert!(auth("http://127.0.0.1:8080/v1", &grants, &grants).is_ok());
        assert!(auth("http://127.0.0.1:8081/v1", &grants, &grants).is_err());
    }
    #[test]
    fn metadata_cannot_be_granted() {
        for url in [
            "http://169.254.169.254",
            "http://100.100.100.200",
            "http://168.63.129.16",
            "http://[fd00:ec2::254]",
            "http://[64:ff9b::a9fe:a9fe]",
            "http://[::169.254.169.254]",
            "http://0xa9fea9fe",
            "http://[fe80::1]",
            "http://[::ffff:169.254.169.254]",
            "http://metadata.google.internal",
        ] {
            assert!(
                TrustedEndpoint::authorize(url, "openai-compatible", &[url.into()], &[url.into()])
                    .is_err(),
                "{url}"
            );
        }
    }
}
