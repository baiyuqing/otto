use super::*;

fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        output_tokens: 4,
        cached_input_tokens: 60,
        cache_write_input_tokens: Some(30),
        cache_write_1h_input_tokens: Some(10),
    }
}

#[test]
fn validates_cache_partitions_and_preserves_optional_zero_on_the_wire() {
    usage().validate().unwrap();
    for invalid in [
        Usage {
            cache_write_input_tokens: Some(-1),
            ..usage()
        },
        Usage {
            cache_write_input_tokens: Some(41),
            ..usage()
        },
        Usage {
            cache_write_1h_input_tokens: Some(-1),
            ..usage()
        },
        Usage {
            cache_write_1h_input_tokens: Some(31),
            ..usage()
        },
        Usage {
            cache_write_input_tokens: None,
            ..usage()
        },
    ] {
        assert!(invalid.validate().is_err());
    }
    let absent: Usage = serde_json::from_str(r#"{"input_tokens":1,"output_tokens":2}"#).unwrap();
    assert_eq!(absent.cache_write_input_tokens, None);
    assert!(
        !serde_json::to_string(&absent)
            .unwrap()
            .contains("cache_write")
    );
    let zero = Usage {
        cache_write_input_tokens: Some(0),
        cache_write_1h_input_tokens: Some(0),
        ..absent
    };
    let encoded = serde_json::to_string(&zero).unwrap();
    assert!(encoded.contains("\"cache_write_input_tokens\":0"));
    assert_eq!(serde_json::from_str::<Usage>(&encoded).unwrap(), zero);
}

#[test]
fn totals_require_complete_breakdowns_and_stay_valid_when_saturated() {
    let total = usage().saturating_add(usage());
    assert_eq!(total.input_tokens, 200);
    assert_eq!(total.cache_write_input_tokens, Some(60));
    assert_eq!(total.cache_write_1h_input_tokens, Some(20));
    let mixed = total.saturating_add(Usage {
        input_tokens: 3,
        ..Default::default()
    });
    assert_eq!(mixed.cache_write_input_tokens, None);
    assert_eq!(mixed.cache_write_1h_input_tokens, None);
    let saturated = usage().saturating_add(Usage {
        input_tokens: i64::MAX,
        cached_input_tokens: i64::MAX,
        cache_write_input_tokens: Some(0),
        cache_write_1h_input_tokens: Some(0),
        ..Default::default()
    });
    saturated.validate().unwrap();
    assert_eq!(saturated.input_tokens, i64::MAX);
    assert_eq!(saturated.cache_write_input_tokens, None);
}
