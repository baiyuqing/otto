//! Best-effort credential detection without registered secret values.

use super::*;

#[test]
fn recording_redacts_unknown_credentials_and_keeps_connection_details() {
    let redactor = Redactor::new(&[]);
    let credentials = [
        (
            format!("prefix client_secret={} suffix", "opaque-value-123"),
            "opaque-value-123".to_string(),
        ),
        (
            format!("AK={}", ["LTAI", "abcdefghijklmnopqrst"].concat()),
            ["LTAI", "abcdefghijklmnopqrst"].concat(),
        ),
        (
            format!("credentials: {}", "opaque-login-value"),
            "opaque-login-value".to_string(),
        ),
        (
            format!(
                "wallet_private_key = '{}'",
                ["01234567", "89abcdef"].concat()
            ),
            "0123456789abcdef".into(),
        ),
        (
            format!("key {}", ["ASIA", "ABCDEFGHIJKLMNOP"].concat()),
            ["ASIA", "ABCDEFGHIJKLMNOP"].concat(),
        ),
        (
            "host=db.internal password='a spaced password' dbname=app".into(),
            "a spaced password".into(),
        ),
        (
            "mysql://reader:p%40ssword@db.internal/app?ssl=true".into(),
            "p%40ssword".into(),
        ),
        (
            "reader:short@tcp(db.internal:4000)/app".into(),
            "short".into(),
        ),
        (
            "https://service.invalid/path?client_secret=opaque-value-123&mode=read".into(),
            "opaque-value-123".into(),
        ),
        ("mongodb://reader:pw@db.internal/app".into(), "pw".into()),
    ];
    for (text, secret) in credentials {
        let recorded = redactor.redact_record_text(&text);
        assert!(
            !recorded.contains(&secret),
            "credential remained in recording"
        );
        assert!(
            !recorded.is_empty(),
            "ordinary surrounding content was lost"
        );
        let request = Request {
            system_prompt: text,
            ..Request::default()
        };
        let guarded = redactor.guard_provider_request(&request).expect("guarded");
        assert!(
            !guarded.system_prompt.contains(&secret),
            "credential reached provider"
        );
    }
    let url = redactor.redact_record_text("postgres://reader:pw@db.internal/app?sslmode=require");
    assert!(url.contains("reader:"));
    assert!(url.ends_with("@db.internal/app?sslmode=require"));
}

#[test]
fn recording_handles_json_escapes_basic_auth_and_multiline_keys() {
    use base64::Engine;
    let redactor = Redactor::new(&[]);
    let raw = r#"{"oauth":{"client_secret":"opaque\u002dvalue-123"},"number":9007199254740993,"enabled":true}"#;
    let recorded = redactor.redact_record_json(raw);
    let decoded: serde_json::Value = serde_json::from_str(recorded.get()).expect("valid JSON");
    assert_ne!(decoded["oauth"]["client_secret"], "opaque-value-123");
    assert_eq!(decoded["number"].as_u64(), Some(9_007_199_254_740_993));
    assert_eq!(decoded["enabled"], true);
    let authorization =
        base64::engine::general_purpose::STANDARD.encode("synthetic-user:synthetic-password");
    assert!(
        !redactor
            .redact_record_text(&format!("Authorization: Basic {authorization}"))
            .contains(&authorization)
    );
    let pem = [
        "-----BEGIN ",
        "ENCRYPTED PRIVATE KEY-----\nsynthetic-key-body\n-----END ",
        "ENCRYPTED PRIVATE KEY-----",
    ]
    .concat();
    assert!(
        !redactor
            .redact_record_text(&pem)
            .contains("synthetic-key-body")
    );
}

#[test]
fn recording_ignores_placeholders_ids_hashes_and_personal_text() {
    let redactor = Redactor::new(&[]);
    for text in [
        "client_secret=<redacted>",
        "token=your-token",
        "password=${PASSWORD}",
        "ordinary prose about credentials and wallet addresses",
        "hash=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "https://service.invalid/path?mode=read",
        "Contact alice@example.com",
        "wallet_address=0x0123456789abcdef0123456789abcdef01234567",
        "{\n  \"count\": 9007199254740993, \"text\": \"ordinary text\"\n}",
    ] {
        assert_eq!(redactor.redact_record_text(text), text);
    }
}

#[test]
fn recording_detects_bip39_checksum_but_preserves_invalid_phrase() {
    let phrase = format!("{}about", "abandon ".repeat(11)); // Public BIP-39 zero-entropy test vector.
    let redactor = Redactor::new(&[]);
    assert!(
        !redactor
            .redact_record_text(&format!("phrase: {phrase}"))
            .contains(&phrase)
    );
    let invalid = "abandon ".repeat(12).trim_end().to_owned();
    assert_eq!(redactor.redact_record_text(&invalid), invalid);
    assert_eq!(wallet_secrets::finding_ranges(&phrase).len(), 1);
}

#[test]
fn recording_checks_wif_checksum_and_rejects_an_invalid_candidate() {
    use sha2::{Digest, Sha256};
    // Synthetic test material, generated locally and never used for a wallet.
    let mut payload = vec![0x80];
    payload.extend([1u8; 32]);
    payload.push(1);
    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut reversed = Vec::new();
    while payload.iter().any(|byte| *byte != 0) {
        let mut remainder = 0u16;
        for byte in &mut payload {
            let dividend = remainder * 256 + u16::from(*byte);
            *byte = (dividend / 58) as u8;
            remainder = dividend % 58;
        }
        reversed.push(ALPHABET[remainder as usize]);
    }
    reversed.reverse();
    let candidate = String::from_utf8(reversed).unwrap();
    let redactor = Redactor::new(&[]);
    assert!(!redactor.redact_record_text(&candidate).contains(&candidate));
    let mut invalid = candidate.clone();
    invalid.pop();
    invalid.push(if candidate.ends_with('1') { '2' } else { '1' });
    assert_eq!(redactor.redact_record_text(&invalid), invalid);
}

#[test]
fn recording_preserves_identity_and_approval_content() {
    let redactor = Redactor::new(&[]);
    let candidate = "opaque-value-123";
    let message = crate::model::Message {
        id: "message-1".into(),
        blocks: vec![crate::model::Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "call-1".into(),
            tool_name: "echo".into(),
            arguments: Some(
                RawValue::from_string(format!(r#"{{"client_secret":"{candidate}"}}"#)).unwrap(),
            ),
            ..Default::default()
        }],
        ..Default::default()
    };
    let recorded = redactor.redact_record_message(&message);
    assert_eq!(recorded.id, message.id);
    assert_eq!(recorded.blocks[0].tool_call_id, "call-1");
    assert!(
        !recorded.blocks[0]
            .arguments
            .as_ref()
            .unwrap()
            .get()
            .contains(candidate)
    );
    assert!(
        message.blocks[0]
            .arguments
            .as_ref()
            .unwrap()
            .get()
            .contains(candidate)
    );
    let mut approval = message;
    approval.blocks[0].tool_name = "request_approval".into();
    assert_eq!(redactor.redact_record_message(&approval), approval);
}
