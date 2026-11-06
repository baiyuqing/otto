//! Layer-2 semantic evaluation of memory proposals (#591 section 10): a
//! labeled offline dataset and a deterministic scorer over the raw proposal a
//! generator produced, independent of the layer-1 checks.

mod labels;
mod metrics;
mod scoring;

pub use labels::{
    DATASET_VERSION, LABELED_DATASET_PATH, Label, LabeledEvidence, LabeledItem, LabeledRecord,
    contains_term, parse_labeled_items, requires_evidence, requires_replacement, retires,
};
pub use metrics::{CategoryMetrics, Metrics};
pub use scoring::{Abstention, Generated, LayerOne, Score, score};

#[cfg(test)]
mod tests;
