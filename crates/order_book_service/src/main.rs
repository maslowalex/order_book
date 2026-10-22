mod helpers;

use order_book_core::{
    allocation::FifoMatcher, instrument::InstrumentSpec, orderbook::OrderBook, types::Side,
};

use tokio::task::yield_now;
// use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let instrument_spec = InstrumentSpec::cents();
    let mut order_book_a = OrderBook::new(instrument_spec, FifoMatcher);
    let mut order_book_b = OrderBook::new(instrument_spec, FifoMatcher);

    let a = tokio::spawn(async move {
        submit_burst(&mut order_book_a, "A").await;
        order_book_a
    });

    let b = tokio::spawn(async move {
        submit_burst(&mut order_book_b, "B").await;
        order_book_b
    });

    dbg!(a.await.unwrap().best_bid());
    dbg!(b.await.unwrap().best_bid());
}

async fn submit_burst<'a>(order_book: &'a mut OrderBook<FifoMatcher>, label: &'a str) -> () {
    println!("burst {} started!", label);
    for _ in 1..10_000 {
        let order = helpers::limit_order(&order_book.spec(), Side::Bid, 10_000, 5, "buyer");
        order_book.submit(order).expect("workshop order is valid");
        yield_now().await
    }
    println!("burst {} finished!", label);
}
