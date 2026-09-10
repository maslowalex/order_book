use slotmap::{SlotMap, new_key_type};

use crate::allocation::Maker;
use crate::types::{ClientId, Order};

new_key_type! {
    /// Identity of an active order in one book's arena, never exposed to callers.
    /// Slotmap versions reused slots; its generation can wrap after 2^31 reuses.
    pub(crate) struct OrderKey;
}

pub(crate) type OrderArena = SlotMap<OrderKey, Node>;

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub(crate) order: Order,
    previous: Option<OrderKey>,
    next: Option<OrderKey>,
}

/// FIFO topology for one level. The book owns the nodes and the ID index.
#[derive(Debug, Clone, Default)]
pub(crate) struct LevelQueue {
    head: Option<OrderKey>,
    tail: Option<OrderKey>,
    len: usize,
}

impl LevelQueue {
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn push_back(&mut self, nodes: &mut OrderArena, order: Order) -> OrderKey {
        let key = nodes.insert(Node {
            order,
            previous: self.tail,
            next: None,
        });
        if let Some(tail) = self.tail {
            nodes
                .get_mut(tail)
                .expect("tail must reference a live node")
                .next = Some(key);
        } else {
            self.head = Some(key);
        }
        self.tail = Some(key);
        self.len += 1;
        key
    }

    /// The caller must obtain `key` from this queue or its indexed location.
    /// A stale key is harmless; a live key from another queue is a caller bug.
    pub(crate) fn remove_key(&mut self, nodes: &mut OrderArena, key: OrderKey) -> Option<Order> {
        let node = nodes.remove(key)?;
        if let Some(previous) = node.previous {
            nodes
                .get_mut(previous)
                .expect("previous link must be live")
                .next = node.next;
        } else {
            self.head = node.next;
        }
        if let Some(next) = node.next {
            nodes
                .get_mut(next)
                .expect("next link must be live")
                .previous = node.previous;
        } else {
            self.tail = node.previous;
        }
        self.len -= 1;
        Some(node.order)
    }

    pub(crate) fn remove_client(
        &mut self,
        nodes: &mut OrderArena,
        client: &ClientId,
    ) -> Vec<Order> {
        let mut removed = Vec::new();
        let mut cursor = self.head;
        while let Some(key) = cursor {
            let node = nodes.get(key).expect("queue link must be live");
            cursor = node.next;
            if &node.order.client_id == client {
                removed.push(
                    self.remove_key(nodes, key)
                        .expect("observed key must be live"),
                );
            }
        }
        removed
    }

    pub(crate) fn collect_makers(
        &self,
        nodes: &OrderArena,
        makers: &mut Vec<Maker>,
        keys: &mut Vec<OrderKey>,
    ) {
        makers.clear();
        keys.clear();
        for (key, order) in self.iter_with_keys(nodes) {
            makers.push(Maker {
                remaining_quantity: order.remaining_quantity,
                arrival: u128::from(order.arrival),
            });
            keys.push(key);
        }
    }

    pub(crate) fn iter<'a>(
        &self,
        nodes: &'a OrderArena,
    ) -> impl ExactSizeIterator<Item = &'a Order> + use<'a> {
        self.iter_with_keys(nodes).map(|(_, order)| order)
    }

    pub(crate) fn iter_with_keys<'a>(&self, nodes: &'a OrderArena) -> IterWithKeys<'a> {
        IterWithKeys {
            nodes,
            next: self.head,
            remaining: self.len,
        }
    }

    #[cfg(test)]
    pub(crate) fn links_are_consistent(&self, nodes: &OrderArena) -> bool {
        if self.head.is_none() || self.tail.is_none() {
            return self.head.is_none() && self.tail.is_none() && self.len == 0;
        }
        let mut previous = None;
        let mut cursor = self.head;
        let mut visited = 0;
        while let Some(key) = cursor {
            let Some(node) = nodes.get(key) else {
                return false;
            };
            if node.previous != previous {
                return false;
            }
            previous = Some(key);
            cursor = node.next;
            visited += 1;
            if visited > self.len {
                return false;
            }
        }
        previous == self.tail && visited == self.len
    }
}

pub(crate) struct IterWithKeys<'a> {
    nodes: &'a OrderArena,
    next: Option<OrderKey>,
    remaining: usize,
}

impl<'a> Iterator for IterWithKeys<'a> {
    type Item = (OrderKey, &'a Order);
    fn next(&mut self) -> Option<Self::Item> {
        let key = self.next?;
        let node = self
            .nodes
            .get(key)
            .expect("queue link must reference a live node");
        self.next = node.next;
        self.remaining -= 1;
        Some((key, &node.order))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for IterWithKeys<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::order;
    use crate::types::Side;

    fn queued(id: &str) -> Order {
        order(Side::Bid, 100, 1, None, id)
    }
    fn ids(queue: &LevelQueue, nodes: &OrderArena) -> Vec<String> {
        queue
            .iter(nodes)
            .map(|order| order.exchange_id.0.clone())
            .collect()
    }

    #[test]
    fn queues_share_storage_without_sharing_links() {
        let mut nodes = OrderArena::with_key();
        let mut left = LevelQueue::default();
        let mut right = LevelQueue::default();
        let a = left.push_back(&mut nodes, queued("a"));
        let other = right.push_back(&mut nodes, queued("other"));
        let b = left.push_back(&mut nodes, queued("b"));
        let c = left.push_back(&mut nodes, queued("c"));
        let d = left.push_back(&mut nodes, queued("d"));
        assert!(left.links_are_consistent(&nodes));
        assert!(right.links_are_consistent(&nodes));
        assert_eq!(left.len(), 4);
        assert_eq!(right.len(), 1);
        for (key, expected) in [
            (b, vec!["a", "c", "d"]),
            (a, vec!["c", "d"]),
            (d, vec!["c"]),
            (c, vec![]),
        ] {
            assert!(left.remove_key(&mut nodes, key).is_some());
            assert_eq!(ids(&left, &nodes), expected);
            assert!(left.links_are_consistent(&nodes));
            assert!(right.links_are_consistent(&nodes));
            assert_eq!(ids(&right, &nodes), ["other"]);
        }
        assert_eq!(nodes.len(), 1);
        assert!(right.remove_key(&mut nodes, other).is_some());
        assert!(nodes.is_empty());
        assert!(right.links_are_consistent(&nodes));
    }

    #[test]
    fn reuse_in_another_queue_does_not_revive_stale_key() {
        let mut nodes = OrderArena::with_key();
        let mut left = LevelQueue::default();
        let mut right = LevelQueue::default();
        let stale = left.push_back(&mut nodes, queued("a"));
        left.remove_key(&mut nodes, stale).unwrap();
        let current = right.push_back(&mut nodes, queued("b"));
        assert_ne!(stale, current);
        assert!(left.remove_key(&mut nodes, stale).is_none());
        assert!(nodes.get(stale).is_none());
        assert_eq!(ids(&right, &nodes), ["b"]);
        assert!(left.links_are_consistent(&nodes));
        assert!(right.links_are_consistent(&nodes));
    }
}
