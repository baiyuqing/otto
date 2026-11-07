//! Claude's cumulative, disjoint input counters and cache-write lifetimes.

use super::stream::StreamError;
use crate::model::Usage;
use serde_json::Value;

#[derive(Debug, Default)]
pub(super) struct CacheUsage {
    input: i64,
    output: i64,
    read: Option<i64>,
    written: Option<i64>,
    five_minute: Option<i64>,
    one_hour: Option<i64>,
}

fn count(value: &Value, key: &str) -> Result<Option<i64>, StreamError> {
    value
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_i64()
                .filter(|count| *count >= 0)
                .ok_or(StreamError::Decode)
        })
        .transpose()
}

fn update(target: &mut Option<i64>, value: Option<i64>) {
    if value.is_some() {
        *target = value;
    }
}

impl CacheUsage {
    pub(super) fn update(&mut self, value: &Value) -> Result<(), StreamError> {
        if !value.is_object() {
            return Err(StreamError::Decode);
        }
        if let Some(input) = count(value, "input_tokens")? {
            self.input = input;
        }
        if let Some(output) = count(value, "output_tokens")? {
            self.output = output;
        }
        update(&mut self.read, count(value, "cache_read_input_tokens")?);
        update(
            &mut self.written,
            count(value, "cache_creation_input_tokens")?,
        );
        if let Some(creation) = value.get("cache_creation").filter(|value| !value.is_null()) {
            if !creation.is_object() {
                return Err(StreamError::Decode);
            }
            update(
                &mut self.five_minute,
                count(creation, "ephemeral_5m_input_tokens")?,
            );
            update(
                &mut self.one_hour,
                count(creation, "ephemeral_1h_input_tokens")?,
            );
        }
        Ok(())
    }

    pub(super) fn read_present(&self) -> bool {
        self.read.is_some()
    }

    pub(super) fn normalized(&self) -> Result<Usage, StreamError> {
        let ttl_total = self
            .five_minute
            .zip(self.one_hour)
            .map(|(five_minute, one_hour)| {
                five_minute.checked_add(one_hour).ok_or(StreamError::Decode)
            })
            .transpose()?;
        if self
            .written
            .zip(ttl_total)
            .is_some_and(|(written, ttl)| written != ttl)
        {
            return Err(StreamError::Decode);
        }
        let written = self.written.or(ttl_total);
        if self.five_minute.is_some() && written.is_none() {
            return Err(StreamError::Decode);
        }
        let read = self.read.unwrap_or(0);
        let input = self
            .input
            .checked_add(read)
            .and_then(|input| input.checked_add(written.unwrap_or(0)))
            .ok_or(StreamError::Decode)?;
        let usage = Usage {
            input_tokens: input,
            output_tokens: self.output,
            cached_input_tokens: read,
            cache_write_input_tokens: written,
            cache_write_1h_input_tokens: self.one_hour,
        };
        usage.validate().map_err(|_| StreamError::Decode)?;
        if self
            .five_minute
            .zip(written)
            .is_some_and(|(five_minute, written)| five_minute > written)
        {
            return Err(StreamError::Decode);
        }
        Ok(usage)
    }
}
