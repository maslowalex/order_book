mod helpers;

use order_book_core::allocation::fifo::FifoMatcher;
use order_book_core::instrument::InstrumentSpec;
use order_book_core::order::{Order, Side};
use order_book_core::orderbook::OrderBook;
use tokio::sync::mpsc;

struct LimitOrderMessage {
    side: Side,
    price: u64,
    quantity: u64,
}

impl LimitOrderMessage {
    fn new(side: Side, price: u64, quantity: u64) -> Self {
        LimitOrderMessage {
            side,
            price,
            quantity,
        }
    }

    fn to_order(&self, spec: &InstrumentSpec) -> Order {
        helpers::limit_order(&spec, self.side, self.price, self.quantity, "client_id")
            .expect("invalid_order")
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let instrument_spec = InstrumentSpec::cents();
    let (tx, mut rx) = mpsc::channel::<LimitOrderMessage>(1);

    let task = tokio::spawn(async move {
        let mut orderbook = OrderBook::new(instrument_spec, FifoMatcher);

        while let Some(limit_order_message) = rx.recv().await {
            orderbook
                .submit(limit_order_message.to_order(&instrument_spec))
                .expect("channel order is valid");
        }

        orderbook
    });

    for _ in 1..10_000 {
        let limit_order_message = LimitOrderMessage::new(Side::Ask, 10_000, 5);

        let _ = tx.send(limit_order_message).await;
    }

    drop(tx);

    let orderbook = task.await.expect("orderbook doesn't materialized");

    dbg!(orderbook.best_ask());
}
