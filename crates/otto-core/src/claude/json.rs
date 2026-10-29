//! Duplicate-key rejecting JSON decoding at the wire boundary.
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::HashSet;

struct Unique;
impl<'de> DeserializeSeed<'de> for Unique {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Unique {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(A::Error::custom("duplicate JSON key"));
            }
            map.next_value_seed(Unique)?;
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(Unique)?.is_some() {}
        Ok(())
    }
    fn visit_bool<E: Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: Error>(self) -> Result<(), E> {
        Ok(())
    }
}
pub(super) fn parse(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    Unique.deserialize(&mut decoder)?;
    decoder.end()?;
    serde_json::from_slice(bytes)
}
