//! I/O-free Claude Messages wire codec. HTTP, authentication and retries belong
//! to the native adapter. One assembler owns one response; no global state is used.
pub mod overflow;
pub mod protocol;
pub mod stream;

mod json;
#[cfg(test)]
mod tests;
