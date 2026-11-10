mod helpers;

use order_book_core::allocation::fifo::FifoMatcher;
use order_book_core::execution_report::ExecutionReport;
use order_book_core::instrument::InstrumentSpec;
use order_book_core::order::{OrderValidationError, Side};
use order_book_core::order_input::OrderInput;
use order_book_core::order_input::OrderKind;
use order_book_core::order_type::TimeInForce;
use order_book_core::orderbook::OrderBook;
use order_book_core::orderbook::OrderBookError;
use tokio::sync::{mpsc, oneshot};

use tracing::{error, info};

#[derive(Debug)]
enum SubmitError {
    Execution(OrderBookError),
    Validation(OrderValidationError),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Execution(error) => write!(f, "submission failed: {error}"),
            Self::Validation(error) => write!(f, "validation failed: {error}"),
        }
    }
}

enum Command {
    Submit {
        input: OrderInput,
        sender: oneshot::Sender<Result<ExecutionReport, SubmitError>>,
    },
}

fn generate_input(i: u64) -> OrderInput {
    OrderInput {
        side: Side::Bid,
        quantity: 50,
        kind: OrderKind::Limit,
        client_id: "test".to_owned(),
        tif: Some(TimeInForce::GTC),
        trigger: None,
        price: Some(1000 + i),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let instrument_spec = InstrumentSpec::cents();
    let (tx, mut rx) = mpsc::channel::<Command>(1);

    let task = tokio::spawn(async move {
        let mut orderbook = OrderBook::new(instrument_spec, FifoMatcher);

        while let Some(command) = rx.recv().await {
            match command {
                Command::Submit { input, sender } => {
                    let result = input
                        .into_order(&instrument_spec)
                        .map_err(SubmitError::Validation)
                        .and_then(|order| orderbook.submit(order).map_err(SubmitError::Execution));

                    let _ = sender.send(result);
                }
            }
        }

        orderbook
    });

    for i in 1..10_000 {
        let input = generate_input(i);
        let (oneshot_sender, oneshot_receiver) = oneshot::channel();

        tx.send(Command::Submit {
            input,
            sender: oneshot_sender,
        })
        .await
        .expect("book owner stopped receiving commands");

        let response = oneshot_receiver
            .await
            .expect("book owner dropped the reply");

        match response {
            Ok(report) => info!(
                request = i,
                order_id = %report.order_id.0,
                outcome = ?report.outcome,
                trades = report.trades.len(),
                cancelled = report.cancelled.len(),
                triggered = report.triggered.len(),
                "order processed"
            ),
            Err(error) => error!(request = i, error = %error, "order submission failed"),
        }
    }

    drop(tx);

    let orderbook = task.await.expect("orderbook doesn't materialized");

    info!(
        best_bid_minor = ?orderbook.best_bid().map(|price| price.minor()),
        best_ask_minor = ?orderbook.best_ask().map(|price| price.minor()),
        "book owner stopped"
    );
}
