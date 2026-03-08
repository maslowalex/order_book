use std::cmp::Reverse;
use std::collections::BTreeMap;

use crate::types::{Price, PriceLevel};

/*
If we were to store the quantities in plain u64 we might need to normalize/denormalize the quantity.
For example, 1 USD is 100 cents, and 1 BTC is 100_000_000 satoshis.
For OrderBook operations we store at the lowest fraction for precision,
and do the normalization on the higher levels.
*/
pub struct OrderBook {
    pub bids: BTreeMap<Reverse<Price>, PriceLevel>, // descending: best (highest) bid first
    pub asks: BTreeMap<Price, PriceLevel>,           // ascending: best (lowest) ask first
}

/*
Q: Why knowing the *spread* is important?

A: At any moment when a LIMIT order arrives, we must check if it crosses the spread.
   For example, best ask 100 and best bid 99 (spread = 1). A LIMIT buy at 101 crosses
   the spread because 101 >= best ask (100), so it executes immediately as a taker
   rather than resting in the book.
*/
