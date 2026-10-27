//! Address `id+n` of a tree node.

use std::fmt;

/// The node that summarizes the `n` log messages starting at message `id`.
///
/// `n` is a power of two and `id` is a multiple of `n`, so the nodes form a binary tree:
/// the node `id+n` has the children `id+(n/2)` and `(id+(n/2))+(n/2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeAddress {
    /// Id of the first message in the range.
    pub id: u64,
    /// Number of messages in the range.
    pub n: u64,
}

impl NodeAddress {
    /// Returns the address, or the reason `id+n` is not a node address.
    pub fn new(id: u64, n: u64) -> Result<Self, String> {
        if n == 0 || !n.is_power_of_two() {
            return Err(format!("n must be a power of two, at least 1; got {n}"));
        }
        if !id.is_multiple_of(n) {
            return Err(format!("id must be a multiple of n; got {id}+{n}"));
        }
        Ok(Self { id, n })
    }

    /// The address of one message.
    pub fn leaf(id: u64) -> Self {
        Self { id, n: 1 }
    }

    /// Whether `n` is a power of two and `id` a multiple of `n`.
    pub fn is_valid(&self) -> bool {
        self.n != 0 && self.n.is_power_of_two() && self.id.is_multiple_of(self.n)
    }

    /// Height in the tree: 0 for a leaf.
    pub fn level(&self) -> u32 {
        self.n.trailing_zeros()
    }

    /// Id of the last message in the range.
    pub fn last_id(&self) -> u64 {
        self.id + self.n - 1
    }

    /// Id one past the last message in the range.
    pub fn end_id(&self) -> u64 {
        self.id + self.n
    }

    /// Whether this is the first of its two siblings.
    pub fn is_left(&self) -> bool {
        self.id.is_multiple_of(2 * self.n)
    }

    /// The node made from this node and its sibling.
    pub fn parent(&self) -> Self {
        Self {
            id: self.id - self.id % (2 * self.n),
            n: 2 * self.n,
        }
    }

    /// The other node with the same parent.
    pub fn sibling(&self) -> Self {
        let id = if self.is_left() {
            self.id + self.n
        } else {
            self.id - self.n
        };
        Self { id, n: self.n }
    }

    /// The two halves of the range; `None` for a leaf.
    pub fn children(&self) -> Option<(Self, Self)> {
        if self.n == 1 {
            return None;
        }
        let half = self.n / 2;
        Some((
            Self {
                id: self.id,
                n: half,
            },
            Self {
                id: self.id + half,
                n: half,
            },
        ))
    }

    /// Whether `other` is this node's sibling.
    pub fn is_sibling_of(&self, other: &Self) -> bool {
        self.n == other.n && self.id != other.id && self.parent() == other.parent()
    }
}

impl fmt::Display for NodeAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}+{}", self.id, self.n)
    }
}
