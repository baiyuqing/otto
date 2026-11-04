//! Local BIP-39 English/checksum and Bitcoin WIF/Base58Check recognition.
//! Format references and wordlist attribution: credential-detection.md.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

static WORDLIST: LazyLock<Vec<&str>> =
    LazyLock::new(|| include_str!("bip39-english.txt").lines().collect());
static WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-zA-Z]+\b").expect("mnemonic words"));
static WIFS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[5KL9c][1-9A-HJ-NP-Za-km-z]{50,51}\b").expect("WIF candidates")
});

pub(super) fn finding_ranges(text: &str) -> Vec<Range<usize>> {
    let mut findings = Vec::new();
    let mut words: Vec<(Range<usize>, usize)> = Vec::new();
    for word in WORDS.find_iter(text) {
        let normalized = word.as_str().to_ascii_lowercase();
        let Ok(index) = WORDLIST.binary_search(&normalized.as_str()) else {
            scan_mnemonics(&words, &mut findings);
            words.clear();
            continue;
        };
        if let Some((previous, _)) = words.last()
            && !text[previous.end..word.start()]
                .chars()
                .all(char::is_whitespace)
        {
            scan_mnemonics(&words, &mut findings);
            words.clear();
        }
        words.push((word.range(), index));
    }
    scan_mnemonics(&words, &mut findings);
    findings.extend(
        WIFS.find_iter(text)
            .filter(|value| valid_wif(value.as_str()))
            .map(|value| value.range()),
    );
    findings
}

fn scan_mnemonics(words: &[(Range<usize>, usize)], findings: &mut Vec<Range<usize>>) {
    let mut start = 0;
    while start + 12 <= words.len() {
        let count = [24, 21, 18, 15, 12].into_iter().find(|count| {
            start + count <= words.len() && valid_mnemonic(&words[start..start + count])
        });
        if let Some(count) = count {
            findings.push(words[start].0.start..words[start + count - 1].0.end);
            start += count;
        } else {
            start += 1;
        }
    }
}

fn valid_mnemonic(words: &[(Range<usize>, usize)]) -> bool {
    let entropy_bits = words.len() * 11 * 32 / 33;
    let checksum_bits = entropy_bits / 32;
    let mut entropy = [0u8; 32];
    let bit = |position: usize| (words[position / 11].1 >> (10 - position % 11)) & 1;
    for position in 0..entropy_bits {
        entropy[position / 8] |= (bit(position) as u8) << (7 - position % 8);
    }
    let checksum = (entropy_bits..entropy_bits + checksum_bits)
        .fold(0, |value, position| (value << 1) | bit(position));
    let digest = Sha256::digest(&entropy[..entropy_bits / 8]);
    checksum == (digest[0] >> (8 - checksum_bits)) as usize
}

fn valid_wif(encoded: &str) -> bool {
    // A WIF is at most 38 decoded bytes, including the four checksum bytes.
    // Fixed-size base conversion avoids an unbounded big-integer dependency.
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut decoded = [0u8; 38];
    for character in encoded.bytes() {
        let Some(digit) = ALPHABET
            .iter()
            .position(|candidate| *candidate == character)
        else {
            return false;
        };
        let mut carry = digit as u16;
        for byte in decoded.iter_mut().rev() {
            carry += u16::from(*byte) * 58;
            *byte = carry as u8;
            carry >>= 8;
        }
        if carry != 0 {
            return false;
        }
    }
    let start = decoded
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(decoded.len());
    let decoded = &decoded[start..];
    if !matches!(decoded.len(), 37 | 38) || !matches!(decoded[0], 0x80 | 0xef) {
        return false;
    }
    let payload_bytes = decoded.len() - 4;
    if payload_bytes == 34 && decoded[33] != 1 {
        return false;
    }
    let digest = Sha256::digest(Sha256::digest(&decoded[..payload_bytes]));
    digest[..4] == decoded[payload_bytes..]
}
