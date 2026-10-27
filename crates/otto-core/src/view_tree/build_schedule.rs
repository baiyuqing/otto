//! Order in which tree nodes are built, without scanning the tree.

use std::collections::{HashSet, VecDeque};

use super::node_address::NodeAddress;
use super::summary_tree::SummaryTree;

/// Queue-based build order for a log of fixed length.
///
/// A leaf becomes ready when fewer than `window` earlier leaves are unbuilt. A merge
/// becomes ready when both children are built. Nodes completed before they were handed
/// out (a rerun that reuses built nodes) count as built and are never handed out.
#[derive(Debug)]
pub struct BuildSchedule {
    log_length: u64,
    window: usize,
    next_leaf: u64,
    unbuilt_released_leaves: usize,
    done: HashSet<NodeAddress>,
    queued: HashSet<NodeAddress>,
    ready: VecDeque<NodeAddress>,
    total_nodes: usize,
}

impl BuildSchedule {
    /// A schedule for `log_length` leaves; `window` is at least 1.
    pub fn new(log_length: u64, window: usize) -> Self {
        let mut schedule = Self {
            log_length,
            window: window.max(1),
            next_leaf: 0,
            unbuilt_released_leaves: 0,
            done: HashSet::new(),
            queued: HashSet::new(),
            ready: VecDeque::new(),
            total_nodes: SummaryTree::addresses_for_log(log_length).len(),
        };
        schedule.release_leaves();
        schedule
    }

    /// Takes every node that is ready now, in the order they became ready.
    pub fn take_ready(&mut self) -> Vec<NodeAddress> {
        let mut taken = Vec::new();
        while let Some(address) = self.ready.pop_front() {
            if !self.done.contains(&address) {
                taken.push(address);
            }
        }
        taken
    }

    /// Records that `address` is built, which can make leaves and its parent ready.
    pub fn complete(&mut self, address: NodeAddress) -> Result<(), String> {
        if !address.is_valid() || address.end_id() > self.log_length {
            return Err(format!(
                "node {address} is not in a log of {} messages",
                self.log_length
            ));
        }
        if !self.done.insert(address) {
            return Err(format!("node {address} is already complete"));
        }
        if address.n == 1 {
            if address.id < self.next_leaf && self.queued.contains(&address) {
                self.unbuilt_released_leaves -= 1;
            }
            self.release_leaves();
        }
        let parent = address.parent();
        if parent.end_id() <= self.log_length
            && self.done.contains(&address.sibling())
            && !self.done.contains(&parent)
            && self.queued.insert(parent)
        {
            self.ready.push_back(parent);
        }
        Ok(())
    }

    /// Whether every node of the log is built.
    pub fn is_done(&self) -> bool {
        self.done.len() == self.total_nodes
    }

    fn release_leaves(&mut self) {
        while self.next_leaf < self.log_length {
            let leaf = NodeAddress::leaf(self.next_leaf);
            if self.done.contains(&leaf) {
                self.next_leaf += 1;
            } else if self.unbuilt_released_leaves < self.window {
                self.queued.insert(leaf);
                self.ready.push_back(leaf);
                self.unbuilt_released_leaves += 1;
                self.next_leaf += 1;
            } else {
                break;
            }
        }
    }
}
