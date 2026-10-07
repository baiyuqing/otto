//! Backend-independent persisted approval envelopes and additive metadata.
use super::{Error, Request, Result};

const REQUEST_FIELDS: &[&str] = &[
    "options",
    "schema_version",
    "id",
    "revision",
    "kind",
    "authority",
    "scope",
    "subject",
    "title",
    "reason",
    "persistent",
    "blocking",
    "created_at",
    "expires_at",
    "state",
    "decision",
    "application",
    "application_error",
];
struct PersistedFields(std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>);
impl<'de> serde::Deserialize<'de> for PersistedFields {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = PersistedFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a persisted approval envelope")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields = std::collections::BTreeMap::new();
                while let Some((key, value)) =
                    map.next_entry::<String, Box<serde_json::value::RawValue>>()?
                {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom(
                            "duplicate approval envelope field",
                        ));
                    }
                }
                Ok(PersistedFields(fields))
            }
        }
        deserializer.deserialize_map(Fields)
    }
}
/// Additive persisted envelope metadata is opaque. Authority remains the exact
/// current wire schema; nested unknown fields and unknown schema versions fail.
pub fn decode_persisted(body: &str) -> Result<Request> {
    let PersistedFields(fields) = serde_json::from_str(body)
        .map_err(|_| Error::Invalid("invalid persisted approval envelope"))?;
    let known = fields
        .into_iter()
        .filter(|(key, _)| REQUEST_FIELDS.contains(&key.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let known = serde_json::to_string(&known).map_err(|_| Error::Unavailable)?;
    let request: Request = serde_json::from_str(&known)
        .map_err(|_| Error::Invalid("invalid persisted approval authority"))?;
    request.validate().map_err(Error::Invalid)?;
    Ok(request)
}
pub fn merge_persisted(body: &str, next: &Request) -> Result<String> {
    let PersistedFields(mut fields) = serde_json::from_str(body)
        .map_err(|_| Error::Invalid("invalid persisted approval envelope"))?;
    let PersistedFields(updated) =
        serde_json::from_str(&serde_json::to_string(next).map_err(|_| Error::Unavailable)?)
            .map_err(|_| Error::Unavailable)?;
    fields.extend(updated);
    serde_json::to_string(&fields).map_err(|_| Error::Unavailable)
}
