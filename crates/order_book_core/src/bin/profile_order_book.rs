//! A deterministic, steady-state CPU-profiling workload.
//!
//! This is deliberately not a benchmark. Criterion remains the source of
//! latency and throughput measurements; this binary gives sampling profilers a
//! long-lived hot loop without Criterion machinery, book cloning, or order
//! construction in the sampled path.

use std::env;
use std::hint::black_box;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use order_book_core::allocation::fifo::FifoMatcher;
use order_book_core::execution_report::SubmitOutcome;
use order_book_core::instrument::InstrumentSpec;
use order_book_core::instrument::price::Price;
use order_book_core::instrument::qty::Qty;
use order_book_core::order::{Order, Side};
use order_book_core::order_type::OrderType;
use order_book_core::orderbook::OrderBook;

const DEFAULT_DEPTH: usize = 10_000;
const DEFAULT_OPERATIONS: usize = 25_000;
const ASK_MINOR: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Config {
    depth: usize,
    operations: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            depth: DEFAULT_DEPTH,
            operations: DEFAULT_OPERATIONS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProfileSummary {
    operations: usize,
    trades: usize,
    final_order_count: usize,
    final_level_depth: usize,
    final_level_quantity: u64,
    checksum: u64,
    elapsed: Duration,
}

enum Command {
    Run(Config),
    Help,
}

#[hotpath::main]
fn main() -> ExitCode {
    let command = match parse_args(env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: {error}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };

    let Command::Run(config) = command else {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    };

    match run_workload(config) {
        Ok(summary) => {
            println!(
                "profile complete: operations={} trades={} elapsed={:?} \
                 final_orders={} final_level_depth={} final_level_quantity={} checksum={}",
                summary.operations,
                summary.trades,
                summary.elapsed,
                summary.final_order_count,
                summary.final_level_depth,
                summary.final_level_quantity,
                summary.checksum,
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("workload failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> &'static str {
    "Usage: profile_order_book [--depth N] [--operations N]\n\
     \n\
     CPU hypothesis probe for FIFO's full-level maker projection.\n\
     Defaults: --depth 10000 --operations 25000"
}

fn parse_args<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = String>,
{
    let mut config = Config::default();
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--depth" => config.depth = parse_positive("--depth", args.next())?,
            "--operations" => config.operations = parse_positive("--operations", args.next())?,
            unknown => return Err(format!("unknown argument `{unknown}`")),
        }
    }

    // `add_order` stamps one arrival per initial maker and replacement. Keep
    // bad CLI input from reaching the engine's deliberate u32 exhaustion panic.
    let rests = config
        .depth
        .checked_add(config.operations)
        .ok_or_else(|| "depth + operations overflows usize".to_owned())?;
    if rests >= u32::MAX as usize {
        return Err("depth + operations must be less than u32::MAX".to_owned());
    }

    Ok(Command::Run(config))
}

fn parse_positive(name: &str, value: Option<String>) -> Result<usize, String> {
    let raw = value.ok_or_else(|| format!("{name} requires a value"))?;
    let parsed = raw
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a positive integer, got `{raw}`"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(parsed)
}

fn limit_order(id: String, client: String, price: Price, quantity: Qty, timestamp: u128) -> Order {
    Order::builder()
        .exchange_id(id)
        .client_id(client)
        .order_type(OrderType::limit_gtc(price))
        .side(Side::Ask)
        .quantity(quantity)
        .timestamp(timestamp)
        .build()
        .expect("profiling order has all required fields")
}

fn market_bid(id: String, client: String, quantity: Qty, timestamp: u128) -> Order {
    Order::builder()
        .exchange_id(id)
        .client_id(client)
        .order_type(OrderType::Market)
        .side(Side::Bid)
        .quantity(quantity)
        .timestamp(timestamp)
        .build()
        .expect("profiling order has all required fields")
}

/// Keep one FIFO level at constant depth. Each operation projects the entire
/// level to fill one unit, then appends one replacement maker. All allocations
/// needed to construct the input orders happen before the timer starts.
#[hotpath::measure]
fn run_workload(config: Config) -> Result<ProfileSummary, String> {
    let spec = InstrumentSpec::cents();
    let price = spec
        .price_from_minor(ASK_MINOR)
        .expect("100.00 is on the one-cent grid");
    let unit = spec
        .qty_from_base(1)
        .expect("one unit is on the unit-lot grid");
    let mut book = OrderBook::new(spec, FifoMatcher);

    for index in 0..config.depth {
        book.add_order(limit_order(
            format!("seed-{index}"),
            format!("seed-client-{index}"),
            price,
            unit,
            index as u128,
        ))
        .map_err(|error| format!("could not seed maker {index}: {error:?}"))?;
    }

    let workload = (0..config.operations)
        .map(|index| {
            let timestamp = (config.depth + index) as u128;
            (
                market_bid(
                    format!("ignored-taker-{index}"),
                    format!("taker-client-{index}"),
                    unit,
                    timestamp,
                ),
                limit_order(
                    format!("replacement-{index}"),
                    format!("replacement-client-{index}"),
                    price,
                    unit,
                    timestamp,
                ),
            )
        })
        .collect::<Vec<_>>();

    let started = Instant::now();
    let mut trades = 0usize;
    let mut checksum = 0u64;

    for (operation, (taker, replacement)) in workload.into_iter().enumerate() {
        let report = book
            .submit(taker)
            .map_err(|error| format!("submit {operation} failed: {error:?}"))?;
        if report.outcome != SubmitOutcome::Filled || report.trades.len() != 1 {
            return Err(format!(
                "submit {operation} produced {:?} with {} trades",
                report.outcome,
                report.trades.len()
            ));
        }

        let trade = &report.trades[0];
        if trade.price != price || trade.quantity != unit {
            return Err(format!(
                "submit {operation} filled at {:?} for {:?}, expected {:?} for {:?}",
                trade.price, trade.quantity, price, unit
            ));
        }
        trades += report.trades.len();
        checksum = checksum
            .wrapping_add(trade.price.minor())
            .wrapping_add(trade.quantity.base())
            .wrapping_add(trade.maker_order_id.0.len() as u64);
        black_box(&report);

        book.add_order(replacement)
            .map_err(|error| format!("replacement {operation} failed: {error:?}"))?;
    }

    let elapsed = started.elapsed();
    let level = book
        .best_level(Side::Ask)
        .ok_or_else(|| "ask level disappeared".to_owned())?;
    let summary = ProfileSummary {
        operations: config.operations,
        trades,
        final_order_count: book.order_count(),
        final_level_depth: level.order_count(),
        final_level_quantity: level.total_quantity().base(),
        checksum,
        elapsed,
    };
    black_box(summary.checksum);

    if summary.trades != config.operations
        || summary.final_order_count != config.depth
        || summary.final_level_depth != config.depth
        || summary.final_level_quantity != config.depth as u64
    {
        return Err(format!("steady-state invariant failed: {summary:?}"));
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn defaults_are_stable() {
        let Command::Run(config) = parse_args(Vec::new()).unwrap() else {
            panic!("expected run command");
        };
        assert_eq!(config, Config::default());
    }

    #[test]
    fn accepts_explicit_workload_shape() {
        let Command::Run(config) =
            parse_args(strings(&["--depth", "7", "--operations", "11"])).unwrap()
        else {
            panic!("expected run command");
        };
        assert_eq!(
            config,
            Config {
                depth: 7,
                operations: 11,
            }
        );
    }

    #[test]
    fn rejects_missing_malformed_zero_and_unknown_values() {
        for args in [
            strings(&["--depth"]),
            strings(&["--depth", "many"]),
            strings(&["--depth", "0"]),
            strings(&["--operations", "0"]),
            strings(&["--unknown", "1"]),
        ] {
            assert!(parse_args(args).is_err());
        }
    }

    #[test]
    fn small_workload_preserves_steady_state() {
        let summary = run_workload(Config {
            depth: 4,
            operations: 8,
        })
        .unwrap();

        assert_eq!(summary.operations, 8);
        assert_eq!(summary.trades, 8);
        assert_eq!(summary.final_order_count, 4);
        assert_eq!(summary.final_level_depth, 4);
        assert_eq!(summary.final_level_quantity, 4);
        assert_ne!(summary.checksum, 0);
    }
}
