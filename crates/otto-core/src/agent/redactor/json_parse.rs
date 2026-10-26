//! JSON parsing that redacts every string and the byte cursor it reads with.

use super::*;

impl Redactor {
    /// Parses `raw` and re-encodes it with every string redacted. `None` means
    /// fail closed.
    pub(super) fn redact_json_text(&self, raw: &str) -> Option<String> {
        let mut parser = Parser {
            bytes: raw.as_bytes(),
            index: 0,
        };
        let value = self.parse_value(&mut parser)?;
        parser.skip_whitespace();
        if parser.index != parser.bytes.len() {
            // A second value is not a document.
            return None;
        }
        Some(value)
    }

    /// Parses provider-bound JSON while preserving scalar literals verbatim
    /// and applying heuristic redaction at each string's own tree position.
    pub(super) fn redact_egress_json_text(
        &self,
        mut pii: Option<&mut PiiRedactor>,
        raw: &str,
    ) -> Option<String> {
        let mut parser = Parser {
            bytes: raw.as_bytes(),
            index: 0,
        };
        let value = self.parse_egress_value(&mut parser, &mut pii)?;
        parser.skip_whitespace();
        (parser.index == parser.bytes.len()).then_some(value)
    }

    fn parse_egress_value(
        &self,
        parser: &mut Parser<'_>,
        pii: &mut Option<&mut PiiRedactor>,
    ) -> Option<String> {
        let mut stack: Vec<EgressFrame> = Vec::new();
        'read: loop {
            parser.skip_whitespace();
            let sensitive_association =
                matches!(stack.last(), Some(EgressFrame::Object(_, Some((_, true)))));
            let mut value = match parser.peek()? {
                b'{' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_EGRESS_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b'}' {
                        parser.index += 1;
                        "{}".to_owned()
                    } else {
                        let key = self.parse_egress_member_key(parser, pii.is_some())?;
                        stack.push(EgressFrame::Object(Vec::new(), Some(key)));
                        continue 'read;
                    }
                }
                b'[' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_EGRESS_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b']' {
                        parser.index += 1;
                        "[]".to_owned()
                    } else {
                        stack.push(EgressFrame::Array(Vec::new()));
                        continue 'read;
                    }
                }
                b'"' => {
                    let secret_safe = redact_json_string_value(
                        self,
                        &parser.parse_string()?,
                        sensitive_association,
                    )?;
                    let value = match pii.as_deref_mut() {
                        Some(pii) => pii.redact_text(&secret_safe),
                        None => secret_safe,
                    };
                    encode_json_string(&value)
                }
                _ => parser.parse_scalar()?,
            };

            loop {
                match stack.last_mut() {
                    None => return Some(value),
                    Some(EgressFrame::Array(items)) => {
                        items.push(value);
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => continue 'read,
                            b']' => {}
                            _ => return None,
                        }
                        let Some(EgressFrame::Array(items)) = stack.pop() else {
                            unreachable!("the frame was just matched as an array")
                        };
                        value = encode_array(&items);
                    }
                    Some(EgressFrame::Object(members, pending_key)) => {
                        let (key, _) = pending_key.take()?;
                        members.push((key, value));
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => {
                                *pending_key =
                                    Some(self.parse_egress_member_key(parser, pii.is_some())?);
                                continue 'read;
                            }
                            b'}' => {}
                            _ => return None,
                        }
                        let Some(EgressFrame::Object(members, _)) = stack.pop() else {
                            unreachable!("the frame was just matched as an object")
                        };
                        value = encode_object(members);
                    }
                }
            }
        }
    }

    fn parse_egress_member_key(
        &self,
        parser: &mut Parser<'_>,
        detect_pii: bool,
    ) -> Option<(String, bool)> {
        parser.skip_whitespace();
        if parser.peek()? != b'"' {
            return None;
        }
        let original = parser.parse_string()?;
        if detect_pii && contains_pii(&original) {
            return None;
        }
        let sensitive = sensitive_key(&original);
        let exact = self.redact_string(&original);
        let ranges = independent_finding_ranges(&exact);
        let key = if ranges.is_empty() {
            exact
        } else {
            redact_ranges(self, &exact, ranges)?
        };
        parser.skip_whitespace();
        if parser.next()? != b':' {
            return None;
        }
        Some((key, sensitive))
    }

    /// An iterative JSON parse that emits redacted JSON text directly. It uses
    /// an explicit stack rather than recursion so the supported depth of
    /// [`MAXIMUM_JSON_DEPTH`] cannot overflow the thread stack.
    fn parse_value(&self, parser: &mut Parser<'_>) -> Option<String> {
        let mut stack: Vec<Frame> = Vec::new();
        'read: loop {
            parser.skip_whitespace();
            let mut value = match parser.peek()? {
                b'{' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b'}' {
                        parser.index += 1;
                        "{}".to_owned()
                    } else {
                        let key = self.parse_member_key(parser)?;
                        stack.push(Frame::Object(Vec::new(), Some(key)));
                        continue 'read;
                    }
                }
                b'[' => {
                    parser.index += 1;
                    if stack.len() >= MAXIMUM_JSON_DEPTH {
                        return None;
                    }
                    parser.skip_whitespace();
                    if parser.peek()? == b']' {
                        parser.index += 1;
                        "[]".to_owned()
                    } else {
                        stack.push(Frame::Array(Vec::new()));
                        continue 'read;
                    }
                }
                b'"' => encode_json_string(&self.redact_string(&parser.parse_string()?)),
                _ => parser.parse_scalar()?,
            };

            // Hand the finished value to its container, closing containers
            // that have no members left.
            loop {
                match stack.last_mut() {
                    None => return Some(value),
                    Some(Frame::Array(items)) => {
                        items.push(value);
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => continue 'read,
                            b']' => {}
                            _ => return None,
                        }
                        let Some(Frame::Array(items)) = stack.pop() else {
                            unreachable!("the frame was just matched as an array")
                        };
                        value = encode_array(&items);
                    }
                    Some(Frame::Object(members, pending_key)) => {
                        members.push((pending_key.take()?, value));
                        parser.skip_whitespace();
                        match parser.next()? {
                            b',' => {
                                *pending_key = Some(self.parse_member_key(parser)?);
                                continue 'read;
                            }
                            b'}' => {}
                            _ => return None,
                        }
                        let Some(Frame::Object(members, _)) = stack.pop() else {
                            unreachable!("the frame was just matched as an object")
                        };
                        value = encode_object(members);
                    }
                }
            }
        }
    }

    /// A redacted object member name plus its `:` separator.
    fn parse_member_key(&self, parser: &mut Parser<'_>) -> Option<String> {
        parser.skip_whitespace();
        if parser.peek()? != b'"' {
            return None;
        }
        let key = self.redact_string(&parser.parse_string()?);
        parser.skip_whitespace();
        if parser.next()? != b':' {
            return None;
        }
        Some(key)
    }
}

/// A container being filled while [`Redactor::parse_value`] descends.
enum Frame {
    Array(Vec<String>),
    /// Members in source order plus the key whose value is being read.
    Object(Vec<(String, String)>, Option<String>),
}

/// Provider-egress JSON frame retaining the pending key's sensitivity.
enum EgressFrame {
    Array(Vec<String>),
    Object(Vec<(String, String)>, Option<(String, bool)>),
}

/// A byte cursor over JSON text.
struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.index += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.index += 1;
        }
    }

    /// Decodes a JSON string starting at the opening quote.
    ///
    /// Escape handling: an unpaired surrogate becomes U+FFFD, and any byte that
    /// is not valid UTF-8 becomes U+FFFD too. A raw control character or an
    /// unknown escape is a parse failure.
    fn parse_string(&mut self) -> Option<String> {
        self.index += 1;
        let mut decoded: Vec<u8> = Vec::new();
        loop {
            match self.next()? {
                b'"' => return Some(safetext::canonicalize_utf8(&decoded)),
                b'\\' => match self.next()? {
                    b'"' => decoded.push(b'"'),
                    b'\\' => decoded.push(b'\\'),
                    b'/' => decoded.push(b'/'),
                    b'b' => decoded.push(0x08),
                    b'f' => decoded.push(0x0c),
                    b'n' => decoded.push(b'\n'),
                    b'r' => decoded.push(b'\r'),
                    b't' => decoded.push(b'\t'),
                    b'u' => {
                        let mut buffer = [0u8; 4];
                        let text = self.parse_unicode_escape()?.encode_utf8(&mut buffer);
                        decoded.extend_from_slice(text.as_bytes());
                    }
                    _ => return None,
                },
                byte if byte < 0x20 => return None,
                byte => decoded.push(byte),
            }
        }
    }

    /// The rune of a `\u` escape, with the leading `\u` already consumed.
    fn parse_unicode_escape(&mut self) -> Option<char> {
        const REPLACEMENT: char = '\u{FFFD}';
        let leading = self.parse_hex4()?;
        if (0xDC00..0xE000).contains(&leading) {
            return Some(REPLACEMENT);
        }
        if !(0xD800..0xDC00).contains(&leading) {
            return char::from_u32(leading).or(Some(REPLACEMENT));
        }
        let resume = self.index;
        if self.bytes.get(self.index) != Some(&b'\\')
            || self.bytes.get(self.index + 1) != Some(&b'u')
        {
            return Some(REPLACEMENT);
        }
        self.index += 2;
        let Some(trailing) = self.parse_hex4() else {
            self.index = resume;
            return Some(REPLACEMENT);
        };
        if !(0xDC00..0xE000).contains(&trailing) {
            self.index = resume;
            return Some(REPLACEMENT);
        }
        let combined = 0x10000 + ((leading - 0xD800) << 10) + (trailing - 0xDC00);
        char::from_u32(combined).or(Some(REPLACEMENT))
    }

    fn parse_hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.index..self.index + 4)?;
        let mut value = 0u32;
        for &digit in digits {
            value = value * 16 + char::from(digit).to_digit(16)?;
        }
        self.index += 4;
        Some(value)
    }

    /// A literal or a number, copied through verbatim so that number
    /// precision and formatting survive.
    fn parse_scalar(&mut self) -> Option<String> {
        for literal in ["true", "false", "null"] {
            if self.bytes[self.index..].starts_with(literal.as_bytes()) {
                self.index += literal.len();
                return Some(literal.to_owned());
            }
        }
        let start = self.index;
        if self.peek()? == b'-' {
            self.index += 1;
        }
        match self.peek()? {
            b'0' => self.index += 1,
            b'1'..=b'9' => self.skip_digits(),
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.index += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            self.skip_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            self.skip_digits();
        }
        String::from_utf8(self.bytes[start..self.index].to_vec()).ok()
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.index += 1;
        }
    }
}
