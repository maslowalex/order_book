mod helpers;

use order_book_core::{
    allocation::FifoMatcher, instrument::InstrumentSpec, orderbook::OrderBook, types::Side,
};

use tokio::task;
// use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let instrument_spec = InstrumentSpec::cents();
    let order_book = OrderBook::new(instrument_spec, FifoMatcher);

    let task = task::spawn(async { println!("submitted!") });

    submit_burst(order_book).await;
    let _ = task.await;
}

async fn submit_burst(mut order_book: OrderBook<FifoMatcher>) -> () {
    println!("burst started!");
    for _ in 1..10_000 {
        let order = helpers::limit_order(&order_book.spec(), Side::Bid, 10_000, 5, "buyer");
        order_book.submit(order).expect("workshop order is valid");
    }
    println!("burst finished!");
}
