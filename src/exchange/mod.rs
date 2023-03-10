//! exchange contains logic for spawning threads and connecting to remote exchanges via ws
//! Different exchange details can be implemented via the Exchange trait.
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::time::{sleep, Duration, Instant};
use tokio_tungstenite::tungstenite::Message::Pong;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};

use crate::app_config::ExchangeConfig;
use crate::exchange::binance::Binance;
use crate::exchange::bitstamp::Bitstamp;
// use crate::orderbook::Level;
use crate::result::Result;

mod binance;
mod bitstamp;

// We wait to avoid hammering the endpoint on retries. Contains the wait time before trying a connection. Should be in config...
const SLEEP_MS: u64 = 250;

#[derive(Debug, PartialEq)]
/// structure representing bids/asks received from an exchange.
pub struct OrderBookUpdate {
    // Timestamp of OrderBookUpdate creation for metrics. This is a little late but captures our code.
    pub(crate) ts: Instant,
    pub(crate) exchange: String,
    // Note: we use the Level struct which will duplicate the exchange in each level.
    // Could be optimized by using another struct but it's simpler like this for now.
    // pub(crate) bids: Vec<Level>,
    // pub(crate) asks: Vec<Level>,
}

#[async_trait]
/// trait representing the specific implementation details needed for a specific exchange
trait Exchange {
    fn parse_order_book_data(&self, bytes: Vec<u8>) -> Result<OrderBookUpdate>;

    fn exchange_config(&self) -> &ExchangeConfig;

    fn subscribe_msg(&self) -> String {
        let pair = self.exchange_config().spot_pair.to_lowercase();
        let msg = self
            .exchange_config()
            .subscription_message_template
            .replace("{{pair}}", &pair);
        info!("sub message {}", msg.clone());
        msg
    }

    fn validate_subscription_reply(&self, bytes: Vec<u8>) -> Result<()>;

    fn empty_order_book_data(&self) -> OrderBookUpdate {
        OrderBookUpdate {
            ts: Instant::now(),
            exchange: self.exchange_config().id.to_string(),
            // bids: vec![],
            // asks: vec![],
        }
    }
}

// TODOs:
// 1. [DONE] validate subscription reply
// 2. [DONE] If we don't get a message in x period of time, should probably close connection and re-establish. Takes too long for exchange...
// 3. Can test this w/ channels.

type WssStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// produces a thread to establish and manages connection/subscription to an exchange.
pub fn create_exchange_ws_connection(
    exchange_config: ExchangeConfig,
    subscribers_tx: mpsc::Sender<OrderBookUpdate>,
) {
    tokio::spawn(async move {
        // there are essentially two nested loops. If an error is encountered in the inner loop (handle_messages),
        // we can drop the connection, and let the connection be re-established.
        // Note that we "clear" the order book for the exchange if it disconnects as the data will become irrelevant quickly.

        // This is similar to the actor model in eg erlang/akka where we assume state can become corrupt,
        // and we just throw away the state on error and recreate it instead of writing any defensive code.
        // see the "LET IT CRASH" design philosophy https://wiki.c2.com/?LetItCrash

        // we need to assume that we don't have order book information for the exchange
        // anymore if failures are encountered.
        // Eg if the exchange endpoint goes down, we want to signal that there are no bids/asks for
        // the exchange available until we re-establish stability.
        loop {
            info!(
                "starting exchange ws order book collection for: [{:?}]",
                exchange_config.clone()
            );
            let exchange = Arc::new(build_exchange_from_config(&exchange_config).unwrap()); // will panic the app if can't build from config.

            match connect_and_subscribe(&exchange_config, exchange.clone()).await {
                Ok(ws_stream) => {
                    handle_messages(
                        exchange_config.clone(),
                        &subscribers_tx,
                        exchange.clone(),
                        ws_stream,
                    )
                    .await;
                }
                Err(e) => {
                    error!("Error connecting/subscribing... Will retry. {:?}", e);
                }
            };

            // Clear the order book if we shut the connection down.
            subscribers_tx
                .send(exchange.empty_order_book_data())
                .await
                .expect("unexpected error sending to channel. Panic!");

            debug!(
                "waiting {}ms before restarting connection to {}...",
                SLEEP_MS,
                exchange_config.id.clone()
            );
            sleep(Duration::from_millis(SLEEP_MS)).await; // wait 100ms to avoid hammering a failing endpoint.
        }
    });
}

/// connects to ws endpoint and subscribes to the pair
async fn connect_and_subscribe(
    exchange_config: &ExchangeConfig,
    exchange: Arc<Box<dyn Exchange + Sync + Send>>,
) -> Result<WssStream> {
    let (mut ws_stream, _) = connect_async(exchange_config.endpoint.clone())
        .await
        .map_err(|e| WsError::new(format!("error connecting to websocket: {:?}", e).into()))?;
    info!(
        "WebSocket handshake has been successfully completed for {}",
        exchange_config.id.as_str()
    );

    // subscribe
    ws_stream
        .send(Message::text(exchange.subscribe_msg()))
        .await
        .map_err(|e| WsError::new(format!("error subscribing via websocket: {:?}", e).into()))?;

    // Get the reply message. If anything not as expected, we just continue the loop w/ a delay.
    match ws_stream.next().await {
        None => {
            Err(Box::new(WsError::new(
                "error getting next subscription message...".into(),
            )))?;
        }
        Some(Ok(msg)) => {
            exchange.validate_subscription_reply(msg.into_data())?;
        }
        Some(e) => {
            e.map_err(|e| {
                WsError::new(
                    format!("something went wrong connecting/subscribing...: {:?}", e).into(),
                )
            })?;
        }
    }

    Ok(ws_stream)
}

/// handle messages will loop and stream messages received.
async fn handle_messages(
    exchange_config: ExchangeConfig,
    subscribers_tx: &Sender<OrderBookUpdate>,
    exchange: Arc<Box<dyn Exchange + Sync + Send>>,
    mut ws_stream: WssStream,
) {
    loop {
        // inner loop will process any input received.
        let exchange = exchange.clone();

        // It can take a long time to detect a failure, so we reset the connection if nothing is coming over the wire. https://github.com/snapview/tungstenite-rs/issues/225
        // this will cause connections to be terminated and exchange data to be dropped. Could try using ws ping frames instead to ensure it's alive.
        // eg bitstamp may not have an order book change in 1s and that be valid, but binance should send every n ms.
        // This is configurable per exchange to handle the different implementation details.
        match tokio::time::timeout(
            Duration::from_secs(exchange_config.receive_timeout_s),
            ws_stream.next(),
        )
        .await
        {
            // We explicitly handle ping frames and reply w/ a pong frame (binance will disconnect after 10m if not handled)
            Ok(Some(Ok(msg))) if msg.is_ping() => {
                info!(
                    "ping frame received for {}. Sending pong",
                    exchange_config.id.as_str()
                );
                if ws_stream.send(Pong(vec![])).await.is_err() {
                    error!(
                        "error sending ping reply to {}... Will retry disconnect/retry.",
                        exchange_config.id.as_str()
                    );
                    break;
                }
            }
            Ok(Some(Ok(msg))) => {
                match exchange.parse_order_book_data(msg.into_data().clone()) {
                    Ok(order_book_update) => {
                        // can possibly spawn this instead of awaiting, but need to ensure order.
                        subscribers_tx
                            .send(order_book_update)
                            .await
                            .expect("unexpected error sending to channel. Panic!");
                    }
                    Err(e) => {
                        error!("Restarting connection as we couldn't parse an orderbook update for {}!: {:?}", exchange_config.id.as_str(), e);
                        break;
                    }
                }
            }
            e => {
                error!("exchange connection error... Will restart. {:?}", e);
                break;
            }
        }
    }
}

/// builds the specific Exchange from the config details.
fn build_exchange_from_config(
    exchange_config: &ExchangeConfig,
) -> Result<Box<dyn Exchange + Sync + Send>> {
    match exchange_config.id.as_str() {
        binance::EXCHANGE_KEY => Ok(Box::new(Binance {
            exchange_config: exchange_config.clone(),
        })),
        bitstamp::EXCHANGE_KEY => Ok(Box::new(Bitstamp {
            exchange_config: exchange_config.clone(),
        })),
        // bybit::EXCHANGE_KEY => Ok(Box::new(Bybit {
        //     exchange_config: exchange_config.clone(),
        // })),
        id => Err(WsError::new(
            format!("error in configuration: unknown exchange id: {}", id).into(),
        ))?,
    }
}

#[derive(Debug)]
/// represents an error in the Ws connection/stream handling.
struct WsError {
    details: String,
}

impl WsError {
    pub fn new(details: String) -> WsError {
        WsError { details }
    }
}

impl Display for WsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Websocket error: {}", self.details)
    }
}

impl Error for WsError {
    fn description(&self) -> &str {
        &self.details
    }
}
