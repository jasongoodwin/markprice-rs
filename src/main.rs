#[macro_use]
extern crate log;

use tokio::sync::{mpsc, watch};
use tonic::transport::Server;

// use crate::orderbook::*;
// use crate::orderbook_aggregator::OrderbookSummaryPublisher;

mod app_config;
mod exchange;
mod metrics;
// mod orderbook_aggregator;
// mod orderbook_data;
mod result;


#[tokio::main]
async fn main() -> result::Result<()> {
    println!("Hello, world!");
    Ok(())
}
