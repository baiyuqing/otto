//! Built tree nodes and the input each node needs.

use std::collections::HashMap;

use super::NODE_MAXIMUM_BYTES;
use super::log_messages::{LogKind, LogMessage};
use super::node_address::NodeAddress;

/// The model call needed for a leaf whose text does not fit in a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafTask {
    /// The leaf to write.
    pub address: NodeAddress,
    /// Kind of the message.
    pub kind: LogKind,
    /// Full message text.
    pub text: String,
}

/// The model call needed for a merge whose joined children do not fit in a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTask {
    /// The parent to write.
    pub address: NodeAddress,
    /// Text of the first child.
    pub first: String,
    /// Text of the second child.
    pub second: String,
}

/// What building one node takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeInput {
    /// The node text is known without a model call.
    Ready(String),
    /// A model call compresses one message.
    Leaf(LeafTask),
    /// A model call merges two child texts.
    Merge(MergeTask),
}

/// Built node texts keyed by address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryTree {
    nodes: HashMap<NodeAddress, String>,
}

impl SummaryTree {
    /// An empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a built node. Errors on an invalid address or a node built before.
    pub fn insert(&mut self, address: NodeAddress, text: String) -> Result<(), String> {
        if !address.is_valid() {
            return Err(format!("invalid node address {address}"));
        }
        if self.nodes.contains_key(&address) {
            return Err(format!("node {address} is already built"));
        }
        self.nodes.insert(address, text);
        Ok(())
    }

    /// The text of a built node.
    pub fn get(&self, address: NodeAddress) -> Option<&str> {
        self.nodes.get(&address).map(String::as_str)
    }

    /// Whether the node is built.
    pub fn contains(&self, address: NodeAddress) -> bool {
        self.nodes.contains_key(&address)
    }

    /// Number of built nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no node is built.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every node that exists for a log of `log_length` messages: all aligned ranges
    /// inside `0..log_length`, leaves first and then by level, each level in id order.
    pub fn addresses_for_log(log_length: u64) -> Vec<NodeAddress> {
        let mut addresses = Vec::new();
        let mut n = 1u64;
        while n <= log_length {
            let mut id = 0;
            while id + n <= log_length {
                addresses.push(NodeAddress { id, n });
                id += n;
            }
            n *= 2;
        }
        addresses
    }

    /// The input to build `address`: a leaf needs the log; a merge needs both children built.
    pub fn node_input(
        &self,
        log: &[LogMessage],
        address: NodeAddress,
    ) -> Result<NodeInput, String> {
        if !address.is_valid() {
            return Err(format!("invalid node address {address}"));
        }
        if address.end_id() > log.len() as u64 {
            return Err(format!(
                "node {address} is outside the log of {} messages",
                log.len()
            ));
        }
        let Some((first, second)) = address.children() else {
            let message = &log[address.id as usize];
            let rendered = format!("{}: {}", message.kind.name(), message.text);
            return Ok(if rendered.len() <= NODE_MAXIMUM_BYTES {
                NodeInput::Ready(rendered)
            } else {
                NodeInput::Leaf(LeafTask {
                    address,
                    kind: message.kind,
                    text: message.text.clone(),
                })
            });
        };
        let (Some(first_text), Some(second_text)) = (self.get(first), self.get(second)) else {
            return Err(format!("children of node {address} are not both built"));
        };
        let joined = format!("{first_text}\n{second_text}");
        Ok(if joined.len() <= NODE_MAXIMUM_BYTES {
            NodeInput::Ready(joined)
        } else {
            NodeInput::Merge(MergeTask {
                address,
                first: first_text.to_owned(),
                second: second_text.to_owned(),
            })
        })
    }
}

/// One rendered view line: `id+n|text` with newlines in the text turned into spaces,
/// followed by a newline.
pub fn render_line(address: NodeAddress, text: &str) -> String {
    format!("{address}|{}\n", text.replace('\n', " "))
}
