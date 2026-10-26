//! Deterministic detection and typed replacement of high-confidence personal data.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::LazyLock;

use chrono::NaiveDate;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PiiKind {
    Email,
    Phone,
    PaymentCard,
    IdentityDocument,
}

impl PiiKind {
    fn label(self) -> &'static str {
        match self {
            Self::Email => "EMAIL",
            Self::Phone => "PHONE",
            Self::PaymentCard => "PAYMENT_CARD",
            Self::IdentityDocument => "ID_DOCUMENT",
        }
    }
}

#[derive(Debug)]
struct PiiFinding {
    range: Range<usize>,
    kind: PiiKind,
    identity: String,
}

#[derive(Default)]
pub(super) struct PiiRedactor {
    replacements: BTreeMap<(PiiKind, String), String>,
    next_by_kind: BTreeMap<PiiKind, usize>,
}

impl PiiRedactor {
    pub(super) fn redact_text(&mut self, text: &str) -> String {
        let findings = findings(text);
        if findings.is_empty() {
            return text.to_owned();
        }
        let mut redacted = String::with_capacity(text.len());
        let mut cursor = 0;
        for finding in findings {
            redacted.push_str(&text[cursor..finding.range.start]);
            redacted.push_str(&self.placeholder(finding.kind, finding.identity));
            cursor = finding.range.end;
        }
        redacted.push_str(&text[cursor..]);
        redacted
    }

    fn placeholder(&mut self, kind: PiiKind, identity: String) -> String {
        if let Some(existing) = self.replacements.get(&(kind, identity.clone())) {
            return existing.clone();
        }
        let next = self.next_by_kind.entry(kind).or_default();
        *next += 1;
        let placeholder = format!("<{}_{next}>", kind.label());
        self.replacements
            .insert((kind, identity), placeholder.clone());
        placeholder
    }
}

pub(super) fn contains_pii(text: &str) -> bool {
    !findings(text).is_empty()
}

fn findings(text: &str) -> Vec<PiiFinding> {
    let mut candidates = Vec::new();
    scan_emails(text, &mut candidates);
    scan_identity_documents(text, &mut candidates);
    scan_payment_cards(text, &mut candidates);
    scan_phones(text, &mut candidates);

    candidates.sort_by_key(|finding| (finding.kind, finding.range.start));
    let mut selected: Vec<PiiFinding> = Vec::new();
    for finding in candidates {
        if selected
            .iter()
            .any(|existing| ranges_overlap(&existing.range, &finding.range))
        {
            continue;
        }
        selected.push(finding);
    }
    selected.sort_by_key(|finding| finding.range.start);
    selected
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+"#,
    )
    .expect("email detector")
});

fn scan_emails(text: &str, findings: &mut Vec<PiiFinding>) {
    for matched in EMAIL.find_iter(text) {
        if !email_boundary(text, matched.start(), matched.end()) || !valid_email(matched.as_str()) {
            continue;
        }
        findings.push(PiiFinding {
            range: matched.range(),
            kind: PiiKind::Email,
            identity: matched.as_str().to_ascii_lowercase(),
        });
    }
}

fn valid_email(candidate: &str) -> bool {
    let Some((local, domain)) = candidate.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || domain.len() > 253
    {
        return false;
    }
    domain.rsplit_once('.').is_some_and(|(_, suffix)| {
        suffix.len() >= 2 && suffix.bytes().any(|byte| byte.is_ascii_alphabetic())
    })
}

fn email_boundary(text: &str, start: usize, end: usize) -> bool {
    let invalid_boundary = |character: char| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '%' | '+' | '-' | '@')
    };
    !text[..start]
        .chars()
        .next_back()
        .is_some_and(invalid_boundary)
        && !text[end..].chars().next().is_some_and(invalid_boundary)
}

fn scan_phones(text: &str, findings: &mut Vec<PiiFinding>) {
    let bytes = text.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] != b'+' || position > 0 && bytes[position - 1].is_ascii_alphanumeric() {
            position += 1;
            continue;
        }
        let start = position;
        position += 1;
        let mut end = position;
        while end < bytes.len()
            && (bytes[end].is_ascii_digit()
                || matches!(bytes[end], b' ' | b'\t' | b'(' | b')' | b'.' | b'-'))
        {
            end += 1;
        }
        while end > position && matches!(bytes[end - 1], b' ' | b'\t' | b'(' | b')' | b'.' | b'-') {
            end -= 1;
        }
        let digits: String = text[position..end]
            .bytes()
            .filter(u8::is_ascii_digit)
            .map(char::from)
            .collect();
        if (8..=15).contains(&digits.len())
            && !digits.starts_with('0')
            && !bytes.get(end).is_some_and(u8::is_ascii_digit)
        {
            findings.push(PiiFinding {
                range: start..end,
                kind: PiiKind::Phone,
                identity: digits,
            });
        }
        position = end.max(start + 1);
    }
}

fn scan_payment_cards(text: &str, findings: &mut Vec<PiiFinding>) {
    let bytes = text.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        if !bytes[position].is_ascii_digit()
            || position > 0 && bytes[position - 1].is_ascii_alphanumeric()
        {
            position += 1;
            continue;
        }
        let start = position;
        let mut end = position;
        while end < bytes.len()
            && (bytes[end].is_ascii_digit() || matches!(bytes[end], b' ' | b'-'))
        {
            end += 1;
        }
        while end > start && matches!(bytes[end - 1], b' ' | b'-') {
            end -= 1;
        }
        let digits: String = text[start..end]
            .bytes()
            .filter(u8::is_ascii_digit)
            .map(char::from)
            .collect();
        if recognized_card_network(&digits)
            && luhn_valid(&digits)
            && !bytes.get(end).is_some_and(u8::is_ascii_alphanumeric)
        {
            findings.push(PiiFinding {
                range: start..end,
                kind: PiiKind::PaymentCard,
                identity: digits,
            });
        }
        position = end.max(start + 1);
    }
}

fn recognized_card_network(digits: &str) -> bool {
    let prefix = |length: usize| digits.get(..length)?.parse::<u32>().ok();
    match digits.len() {
        13 => digits.starts_with('4'),
        15 => matches!(prefix(2), Some(34 | 37)),
        16 => {
            digits.starts_with('4')
                || matches!(prefix(2), Some(51..=55 | 62 | 65))
                || matches!(prefix(3), Some(644..=649))
                || matches!(prefix(4), Some(2221..=2720 | 3528..=3589 | 6011))
        }
        17 | 18 => matches!(prefix(2), Some(62)) || matches!(prefix(4), Some(3528..=3589)),
        19 => {
            digits.starts_with('4')
                || matches!(prefix(2), Some(62 | 65))
                || matches!(prefix(4), Some(3528..=3589 | 6011))
        }
        _ => false,
    }
}

fn luhn_valid(digits: &str) -> bool {
    let sum: u32 = digits
        .bytes()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            let mut value = u32::from(digit - b'0');
            if index % 2 == 1 {
                value *= 2;
                if value > 9 {
                    value -= 9;
                }
            }
            value
        })
        .sum();
    sum.is_multiple_of(10)
}

static CHINESE_RESIDENT_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[1-9][0-9]{5}(?:18|19|20)[0-9]{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12][0-9]|3[01])[0-9]{3}[0-9X]")
        .expect("Chinese resident identity document detector")
});

fn scan_identity_documents(text: &str, findings: &mut Vec<PiiFinding>) {
    for matched in CHINESE_RESIDENT_ID.find_iter(text) {
        let before_is_alphanumeric = text[..matched.start()]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        let after_is_alphanumeric = text[matched.end()..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        if before_is_alphanumeric || after_is_alphanumeric || !resident_id_valid(matched.as_str()) {
            continue;
        }
        findings.push(PiiFinding {
            range: matched.range(),
            kind: PiiKind::IdentityDocument,
            identity: matched.as_str().to_ascii_uppercase(),
        });
    }
}

fn resident_id_valid(candidate: &str) -> bool {
    let Some(date) = candidate.get(6..14) else {
        return false;
    };
    let Ok(year) = date[..4].parse() else {
        return false;
    };
    let Ok(month) = date[4..6].parse() else {
        return false;
    };
    let Ok(day) = date[6..].parse() else {
        return false;
    };
    if NaiveDate::from_ymd_opt(year, month, day).is_none() {
        return false;
    }

    const WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECKSUMS: [u8; 11] = *b"10X98765432";
    let bytes = candidate.as_bytes();
    let sum: u32 = bytes[..17]
        .iter()
        .zip(WEIGHTS)
        .map(|(digit, weight)| u32::from(*digit - b'0') * weight)
        .sum();
    bytes[17].to_ascii_uppercase() == CHECKSUMS[(sum % 11) as usize]
}
