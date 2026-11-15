//! Layer-2 semantic evaluation of memory proposals (#591 section 10): a
//! labeled offline dataset and a deterministic scorer over the raw proposal a
//! generator produced, independent of the layer-1 checks.

mod labels;
mod metrics;
mod scoring;

pub use labels::{
    DATASET_VERSION, Label, LabeledItem, contains_term, parse_labeled_items, retires,
};
#[cfg(test)]
pub(crate) use labels::{
    LABELED_DATASET_PATH, LabeledEvidence, LabeledRecord, requires_replacement,
};
#[cfg(test)]
pub(crate) use metrics::CategoryMetrics;
pub use metrics::Metrics;
pub use scoring::{Abstention, Generated, LayerOne, Score, score};

#[cfg(test)]
mod tests;
