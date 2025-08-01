//! Histogram Data example
//!
//! # Usage
//!
//! ```bash
//! cargo run --features sync --example histogram_data
//! ```

use ibapi::contracts::Contract;
use ibapi::market_data::historical::BarSize;
use ibapi::Client;

fn main() {
    env_logger::init();

    let client = Client::connect("127.0.0.1:4002", 100).expect("connection failed");

    let contract = Contract::stock("GM");

    let histogram = client.histogram_data(&contract, true, BarSize::Week).expect("histogram request failed");

    for item in &histogram {
        println!("{item:?}");
    }
}
