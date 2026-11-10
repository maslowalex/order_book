use order_book_core::allocation::fifo::FifoMatcher;
use order_book_core::execution_report::ExecutionReport;
use order_book_core::instrument::InstrumentSpec;
use order_book_core::instrument::price::Price;
use order_book_core::instrument::qty::Qty;
use order_book_core::order::ExchangeId;
use order_book_core::order::{OrderValidationError, Side};
use order_book_core::order_input::OrderInput;
use order_book_core::order_input::OrderKind;
use order_book_core::order_type::TimeInForce;
use order_book_core::orderbook::OrderBook;
use order_book_core::orderbook::OrderBookError;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, sleep};

use std::collections::HashMap;

use tracing::{error, info, warn};

#[derive(Debug)]
enum SubmitError {
    Execution(OrderBookError),
    Validation(OrderValidationError),
    CommandChannelClosed,
    ReplyDropped,
}

#[derive(Clone)]
struct BookHandle {
    sender: mpsc::Sender<Command>,
}

impl BookHandle {
    pub async fn submit(&self, input: OrderInput) -> Result<ExecutionReport, SubmitError> {
        let (oneshot_sender, oneshot_receiver) = oneshot::channel();

        self.sender
            .send(Command::Submit {
                input,
                sender: oneshot_sender,
            })
            .await
            .map_err(|_| SubmitError::CommandChannelClosed)?;

        oneshot_receiver
            .await
            .map_err(|_| SubmitError::ReplyDropped)?
    }

    pub async fn cancel(&self, order_id: ExchangeId) -> Result<(), SubmitError> {
        let (oneshot_sender, oneshot_receiver) = oneshot::channel();
        let command = Command::Cancel {
            order_id,
            sender: oneshot_sender,
        };

        self.sender
            .send(command)
            .await
            .map_err(|_| SubmitError::CommandChannelClosed)?;

        oneshot_receiver
            .await
            .map_err(|_| SubmitError::ReplyDropped)?
            .map_err(SubmitError::Execution)
    }

    pub async fn depth(&self) -> Result<HashMap<Side, Vec<(Price, Qty)>>, SubmitError> {
        let (oneshot_sender, oneshot_receiver) = oneshot::channel();
        let command = Command::Depth {
            sender: oneshot_sender,
        };

        self.sender
            .send(command)
            .await
            .map_err(|_| SubmitError::CommandChannelClosed)?;

        oneshot_receiver
            .await
            .map_err(|_| SubmitError::ReplyDropped)
    }
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Execution(error) => write!(f, "submission failed: {error}"),
            Self::Validation(error) => write!(f, "validation failed: {error}"),
            Self::CommandChannelClosed => write!(f, "command channel closed"),
            Self::ReplyDropped => write!(f, "reply is dropped"),
        }
    }
}

enum Command {
    Submit {
        input: OrderInput,
        sender: oneshot::Sender<Result<ExecutionReport, SubmitError>>,
    },
    Cancel {
        order_id: ExchangeId,
        sender: oneshot::Sender<Result<(), OrderBookError>>,
    },
    Depth {
        sender: oneshot::Sender<HashMap<Side, Vec<(Price, Qty)>>>,
    },
}

fn generate_input(i: u64, client_id: Option<&str>) -> OrderInput {
    let client_id = client_id.unwrap_or("UNKNOWN");

    OrderInput {
        side: Side::Bid,
        quantity: 50,
        kind: OrderKind::Limit,
        client_id: client_id.to_owned(),
        tif: Some(TimeInForce::GTC),
        trigger: None,
        price: Some(1000 + i),
    }
}

async fn dispatch_orders(handle: BookHandle, worker_tag: &str, number_of_orders: u64) -> () {
    for i in 1..=number_of_orders {
        let input = generate_input(i, Some(worker_tag));
        let response = handle.submit(input).await;

        if i == 3 {
            match &response {
                Ok(execution_report) => {
                    let cancelation_result = handle.cancel(execution_report.order_id.clone()).await;

                    match cancelation_result {
                        Ok(()) => info!(
                            worker = worker_tag,
                            order_id = %execution_report.order_id.0,
                            "order cancelled"
                        ),
                        Err(error) => {
                            error!(request = i, error = %error, "order cancellation failed")
                        }
                    }

                    let second_cancelation_result =
                        handle.cancel(execution_report.order_id.clone()).await;

                    match second_cancelation_result {
                        Ok(()) => error!(
                            worker = worker_tag,
                            order_id = %execution_report.order_id.0,
                            "ALREADY CANCELLED ORDER WAS ABLE TO CANCEL"
                        ),
                        Err(SubmitError::Execution(OrderBookError::OrderNotFound)) => {
                            warn!(request = i, "You can't cancel already cancelled order")
                        }
                        Err(unexpected) => error!(
                            worker = worker_tag,
                            order_id = %execution_report.order_id.0,
                            error = %unexpected,
                            "Unexpected error during double cancellation"
                        ),
                    }
                }
                Err(error) => error!(request = i, error = %error, "order cancellation failed"),
            }
        }

        match response {
            Ok(report) => info!(
                request = i,
                worker = worker_tag,
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
                Command::Cancel { order_id, sender } => {
                    let result = orderbook.cancel_order(order_id);

                    let _ = sender.send(result);
                }
                Command::Depth { sender } => {
                    let result = orderbook.full_depth();

                    let _ = sender.send(result);
                }
            }
        }

        orderbook
    });

    let handle = BookHandle { sender: tx };

    let sender_a = tokio::spawn(dispatch_orders(handle.clone(), "A", 5));
    let sender_b = tokio::spawn(dispatch_orders(handle.clone(), "B", 5));

    let checker_handle = handle.clone();
    let n = 8;

    tokio::spawn(async move {
        for attempt in 0..n {
            tokio::select! {
                result = checker_handle.depth() => {
                    match result {
                        Ok(depth) => info!(attempt, ?depth, "book depth"),
                        Err(error) => warn!(attempt, %error, "depth query failed"),
                    }
                }
                _ = sleep(Duration::from_millis(10)) => {
                    warn!(attempt, "depth query timed out");
                }
            }
        }
    });

    drop(handle);

    sender_a.await.expect("SENDER A FAILED");
    sender_b.await.expect("SENDER B FAILED");

    let orderbook = task.await.expect("orderbook doesn't materialized");

    info!(
        best_bid_minor = ?orderbook.best_bid().map(|price| price.minor()),
        best_ask_minor = ?orderbook.best_ask().map(|price| price.minor()),
        "book owner stopped"
    );
}
