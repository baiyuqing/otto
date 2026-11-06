use super::*;

#[test]
fn all_canonical_legacy_layouts_reconstruct_and_reject_modified_expressions() {
    for (family, version) in [
        ("usage", 0),
        ("tasks", 1),
        ("workflow", 0),
        ("skill-checks", 1),
    ] {
        for schema in candidates(family, version).unwrap() {
            let objects = schema_objects(&schema).unwrap();
            let rebuilt = canonical_schema(family, version, &objects).unwrap();
            validate_objects(&objects, &schema_objects(&rebuilt).unwrap()).unwrap();
            let mut altered = objects.clone();
            altered[0] = altered[0].replacen(
                "TEXT NOT NULL",
                "TEXT NOT NULL DEFAULT (lower('UNTRUSTED'))",
                1,
            );
            assert_ne!(altered, objects);
            assert!(canonical_schema(family, version, &altered).is_err());
            let mut extra = objects.clone();
            extra.push("CREATE INDEX unexpected ON unknown(lower(value))".into());
            assert!(canonical_schema(family, version, &extra).is_err());
        }
    }
}
