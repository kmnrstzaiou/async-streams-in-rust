use std::{
    fs::File,
    io::{BufWriter, Write},
    time::Duration,
};

use async_std::{prelude::*, stream};
use async_trait::async_trait;
use chrono::prelude::*;
use clap::Parser;
use xactor::{message, Actor, Broker, Context, Handler, Result, Service, Supervisor};
use yahoo_finance_api as yahoo;

#[derive(Parser, Debug)]
#[clap(
    version = "1.0",
    author = "Claus Matzinger",
    about = "A Manning LiveProject: async Rust"
)]
struct Opts {
    #[clap(short, long, default_value = "AAPL,MSFT,UBER,GOOG")]
    symbols: String,
    #[clap(short, long)]
    from: String,
}

///
/// A trait to provide a common interface for all signal calculations.
///
#[async_trait]
trait AsyncStockSignal {
    ///
    /// The signal's data type.
    ///
    type SignalType;

    ///
    /// Calculate the signal on the provided series.
    ///
    /// # Returns
    ///
    /// The signal (using the provided type) or `None` on error/invalid data.
    ///
    async fn calculate(&self, series: &[f64]) -> Option<Self::SignalType>;
}

///
/// Calculates the absolute and relative difference between the beginning and ending of an f64 series.
/// The relative difference is relative to the beginning.
///
struct PriceDifference {}

#[async_trait]
impl AsyncStockSignal for PriceDifference {
    ///
    /// A tuple `(absolute, relative)` to represent a price difference.
    ///
    type SignalType = (f64, f64);

    async fn calculate(&self, series: &[f64]) -> Option<Self::SignalType> {
        if !series.is_empty() {
            // unwrap is safe here even if first == last
            let (first, last) = (series.first().unwrap(), series.last().unwrap());
            let abs_diff = last - first;
            let first = if *first == 0.0 { 1.0 } else { *first };
            let rel_diff = abs_diff / first;
            Some((abs_diff, rel_diff))
        } else {
            None
        }
    }
}

///
/// Window function to create a simple moving average
///
struct WindowedSMA {
    pub window_size: usize,
}

#[async_trait]
impl AsyncStockSignal for WindowedSMA {
    type SignalType = Vec<f64>;

    async fn calculate(&self, series: &[f64]) -> Option<Self::SignalType> {
        if !series.is_empty() && self.window_size > 1 {
            Some(
                series
                    .windows(self.window_size)
                    .map(|w| w.iter().sum::<f64>() / w.len() as f64)
                    .collect(),
            )
        } else {
            None
        }
    }
}

///
/// Find the maximum in a series of f64
///
struct MaxPrice {}

#[async_trait]
impl AsyncStockSignal for MaxPrice {
    type SignalType = f64;

    async fn calculate(&self, series: &[f64]) -> Option<Self::SignalType> {
        if series.is_empty() {
            None
        } else {
            Some(series.iter().fold(f64::MIN, |acc, q| acc.max(*q)))
        }
    }
}

///
/// Find the maximum in a series of f64
///
struct MinPrice {}

#[async_trait]
impl AsyncStockSignal for MinPrice {
    type SignalType = f64;

    async fn calculate(&self, series: &[f64]) -> Option<Self::SignalType> {
        if series.is_empty() {
            None
        } else {
            Some(series.iter().fold(f64::MAX, |acc, q| acc.min(*q)))
        }
    }
}

#[message]
#[derive(Debug, Default, Clone)]
struct Quotes {
    symbol: String,
    quotes: Vec<yahoo::Quote>,
}

#[message]
#[derive(Debug, Clone)]
struct QuoteRequest {
    symbol: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

#[message]
#[derive(Debug, Clone)]
struct PerformanceIndicators {
    symbol: String,
    timestamp: DateTime<Utc>,
    price: f64,
    pct_change: f64,
    period_min: f64,
    period_max: f64,
    last_sma: f64,
}

struct StockDataDownloader;

#[async_trait]
impl Actor for StockDataDownloader {
    async fn started(&mut self, ctx: &mut Context<Self>) -> Result<()> {
        ctx.subscribe::<QuoteRequest>().await
    }
}

#[async_trait]
impl Handler<QuoteRequest> for StockDataDownloader {
    async fn handle(&mut self, _ctx: &mut Context<Self>, msg: QuoteRequest) {
        let symbol = msg.symbol;
        let start = yahoo::time::OffsetDateTime::from_unix_timestamp(msg.from.timestamp()).unwrap();
        let end = yahoo::time::OffsetDateTime::from_unix_timestamp(msg.to.timestamp()).unwrap();
        let provider = yahoo::YahooConnector::new();
        let data = match provider.get_quote_history(&symbol, start, end).await {
            Ok(response) => {
                if let Ok(quotes) = response.quotes() {
                    Quotes {
                        symbol: symbol.clone(),
                        quotes,
                    }
                } else {
                    Quotes {
                        symbol: symbol.clone(),
                        quotes: vec![],
                    }
                }
            }
            Err(e) => {
                eprintln!("Ignoring API error for symbol '{symbol}': {e}");
                Quotes {
                    symbol: symbol.clone(),
                    quotes: vec![],
                }
            }
        };
        if let Err(e) = Broker::from_registry().await.unwrap().publish(data) {
            eprintln!("{e}");
        }
    }
}

struct StockDataProcessor;

#[async_trait]
impl Actor for StockDataProcessor {
    async fn started(&mut self, ctx: &mut Context<Self>) -> Result<()> {
        ctx.subscribe::<Quotes>().await
    }
}

#[async_trait]
impl Handler<Quotes> for StockDataProcessor {
    async fn handle(&mut self, _ctx: &mut Context<Self>, mut msg: Quotes) {
        if !msg.quotes.is_empty() {
            msg.quotes.sort_by_cached_key(|k| k.timestamp);

            let last_date =
                DateTime::from_timestamp(msg.quotes.last().unwrap().timestamp as i64, 0).unwrap();
            let closes = msg.quotes.iter().map(|q| q.close).collect::<Vec<_>>();

            let diff = PriceDifference {};
            let min = MinPrice {};
            let max = MaxPrice {};
            let sma = WindowedSMA { window_size: 30 };

            let period_max: f64 = max.calculate(&closes).await.unwrap();
            let period_min: f64 = min.calculate(&closes).await.unwrap();

            let last_price = *closes.last().unwrap();
            let (_, pct_change) = diff.calculate(&closes).await.unwrap();
            let sma = sma.calculate(&closes).await.unwrap();

            let data = PerformanceIndicators {
                timestamp: last_date,
                symbol: msg.symbol.clone(),
                price: last_price,
                pct_change,
                period_min,
                period_max,
                last_sma: *sma.last().unwrap_or(&0.0),
            };

            if let Err(e) = Broker::from_registry().await.unwrap().publish(data) {
                eprintln!("{e}");
            }

            println!(
                "{},{},${:.2},{:.2}%,${:.2},${:.2},${:.2}",
                last_date.to_rfc3339(),
                msg.symbol,
                last_price,
                pct_change * 100.0,
                period_min,
                period_max,
                sma.last().unwrap_or(&0.0)
            )
        } else {
            println!("empty quotes");
        }
    }
}

#[derive(Debug, Default)]
struct FileSink {
    filename: String,
    writer: Option<BufWriter<File>>,
}

#[async_trait]
impl Actor for FileSink {
    async fn started(&mut self, ctx: &mut Context<Self>) -> Result<()> {
        let mut file = File::create(&self.filename).unwrap();
        let _ = writeln!(file, "period start,symbol,price,change %,min,max,30d avg");
        self.writer = Some(BufWriter::new(file));
        ctx.subscribe::<PerformanceIndicators>().await
    }

    async fn stopped(&mut self, ctx: &mut Context<Self>) {
        if let Some(writer) = &mut self.writer.take() {
            writer.flush().unwrap();
        }
        ctx.stop(None);
    }
}

#[async_trait]
impl Handler<PerformanceIndicators> for FileSink {
    async fn handle(&mut self, _ctx: &mut Context<Self>, msg: PerformanceIndicators) {
        if let Some(file) = &mut self.writer {
            let _ = writeln!(
                file,
                "{},{},${:.2},{:.2}%,${:.2},${:.2},${:.2}",
                msg.timestamp.to_rfc3339(),
                msg.symbol,
                msg.price,
                msg.pct_change * 100.0,
                msg.period_min,
                msg.period_max,
                msg.last_sma,
            );
            file.flush().unwrap();
        }
    }
}

#[xactor::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    let from: DateTime<Utc> = opts.from.parse().expect("Couldn't parse 'from' date");
    let to = Utc::now();

    let _downloader = Supervisor::start(|| StockDataDownloader).await;
    let _processor = Supervisor::start(|| StockDataProcessor).await;
    let _sink = Supervisor::start(|| FileSink {
        filename: "output.csv".to_string(),
        ..Default::default()
    })
    .await;

    let mut interval = stream::interval(Duration::from_secs(30));
    // a simple way to output a CSV header
    println!("period start,symbol,price,change %,min,max,30d avg");
    let symbols = opts.symbols.split(',').collect::<Vec<_>>();
    'outer: while interval.next().await.is_some() {
        for symbol in &symbols {
            if let Err(e) = Broker::from_registry().await?.publish(QuoteRequest {
                symbol: symbol.to_string(),
                from,
                to,
            }) {
                eprintln!("{e}");
                break 'outer;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(non_snake_case)]
    use super::*;

    #[async_std::test]
    async fn test_PriceDifference_calculate() {
        let signal = PriceDifference {};
        assert_eq!(signal.calculate(&[]).await, None);
        assert_eq!(signal.calculate(&[1.0]).await, Some((0.0, 0.0)));
        assert_eq!(signal.calculate(&[1.0, 0.0]).await, Some((-1.0, -1.0)));
        assert_eq!(
            signal
                .calculate(&[2.0, 3.0, 5.0, 6.0, 1.0, 2.0, 10.0])
                .await,
            Some((8.0, 4.0))
        );
        assert_eq!(
            signal.calculate(&[0.0, 3.0, 5.0, 6.0, 1.0, 2.0, 1.0]).await,
            Some((1.0, 1.0))
        );
    }

    #[async_std::test]
    async fn test_MinPrice_calculate() {
        let signal = MinPrice {};
        assert_eq!(signal.calculate(&[]).await, None);
        assert_eq!(signal.calculate(&[1.0]).await, Some(1.0));
        assert_eq!(signal.calculate(&[1.0, 0.0]).await, Some(0.0));
        assert_eq!(
            signal
                .calculate(&[2.0, 3.0, 5.0, 6.0, 1.0, 2.0, 10.0])
                .await,
            Some(1.0)
        );
        assert_eq!(
            signal.calculate(&[0.0, 3.0, 5.0, 6.0, 1.0, 2.0, 1.0]).await,
            Some(0.0)
        );
    }

    #[async_std::test]
    async fn test_MaxPrice_calculate() {
        let signal = MaxPrice {};
        assert_eq!(signal.calculate(&[]).await, None);
        assert_eq!(signal.calculate(&[1.0]).await, Some(1.0));
        assert_eq!(signal.calculate(&[1.0, 0.0]).await, Some(1.0));
        assert_eq!(
            signal
                .calculate(&[2.0, 3.0, 5.0, 6.0, 1.0, 2.0, 10.0])
                .await,
            Some(10.0)
        );
        assert_eq!(
            signal.calculate(&[0.0, 3.0, 5.0, 6.0, 1.0, 2.0, 1.0]).await,
            Some(6.0)
        );
    }

    #[async_std::test]
    async fn test_WindowedSMA_calculate() {
        let series = vec![2.0, 4.5, 5.3, 6.5, 4.7];

        let signal = WindowedSMA { window_size: 3 };
        assert_eq!(
            signal.calculate(&series).await,
            Some(vec![3.9333333333333336, 5.433333333333334, 5.5])
        );

        let signal = WindowedSMA { window_size: 5 };
        assert_eq!(signal.calculate(&series).await, Some(vec![4.6]));

        let signal = WindowedSMA { window_size: 10 };
        assert_eq!(signal.calculate(&series).await, Some(vec![]));
    }
}
