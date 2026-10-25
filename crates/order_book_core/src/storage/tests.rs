use super::*;
use crate::order_queue::OrderArena;
use crate::test_helpers::{order, px};

pub(super) fn bounded_spec() -> InstrumentSpec {
    InstrumentSpec::cents()
        .with_price_range(px(95), Some(px(105)))
        .unwrap()
}

fn check_store_contract<S: OrderBookStore>() {
    let mut store = S::try_new(bounded_spec()).unwrap();
    assert!(store.best_level(Side::Bid).is_none());
    assert!(store.best_level(Side::Ask).is_none());
    for side in [Side::Bid, Side::Ask] {
        assert!(store.best_level_mut(side).is_none());
        assert!(store.level(side, px(100)).is_none());
        assert!(store.level_mut(side, px(100)).is_none());
        assert!(store.remove_level(side, px(100)).is_none());
        assert_eq!(store.levels(side).count(), 0);
    }

    store.ensure_level(Side::Bid, px(98)).unwrap();
    store.ensure_level(Side::Bid, px(100)).unwrap();
    store.ensure_level(Side::Ask, px(102)).unwrap();
    store.ensure_level(Side::Ask, px(101)).unwrap();

    assert_eq!(store.best_price(Side::Bid), Some(px(100)));
    assert_eq!(store.best_price(Side::Ask), Some(px(101)));
    assert_eq!(
        store
            .levels(Side::Bid)
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![px(100), px(98)]
    );
    assert_eq!(
        store
            .levels(Side::Ask)
            .map(|level| level.price)
            .collect::<Vec<_>>(),
        vec![px(101), px(102)]
    );

    let mut nodes = OrderArena::with_key();
    for (side, price) in [(Side::Bid, 100), (Side::Ask, 101)] {
        let key = store
            .level_mut(side, px(price))
            .unwrap()
            .queue
            .push_back(&mut nodes, order(side, price, 1, None, "contract"));
        // Ensuring an existing level must retain its queue, not replace it.
        assert_eq!(
            store.ensure_level(side, px(price)).unwrap().order_count(),
            1
        );
        assert_eq!(store.level(side, px(price)).unwrap().side, side);
        let mut clone = store.clone();
        clone.remove_level(side, px(price)).unwrap();
        assert_eq!(store.level(side, px(price)).unwrap().order_count(), 1);
        store
            .best_level_mut(side)
            .unwrap()
            .queue
            .remove_key(&mut nodes, key)
            .unwrap();
        assert!(store.level(side, px(price)).unwrap().is_empty());
    }

    let removed = store.remove_level(Side::Bid, px(100));
    assert_eq!(removed.unwrap().price, px(100));
    assert_eq!(store.best_price(Side::Bid), Some(px(98)));
    assert!(store.level(Side::Bid, px(100)).is_none());

    store.remove_level(Side::Ask, px(101));
    assert_eq!(store.best_price(Side::Ask), Some(px(102)));
}

#[test]
fn btree_satisfies_the_store_contract() {
    check_store_contract::<BTreeStore>();
}

#[test]
fn hash_map_satisfies_the_store_contract() {
    check_store_contract::<HashMapStore>();
}

#[test]
fn tick_ladder_satisfies_the_store_contract() {
    check_store_contract::<TickLadderStore>();
}

#[test]
fn bbtree_satisfies_the_store_contract() {
    check_store_contract::<BBTreeStore>();
}
