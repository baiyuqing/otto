//! The slice of Go's `net/url` that Otto's credential detection depends on.
//!
//! [`crate::urlprivacy`] must agree with Go byte for byte about which proxy
//! URLs parse and which do not, because a URL that Go accepts but Rust rejects
//! would be reported as an ambiguous credential and would make
//! `ResolveEnvironment` refuse an ordinary proxy setting. Only the parts Otto
//! reaches are ported: [`parse`], [`path_unescape`], and [`Userinfo`].
//!
//! Everything works on bytes rather than `str` because Go strings may hold
//! arbitrary bytes and percent escapes routinely decode to invalid UTF-8.
//! Callers canonicalize with [`crate::safetext::canonicalize_utf8`].
//!
//! These items are pure functions with no shared state, so they are `Send`,
//! `Sync`, and free of cancellation concerns. Errors carry no detail: every
//! caller only distinguishes "Go would have returned an error" from "Go would
//! have succeeded", so the error type is the unit type.

use std::net::IpAddr;

/// Which part of a URL a byte is being escaped or unescaped for. Go selects
/// escaping rules per component; the discriminants mirror Go's `encoding`
/// constants in name only, never by value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Encoding {
    Path,
    PathSegment,
    Host,
    Zone,
    UserPassword,
    QueryComponent,
    Fragment,
}

/// Go's `shouldEscape`, transcribed from `gen_encoding_table.go`'s reference
/// implementation rather than from the generated lookup table.
fn should_escape(c: u8, mode: Encoding) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    if mode == Encoding::Host || mode == Encoding::Zone {
        match c {
            b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':'
            | b'[' | b']' | b'<' | b'>' | b'"' => return false,
            _ => {}
        }
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => return false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Encoding::Path => return c == b'?',
            Encoding::PathSegment => {
                return c == b'/' || c == b';' || c == b',' || c == b'?';
            }
            Encoding::UserPassword => {
                return c == b'@' || c == b'/' || c == b'?' || c == b':';
            }
            Encoding::QueryComponent => return true,
            Encoding::Fragment => return false,
            Encoding::Host | Encoding::Zone => {}
        },
        _ => {}
    }
    if mode == Encoding::Fragment {
        match c {
            b'!' | b'(' | b')' | b'*' => return false,
            _ => {}
        }
    }
    true
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// Precondition: `is_hex(c)`. Go's `unhex`, which relies on the ASCII layout of
/// the letter and digit ranges.
fn unhex(c: u8) -> u8 {
    9 * (c >> 6) + (c & 15)
}

/// Go's `url.unescape`. Returns `Err(())` where Go returns `EscapeError` or
/// `InvalidHostError`.
pub(crate) fn unescape(s: &[u8], mode: Encoding) -> Result<Vec<u8>, ()> {
    let mut escapes = 0usize;
    let mut has_plus = false;
    let mut index = 0usize;
    while index < s.len() {
        match s[index] {
            b'%' => {
                escapes += 1;
                if index + 2 >= s.len() || !is_hex(s[index + 1]) || !is_hex(s[index + 2]) {
                    return Err(());
                }
                if mode == Encoding::Host
                    && unhex(s[index + 1]) < 8
                    && &s[index..index + 3] != b"%25"
                {
                    return Err(());
                }
                if mode == Encoding::Zone {
                    let value = (unhex(s[index + 1]) << 4) | unhex(s[index + 2]);
                    if &s[index..index + 3] != b"%25"
                        && value != b' '
                        && should_escape(value, Encoding::Host)
                    {
                        return Err(());
                    }
                }
                index += 3;
            }
            b'+' => {
                has_plus = mode == Encoding::QueryComponent;
                index += 1;
            }
            byte => {
                if (mode == Encoding::Host || mode == Encoding::Zone)
                    && byte < 0x80
                    && should_escape(byte, mode)
                {
                    return Err(());
                }
                index += 1;
            }
        }
    }

    if escapes == 0 && !has_plus {
        return Ok(s.to_vec());
    }

    let unescaped_plus = if mode == Encoding::QueryComponent {
        b' '
    } else {
        b'+'
    };
    let mut out = Vec::with_capacity(s.len() - 2 * escapes);
    let mut index = 0usize;
    while index < s.len() {
        match s[index] {
            b'%' => {
                out.push((unhex(s[index + 1]) << 4) | unhex(s[index + 2]));
                index += 3;
            }
            b'+' => {
                out.push(unescaped_plus);
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    Ok(out)
}

/// Go's `url.escape`.
pub(crate) fn escape(s: &[u8], mode: Encoding) -> Vec<u8> {
    const UPPERHEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        if c == b' ' && mode == Encoding::QueryComponent {
            out.push(b'+');
        } else if should_escape(c, mode) {
            out.push(b'%');
            out.push(UPPERHEX[(c >> 4) as usize]);
            out.push(UPPERHEX[(c & 15) as usize]);
        } else {
            out.push(c);
        }
    }
    out
}

/// Go's `url.PathUnescape`: percent decoding that leaves `+` alone.
pub(crate) fn path_unescape(s: &[u8]) -> Result<Vec<u8>, ()> {
    unescape(s, Encoding::PathSegment)
}

/// Go's `url.Userinfo`: an immutable username with an optionally set password.
/// A set but empty password is distinct from an absent one, exactly as in Go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Userinfo {
    username: Vec<u8>,
    password: Vec<u8>,
    password_set: bool,
}

impl Userinfo {
    /// Go's `url.User`.
    fn user(username: Vec<u8>) -> Self {
        Self {
            username,
            password: Vec::new(),
            password_set: false,
        }
    }

    /// Go's `url.UserPassword`.
    fn user_password(username: Vec<u8>, password: Vec<u8>) -> Self {
        Self {
            username,
            password,
            password_set: true,
        }
    }

    /// The decoded username.
    pub(crate) fn username(&self) -> &[u8] {
        &self.username
    }

    /// The decoded password, or `None` when the URL carried no `:` separator.
    pub(crate) fn password(&self) -> Option<&[u8]> {
        if self.password_set {
            Some(&self.password)
        } else {
            None
        }
    }

    /// Go's `(*Userinfo).String`: the re-encoded `username[:password]` form.
    pub(crate) fn encoded(&self) -> Vec<u8> {
        let mut out = escape(&self.username, Encoding::UserPassword);
        if self.password_set {
            out.push(b':');
            out.extend_from_slice(&escape(&self.password, Encoding::UserPassword));
        }
        out
    }
}

/// The subset of Go's `url.URL` that Otto reads. Fields Otto never inspects
/// (`RawPath`, `RawFragment`, `ForceQuery`, `OmitHost`) are not stored, because
/// they only affect `URL.String`, which Otto does not call.
#[derive(Clone, Debug, Default)]
pub(crate) struct Url {
    pub(crate) scheme: Vec<u8>,
    pub(crate) opaque: Vec<u8>,
    pub(crate) user: Option<Userinfo>,
    pub(crate) host: Vec<u8>,
    pub(crate) path: Vec<u8>,
    pub(crate) raw_query: Vec<u8>,
    pub(crate) fragment: Vec<u8>,
}

fn contains_ctl_byte(s: &[u8]) -> bool {
    s.iter().any(|&b| b < b' ' || b == 0x7f)
}

/// Go's `url.getScheme`. `Err(())` stands for Go's "missing protocol scheme".
fn get_scheme(raw: &[u8]) -> Result<(&[u8], &[u8]), ()> {
    for (index, &c) in raw.iter().enumerate() {
        if c.is_ascii_alphabetic() {
            continue;
        }
        if c.is_ascii_digit() || c == b'+' || c == b'-' || c == b'.' {
            if index == 0 {
                return Ok((b"", raw));
            }
            continue;
        }
        if c == b':' {
            if index == 0 {
                return Err(());
            }
            return Ok((&raw[..index], &raw[index + 1..]));
        }
        return Ok((b"", raw));
    }
    Ok((b"", raw))
}

/// Go's `url.validOptionalPort`: empty, or `:` followed by decimal digits.
fn valid_optional_port(port: &[u8]) -> bool {
    if port.is_empty() {
        return true;
    }
    port[0] == b':' && port[1..].iter().all(|b| b.is_ascii_digit())
}

/// Go's `url.validUserinfo`. Every permitted rune is ASCII, so any byte at or
/// above 0x80 fails here just as a decoded non-ASCII rune fails in Go.
fn valid_userinfo(s: &[u8]) -> bool {
    s.iter().all(|&c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'.'
                    | b'_'
                    | b':'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b'%'
                    | b'@'
            )
    })
}

/// Go's `netip.ParseAddr`, reduced to the two facts `parseHost` needs: whether
/// the literal parses at all, and whether it is a plain IPv4 address.
fn parse_ip_literal(host: &[u8]) -> Result<bool, ()> {
    let text = std::str::from_utf8(host).map_err(|_| ())?;
    let (address, zone) = match text.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (text, None),
    };
    let address: IpAddr = address.parse().map_err(|_| ())?;
    if let Some(zone) = zone
        && (zone.is_empty() || address.is_ipv4())
    {
        return Err(());
    }
    Ok(address.is_ipv4())
}

/// Go's `url.parseHost`: parses `host[:port]`, including bracketed IPv6
/// literals with RFC 6874 zone identifiers.
fn parse_host(scheme: &[u8], host: &[u8]) -> Result<Vec<u8>, ()> {
    let open_bracket = host.iter().rposition(|&b| b == b'[');
    match open_bracket {
        Some(index) if index > 0 => return Err(()),
        Some(_) => {
            let close_bracket = host.iter().rposition(|&b| b == b']').ok_or(())?;
            let colon_port = &host[close_bracket + 1..];
            if !valid_optional_port(colon_port) {
                return Err(());
            }
            let unescaped_colon_port = unescape(colon_port, Encoding::Host)?;

            let hostname = &host[1..close_bracket];
            let zone = hostname
                .windows(3)
                .position(|window| window == b"%25")
                .unwrap_or(usize::MAX);
            let unescaped_hostname = if zone == usize::MAX {
                unescape(hostname, Encoding::Host)?
            } else {
                let mut out = unescape(&hostname[..zone], Encoding::Host)?;
                out.extend_from_slice(&unescape(&hostname[zone..], Encoding::Zone)?);
                out
            };

            if parse_ip_literal(&unescaped_hostname)? {
                return Err(());
            }
            let mut out = Vec::with_capacity(unescaped_hostname.len() + 2);
            out.push(b'[');
            out.extend_from_slice(&unescaped_hostname);
            out.push(b']');
            out.extend_from_slice(&unescaped_colon_port);
            return Ok(out);
        }
        None => {}
    }

    if let Some(first_colon) = host.iter().position(|&b| b == b':') {
        let last_colon = host.iter().rposition(|&b| b == b':').unwrap_or(first_colon);
        // RFC 3986 bars colons in the host, but Go keeps accepting
        // comma-separated `host:port` lists outside http and https.
        let colon = if last_colon != first_colon && scheme != b"http" && scheme != b"https" {
            last_colon
        } else {
            first_colon
        };
        if !valid_optional_port(&host[colon..]) {
            return Err(());
        }
    }
    unescape(host, Encoding::Host)
}

/// Go's `url.parseAuthority`. The userinfo split takes the *last* `@`, so
/// `user:p@ss@host` keeps `user:p@ss` as the credential.
fn parse_authority(scheme: &[u8], authority: &[u8]) -> Result<(Option<Userinfo>, Vec<u8>), ()> {
    let at = authority.iter().rposition(|&b| b == b'@');
    let host = match at {
        Some(index) => parse_host(scheme, &authority[index + 1..])?,
        None => parse_host(scheme, authority)?,
    };
    let Some(at) = at else {
        return Ok((None, host));
    };
    let userinfo = &authority[..at];
    if !valid_userinfo(userinfo) {
        return Err(());
    }
    let user = match userinfo.iter().position(|&b| b == b':') {
        None => Userinfo::user(unescape(userinfo, Encoding::UserPassword)?),
        Some(colon) => Userinfo::user_password(
            unescape(&userinfo[..colon], Encoding::UserPassword)?,
            unescape(&userinfo[colon + 1..], Encoding::UserPassword)?,
        ),
    };
    Ok((Some(user), host))
}

/// Go's `url.parse` with `viaRequest` fixed to false.
fn parse_relative(raw: &[u8]) -> Result<Url, ()> {
    if contains_ctl_byte(raw) {
        return Err(());
    }
    let mut url = Url::default();
    if raw == b"*" {
        url.path = b"*".to_vec();
        return Ok(url);
    }

    let (scheme, mut rest) = get_scheme(raw)?;
    url.scheme = scheme.to_ascii_lowercase();

    if rest.ends_with(b"?") && rest.iter().filter(|&&b| b == b'?').count() == 1 {
        rest = &rest[..rest.len() - 1];
    } else if let Some(mark) = rest.iter().position(|&b| b == b'?') {
        url.raw_query = rest[mark + 1..].to_vec();
        rest = &rest[..mark];
    }

    if !rest.starts_with(b"/") {
        if !url.scheme.is_empty() {
            url.opaque = rest.to_vec();
            return Ok(url);
        }
        let segment = match rest.iter().position(|&b| b == b'/') {
            Some(slash) => &rest[..slash],
            None => rest,
        };
        if segment.contains(&b':') {
            return Err(());
        }
    }

    if (!url.scheme.is_empty() || !rest.starts_with(b"///")) && rest.starts_with(b"//") {
        let after = &rest[2..];
        let (authority, remainder) = match after.iter().position(|&b| b == b'/') {
            Some(slash) => (&after[..slash], &after[slash..]),
            None => (after, &after[after.len()..]),
        };
        let (user, host) = parse_authority(&url.scheme, authority)?;
        url.user = user;
        url.host = host;
        rest = remainder;
    }

    url.path = unescape(rest, Encoding::Path)?;
    Ok(url)
}

/// Go's `url.Parse`: cuts the fragment first, then parses the remainder as a
/// possibly relative reference. `Err(())` means Go would have returned an
/// error; the message is never inspected.
pub(crate) fn parse(raw: &[u8]) -> Result<Url, ()> {
    let (head, fragment) = match raw.iter().position(|&b| b == b'#') {
        Some(hash) => (&raw[..hash], &raw[hash + 1..]),
        None => (raw, &raw[raw.len()..]),
    };
    let mut url = parse_relative(head)?;
    if fragment.is_empty() {
        return Ok(url);
    }
    url.fragment = unescape(fragment, Encoding::Fragment)?;
    Ok(url)
}
