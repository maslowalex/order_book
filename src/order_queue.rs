use std::collections::HashMap;

use crate::allocation::Maker;
use crate::arena::{Arena, Handle};
use crate::types::{ClientId, ExchangeId, Order};

/// Stable, generational identity for a resting order inside one level.
pub(crate) type OrderKey = Handle;

#[derive(Debug, Clone)]
struct Node {
    order: Order,
    previous: Option<OrderKey>,
    next: Option<OrderKey>,
}

/// A price-time queue backed by a safe generational arena.
///
/// `Arena` owns the nodes and rejects stale handles. FIFO topology lives in the
/// nodes themselves, while `by_id` supplies constant-time arbitrary removal.
#[derive(Debug, Clone, Default)]
pub(crate) struct OrderQueue {
    nodes: Arena<Node>,
    by_id: HashMap<ExchangeId, OrderKey>,
    head: Option<OrderKey>,
    tail: Option<OrderKey>,
}

impl PartialEq for OrderQueue {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl OrderQueue {
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn push_back(&mut self, order: Order) -> Result<OrderKey, Order> {
        if self.by_id.contains_key(&order.exchange_id) {
            return Err(order);
        }

        let previous = self.tail;
        let exchange_id = order.exchange_id.clone();
        let key = self.nodes.insert(Node {
            order,
            previous,
            next: None,
        });

        if let Some(previous) = previous {
            self.nodes
                .get_mut(previous)
                .expect("tail key must name a live node")
                .next = Some(key);
        } else {
            self.head = Some(key);
        }

        self.tail = Some(key);
        self.by_id.insert(exchange_id, key);
        debug_assert!(self.links_are_consistent());
        Ok(key)
    }

    pub(crate) fn get(&self, id: &ExchangeId) -> Option<&Order> {
        let key = *self.by_id.get(id)?;
        self.nodes.get(key).map(|node| &node.order)
    }

    pub(crate) fn get_by_key_mut(&mut self, key: OrderKey) -> Option<&mut Order> {
        self.nodes.get_mut(key).map(|node| &mut node.order)
    }

    pub(crate) fn remove(&mut self, id: &ExchangeId) -> Option<Order> {
        let key = *self.by_id.get(id)?;
        self.remove_key(key)
    }

    pub(crate) fn remove_key(&mut self, key: OrderKey) -> Option<Order> {
        // Remove from the arena first: it performs its fallible free-list
        // reservation before changing anything. Once this succeeds, repairing
        // links and deleting the hash entry are allocation-free.
        let node = self.nodes.remove(key)?;
        let previous = node.previous;
        let next = node.next;

        if let Some(previous) = previous {
            self.nodes
                .get_mut(previous)
                .expect("previous link must name a live node")
                .next = next;
        } else {
            self.head = next;
        }

        if let Some(next) = next {
            self.nodes
                .get_mut(next)
                .expect("next link must name a live node")
                .previous = previous;
        } else {
            self.tail = previous;
        }

        let indexed = self.by_id.remove(&node.order.exchange_id);
        debug_assert_eq!(indexed, Some(key));
        debug_assert_eq!(self.head.is_none(), self.tail.is_none());
        debug_assert!(self.links_are_consistent());
        Some(node.order)
    }

    pub(crate) fn remove_where(&mut self, mut predicate: impl FnMut(&Order) -> bool) -> Vec<Order> {
        let mut removed = Vec::new();
        let mut cursor = self.head;

        while let Some(key) = cursor {
            let node = self
                .nodes
                .get(key)
                .expect("queue link must name a live node");
            cursor = node.next;
            if predicate(&node.order) {
                removed.push(
                    self.remove_key(key)
                        .expect("key observed during exclusive traversal must remain live"),
                );
            }
        }

        removed
    }

    pub(crate) fn remove_client(&mut self, client: &ClientId) -> Vec<Order> {
        self.remove_where(|order| &order.client_id == client)
    }

    pub(crate) fn collect_makers(&self, makers: &mut Vec<Maker>, keys: &mut Vec<OrderKey>) {
        makers.clear();
        keys.clear();
        for (key, order) in self.iter_with_keys() {
            makers.push(Maker {
                remaining_quantity: order.remaining_quantity,
                arrival: u128::from(order.arrival),
            });
            keys.push(key);
        }
    }

    pub(crate) fn iter(&self) -> Iter<'_> {
        Iter {
            nodes: &self.nodes,
            next: self.head,
        }
    }

    fn iter_with_keys(&self) -> IterWithKeys<'_> {
        IterWithKeys {
            nodes: &self.nodes,
            next: self.head,
        }
    }

    #[cfg(test)]
    pub(crate) fn at(&self, position: usize) -> Option<&Order> {
        self.iter().nth(position)
    }

    fn links_are_consistent(&self) -> bool {
        if self.head.is_none() || self.tail.is_none() {
            return self.head.is_none() && self.tail.is_none() && self.nodes.is_empty();
        }

        let mut previous = None;
        let mut cursor = self.head;
        let mut visited = 0usize;
        while let Some(key) = cursor {
            let Some(node) = self.nodes.get(key) else {
                return false;
            };
            if node.previous != previous {
                return false;
            }
            previous = Some(key);
            cursor = node.next;
            visited += 1;
            if visited > self.nodes.len() {
                return false;
            }
        }

        previous == self.tail && visited == self.nodes.len() && visited == self.by_id.len()
    }
}

pub(crate) struct Iter<'a> {
    nodes: &'a Arena<Node>,
    next: Option<OrderKey>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Order;

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.next?;
        let node = self
            .nodes
            .get(key)
            .expect("queue link must name a live node");
        self.next = node.next;
        Some(&node.order)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.nodes.len()))
    }
}

struct IterWithKeys<'a> {
    nodes: &'a Arena<Node>,
    next: Option<OrderKey>,
}

impl<'a> Iterator for IterWithKeys<'a> {
    type Item = (OrderKey, &'a Order);

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.next?;
        let node = self
            .nodes
            .get(key)
            .expect("queue link must name a live node");
        self.next = node.next;
        Some((key, &node.order))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::order;
    use crate::types::Side;

    fn queued(id: &str) -> Order {
        order(Side::Bid, 100, 1, None, id)
    }

    fn ids(queue: &OrderQueue) -> Vec<String> {
        queue
            .iter()
            .map(|order| order.exchange_id.0.clone())
            .collect()
    }

    #[test]
    fn arbitrary_removal_repairs_both_links() {
        let mut queue = OrderQueue::default();
        for id in ["a", "b", "c", "d"] {
            queue.push_back(queued(id)).unwrap();
        }

        assert_eq!(
            queue.remove(&ExchangeId("c".into())).unwrap().exchange_id.0,
            "c"
        );
        assert_eq!(ids(&queue), ["a", "b", "d"]);
        assert!(queue.links_are_consistent());
    }

    #[test]
    fn head_tail_and_last_removal_are_consistent() {
        let mut queue = OrderQueue::default();
        for id in ["a", "b", "c"] {
            queue.push_back(queued(id)).unwrap();
        }

        queue.remove(&ExchangeId("a".into())).unwrap();
        queue.remove(&ExchangeId("c".into())).unwrap();
        assert_eq!(ids(&queue), ["b"]);
        queue.remove(&ExchangeId("b".into())).unwrap();
        assert!(queue.is_empty());
        assert!(queue.links_are_consistent());
    }

    #[test]
    fn removed_slot_reuse_does_not_revive_old_key() {
        let mut queue = OrderQueue::default();
        let stale = queue.push_back(queued("a")).unwrap();
        queue.remove_key(stale).unwrap();
        let current = queue.push_back(queued("b")).unwrap();

        assert_ne!(stale, current);
        assert!(queue.nodes.get(stale).is_none());
        assert_eq!(queue.nodes.get(current).unwrap().order.exchange_id.0, "b");
    }

    #[test]
    fn duplicate_id_is_rejected_without_mutation() {
        let mut queue = OrderQueue::default();
        queue.push_back(queued("a")).unwrap();
        let duplicate = queue.push_back(queued("a")).unwrap_err();

        assert_eq!(duplicate.exchange_id.0, "a");
        assert_eq!(ids(&queue), ["a"]);
        assert!(queue.links_are_consistent());
    }
}
