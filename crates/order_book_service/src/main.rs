mod helpers;

use order_book_core::{
    allocation::FifoMatcher, instrument::InstrumentSpec, orderbook::OrderBook, types::Side,
};

use tokio::task::yield_now;
// use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let instrument_spec = InstrumentSpec::cents();
    let order_book_a = OrderBook::new(instrument_spec, FifoMatcher);
    let order_book_b = OrderBook::new(instrument_spec, FifoMatcher);

    tokio::join!(
        submit_burst(order_book_a, "A"),
        submit_burst(order_book_b, "B")
    );
}

async fn submit_burst(mut order_book: OrderBook<FifoMatcher>, label: &str) -> () {
    println!("burst {} started!", label);
    for _ in 1..10_000 {
        let order = helpers::limit_order(&order_book.spec(), Side::Bid, 10_000, 5, "buyer");
        order_book.submit(order).expect("workshop order is valid");
        yield_now().await
    }
    println!("burst {} finished!", label);
}
