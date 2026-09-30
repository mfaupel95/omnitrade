//! OKX Spot connector (candles over WebSocket, orders over REST v5).
//!
//! `Network::Testnet` maps to OKX demo trading: same REST host with the
//! `x-simulated-trading: 1` header, and the `wspap` WebSocket host. Demo
//! trading needs API keys created in the OKX demo account; live keys are
//! rejected there.
//!
//! Keys are bound to a region: accounts in the EEA only work against
//! `eea.okx.com` (`with_eea(true)`); elsewhere OKX answers `50119 API key
//! doesn't exist`. EEA demo balances hold USD/USDC, not USDT (`BTCUSDC`).
//!
//! Differences to the Binance connector:
//! - OKX signs with a passphrase in addition to key and secret.
//! - Instruments are `BTC-USDT`; `BTCUSDT` is converted automatically.
//! - Only *closed* candles (`confirm == "1"`) are forwarded, so a strategy
//!   sees each bar once, as in a backtest.
//! - `subscribe_candles` works before or after `connect`; the socket is
//!   opened on subscription.

use crate::credentials::ApiCredentials;
use crate::error::ExchangeError;
use crate::trading::{Account, AccountBalance, ExchangeTrading, OrderRequest};
use crate::traits::{DepthUpdate, ExchangeInfo, ExchangeStream};
use async_trait::async_trait;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use omnitrade_core::{Candle, Order, OrderStatus, OrderType, Side, TimeInForce};
use reqwest::{Client, Method};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const REST_URL: &str = "https://www.okx.com";
const WS_BUSINESS_DEMO: &str = "wss://wspap.okx.com:8443/ws/v5/business";
const WS_BUSINESS_LIVE: &str = "wss://ws.okx.com:8443/ws/v5/business";
const REST_URL_EEA: &str = "https://eea.okx.com";
const WS_BUSINESS_DEMO_EEA: &str = "wss://wseeapap.okx.com:8443/ws/v5/business";
const WS_BUSINESS_LIVE_EEA: &str = "wss://wseea.okx.com:8443/ws/v5/business";
/// OKX drops idle sockets after 30 s; ping well before that.
const PING_EVERY: Duration = Duration::from_secs(20);
const QUOTES: [&str; 6] = ["USDT", "USDC", "EUR", "USD", "BTC", "ETH"];

/// `BTCUSDT` -> `BTC-USDT`; already dashed ids pass through unchanged.
pub fn inst_id(symbol: &str) -> String {
    let s = symbol.trim().to_uppercase();
    if s.contains('-') {
        return s;
    }
    for quote in QUOTES {
        if let Some(base) = s.strip_suffix(quote) {
            if !base.is_empty() {
                return format!("{base}-{quote}");
            }
        }
    }
    s
}

/// `1m` -> `candle1m`, `4h` -> `candle4H`, `1d` -> `candle1Dutc`.
pub fn candle_channel(interval: &str) -> Result<String, ExchangeError> {
    let (n, unit) = interval.split_at(interval.len().saturating_sub(1));
    if n.is_empty() || n.parse::<u32>().is_err() {
        return Err(ExchangeError::InvalidRequest(format!(
            "unsupported interval '{interval}'"
        )));
    }
    let bar = match unit {
        "m" => format!("{n}m"),
        "h" | "H" => format!("{n}H"),
        "d" | "D" => format!("{n}Dutc"),
        "w" | "W" => format!("{n}Wutc"),
        _ => {
            return Err(ExchangeError::InvalidRequest(format!(
                "unsupported interval '{interval}'"
            )))
        }
    };
    Ok(format!("candle{bar}"))
}

/// Milliseconds since the epoch as `2020-12-08T09:08:57.715Z`.
pub fn iso_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// Base64(HMAC-SHA256(secret, timestamp + method + path + body)).
pub fn sign(
    timestamp: &str,
    method: &str,
    path: &str,
    body: &str,
    secret: &str,
) -> Result<String, ExchangeError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| ExchangeError::InvalidCredentials(e.to_string()))?;
    mac.update(format!("{timestamp}{method}{path}{body}").as_bytes());
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn num(v: &Value) -> f64 {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_f64())
        .unwrap_or(0.0)
}

/// Closed candles from one WebSocket push; open (still forming) bars are skipped.
pub fn parse_candle_push(text: &str) -> Vec<Candle> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(rows) = v.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(Value::as_array)
        .filter(|r| r.len() >= 9 && r[8].as_str() == Some("1"))
        .filter_map(|r| {
            let ts = r[0].as_str()?.parse::<u64>().ok()?;
            Some(Candle::new(
                ts,
                num(&r[1]),
                num(&r[2]),
                num(&r[3]),
                num(&r[4]),
                num(&r[5]),
            ))
        })
        .collect()
}

/// Closed candles of a REST history response, oldest first (OKX sends newest first).
pub fn parse_history(text: &str) -> Vec<Candle> {
    let mut candles = parse_candle_push(text);
    candles.sort_by_key(|c| c.timestamp_ms);
    candles
}

/// Maps one OKX order object (`/trade/order`, `/trade/orders-pending`).
pub fn parse_order(o: &Value) -> Result<Order, ExchangeError> {
    let text = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("");
    let id = text("ordId")
        .parse::<u64>()
        .map_err(|_| ExchangeError::MessageParseFailed {
            raw: o.to_string(),
            reason: "missing or non-numeric ordId".to_string(),
        })?;
    let px = num(&o["px"]);
    let avg = num(&o["avgPx"]);
    Ok(Order {
        id,
        symbol: text("instId").to_string(),
        side: if text("side") == "sell" {
            Side::Sell
        } else {
            Side::Buy
        },
        order_type: if text("ordType") == "limit" {
            OrderType::Limit
        } else {
            OrderType::Market
        },
        time_in_force: TimeInForce::GTC,
        price: if avg > 0.0 { avg } else { px },
        stop_price: 0.0,
        status: match text("state") {
            "partially_filled" => OrderStatus::PartiallyFilled,
            "filled" => OrderStatus::Filled,
            "canceled" | "mmp_canceled" => OrderStatus::Cancelled,
            _ => OrderStatus::New,
        },
        quantity: num(&o["sz"]),
        filled_quantity: num(&o["accFillSz"]),
        created_at_ms: text("cTime").parse().unwrap_or(0),
        updated_at_ms: text("uTime").parse().unwrap_or(0),
        trailing_highest_price: None,
    })
}

/// Checks the v5 envelope (`code == "0"`) and per-item `sCode`, returns `data`.
pub fn check_envelope(v: &Value) -> Result<Vec<Value>, ExchangeError> {
    let code = v.get("code").and_then(Value::as_str).unwrap_or("?");
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let item_err = data.iter().find_map(|d| {
        let s = d.get("sCode").and_then(Value::as_str)?;
        (s != "0").then(|| {
            format!(
                "{s} {}",
                d.get("sMsg").and_then(Value::as_str).unwrap_or("")
            )
        })
    });
    if code != "0" || item_err.is_some() {
        let msg = v.get("msg").and_then(Value::as_str).unwrap_or("");
        return Err(ExchangeError::ApiError(format!(
            "OKX code {code}: {}",
            item_err.unwrap_or_else(|| msg.to_string())
        )));
    }
    Ok(data)
}

/// OKX Spot client. One instance can stream and trade; `&self` trading calls
/// and `&mut self` streaming calls do not interfere.
pub struct OkxClient {
    http: Client,
    base_url: String,
    credentials: Option<(ApiCredentials, String)>,
    demo: bool,
    eea: bool,
    tasks: Vec<JoinHandle<()>>,
}

impl OkxClient {
    /// Client with key, secret and passphrase. `credentials.network` decides
    /// demo (Testnet) or live (Mainnet).
    pub fn new(credentials: ApiCredentials, passphrase: String) -> Result<Self, ExchangeError> {
        credentials
            .validate()
            .map_err(ExchangeError::InvalidCredentials)?;
        if passphrase.is_empty() {
            return Err(ExchangeError::InvalidCredentials(
                "OKX passphrase cannot be empty".to_string(),
            ));
        }
        let demo = credentials.is_testnet();
        Ok(Self {
            http: Client::new(),
            base_url: REST_URL.to_string(),
            credentials: Some((credentials, passphrase)),
            demo,
            eea: false,
            tasks: Vec::new(),
        })
    }

    /// Market-data-only client (no orders possible).
    pub fn public(demo: bool) -> Self {
        Self {
            http: Client::new(),
            base_url: REST_URL.to_string(),
            credentials: None,
            demo,
            eea: false,
            tasks: Vec::new(),
        }
    }

    /// Switches REST and WebSocket to the EEA hosts (keys of EU accounts).
    pub fn with_eea(mut self, eea: bool) -> Self {
        self.eea = eea;
        self.base_url = if eea { REST_URL_EEA } else { REST_URL }.to_string();
        self
    }

    fn ws_url(&self) -> &'static str {
        match (self.demo, self.eea) {
            (true, false) => WS_BUSINESS_DEMO,
            (false, false) => WS_BUSINESS_LIVE,
            (true, true) => WS_BUSINESS_DEMO_EEA,
            (false, true) => WS_BUSINESS_LIVE_EEA,
        }
    }

    /// The most recent closed candles (oldest first, at most 300), from the
    /// public `market/candles` endpoint. Used to warm up indicators at start.
    pub async fn recent_candles(
        &self,
        symbol: &str,
        interval: &str,
        limit: usize,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let channel = candle_channel(interval)?;
        let bar = channel.trim_start_matches("candle");
        let url = format!(
            "{}/api/v5/market/candles?instId={}&bar={bar}&limit={}",
            self.base_url,
            inst_id(symbol),
            limit.clamp(1, 300)
        );
        let text = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed {
                url: url.clone(),
                reason: e.to_string(),
            })?
            .text()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed {
                url: url.clone(),
                reason: e.to_string(),
            })?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| ExchangeError::MessageParseFailed {
                raw: text.chars().take(200).collect(),
                reason: e.to_string(),
            })?;
        check_envelope(&v)?;
        Ok(parse_history(&text))
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Vec<Value>, ExchangeError> {
        let (creds, passphrase) = self.credentials.as_ref().ok_or_else(|| {
            ExchangeError::InvalidCredentials("OKX client has no API key".to_string())
        })?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let ts = iso_timestamp(now_ms());
        let signature = sign(&ts, method.as_str(), path, &body, &creds.secret)?;
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .http
            .request(method, &url)
            .header("OK-ACCESS-KEY", &creds.key)
            .header("OK-ACCESS-SIGN", signature)
            .header("OK-ACCESS-TIMESTAMP", ts)
            .header("OK-ACCESS-PASSPHRASE", passphrase)
            .header("Content-Type", "application/json");
        if self.demo {
            req = req.header("x-simulated-trading", "1");
        }
        if !body.is_empty() {
            req = req.body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed {
                url: url.clone(),
                reason: e.to_string(),
            })?;
        let text = resp
            .text()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed {
                url: url.clone(),
                reason: e.to_string(),
            })?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| ExchangeError::MessageParseFailed {
                raw: text.chars().take(200).collect(),
                reason: e.to_string(),
            })?;
        check_envelope(&v)
    }
}

/// Plain decimal with at most 8 places, no float noise (`0.01195654`).
pub fn decimal(x: f64) -> String {
    let s = format!("{x:.8}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Body for `POST /api/v5/trade/order` (cash spot; `sz` in base currency).
pub fn order_body(req: &OrderRequest) -> Result<Value, ExchangeError> {
    if req.quantity <= 0.0 {
        return Err(ExchangeError::InvalidRequest(
            "Quantity must be positive".to_string(),
        ));
    }
    let side = match req.side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    };
    let mut body = json!({
        "instId": inst_id(&req.symbol),
        "tdMode": "cash",
        "side": side,
        "sz": decimal(req.quantity),
    });
    match req.order_type {
        OrderType::Market => {
            body["ordType"] = json!("market");
            // Market buys default to quote currency on OKX spot.
            body["tgtCcy"] = json!("base_ccy");
        }
        OrderType::Limit => {
            let px = req.price.ok_or_else(|| {
                ExchangeError::InvalidRequest("Limit orders require price".to_string())
            })?;
            body["ordType"] = json!("limit");
            body["px"] = json!(px.to_string());
        }
        _ => {
            return Err(ExchangeError::InvalidRequest(
                "OKX connector supports market and limit orders only".to_string(),
            ))
        }
    }
    Ok(body)
}

#[async_trait]
impl ExchangeTrading for OkxClient {
    async fn place_order(&self, req: &OrderRequest) -> Result<Order, ExchangeError> {
        let body = order_body(req)?;
        let data = self
            .request(Method::POST, "/api/v5/trade/order", Some(body))
            .await?;
        let ord_id = data
            .first()
            .and_then(|d| d.get("ordId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Market orders fill asynchronously; one lookup gives the real state.
        if let Ok(Some(order)) = self.get_order(&ord_id, &req.symbol).await {
            return Ok(order);
        }
        let now = now_ms();
        Ok(Order {
            id: ord_id.parse().unwrap_or(0),
            symbol: inst_id(&req.symbol),
            side: req.side,
            order_type: req.order_type.clone(),
            time_in_force: TimeInForce::GTC,
            price: req.price.unwrap_or(0.0),
            stop_price: 0.0,
            status: OrderStatus::New,
            quantity: req.quantity,
            filled_quantity: 0.0,
            created_at_ms: now,
            updated_at_ms: now,
            trailing_highest_price: None,
        })
    }

    async fn cancel_order(&self, order_id: &str, symbol: &str) -> Result<(), ExchangeError> {
        let body = json!({"instId": inst_id(symbol), "ordId": order_id});
        self.request(Method::POST, "/api/v5/trade/cancel-order", Some(body))
            .await
            .map(|_| ())
    }

    async fn get_open_orders(&self) -> Result<Vec<Order>, ExchangeError> {
        self.request(
            Method::GET,
            "/api/v5/trade/orders-pending?instType=SPOT",
            None,
        )
        .await?
        .iter()
        .map(parse_order)
        .collect()
    }

    async fn get_balance(&self) -> Result<Account, ExchangeError> {
        let data = self
            .request(Method::GET, "/api/v5/account/balance", None)
            .await?;
        let mut balances = HashMap::new();
        for d in data
            .iter()
            .filter_map(|a| a.get("details").and_then(Value::as_array))
            .flatten()
        {
            if let Some(ccy) = d.get("ccy").and_then(Value::as_str) {
                balances.insert(
                    ccy.to_string(),
                    AccountBalance {
                        free: num(&d["availBal"]),
                        locked: num(&d["frozenBal"]),
                    },
                );
            }
        }
        Ok(Account { balances })
    }

    async fn get_order(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> Result<Option<Order>, ExchangeError> {
        let path = format!(
            "/api/v5/trade/order?instId={}&ordId={}",
            urlencoding::encode(&inst_id(symbol)),
            urlencoding::encode(order_id)
        );
        let data = self.request(Method::GET, &path, None).await?;
        data.first().map(parse_order).transpose()
    }
}

#[async_trait]
impl ExchangeStream for OkxClient {
    /// Nothing to do up front; each subscription opens its own socket.
    async fn connect(&mut self) -> Result<(), ExchangeError> {
        Ok(())
    }

    async fn subscribe_candles(
        &mut self,
        symbol: &str,
        interval: &str,
    ) -> Result<mpsc::Receiver<Candle>, ExchangeError> {
        let channel = candle_channel(interval)?;
        let inst = inst_id(symbol);
        let url = self.ws_url();
        let (ws, _) = tokio_tungstenite::connect_async(url).await.map_err(|e| {
            ExchangeError::ConnectionFailed {
                url: url.to_string(),
                reason: e.to_string(),
            }
        })?;
        let (mut write, mut read) = ws.split();
        let sub = json!({"op": "subscribe", "args": [{"channel": channel, "instId": inst}]});
        write
            .send(Message::Text(sub.to_string().into()))
            .await
            .map_err(|e| ExchangeError::SubscriptionFailed {
                symbol: inst.clone(),
                reason: e.to_string(),
            })?;
        info!("OKX {url}: subscribed {channel} {inst}");

        let (tx, rx) = mpsc::channel(256);
        let handle = tokio::spawn(async move {
            let mut ping = tokio::time::interval(PING_EVERY);
            ping.tick().await;
            let mut last_ts = 0u64;
            loop {
                tokio::select! {
                    _ = ping.tick() => {
                        if write.send(Message::Text("ping".into())).await.is_err() {
                            warn!("OKX ping failed; closing candle stream");
                            break;
                        }
                    }
                    msg = read.next() => match msg {
                        Some(Ok(Message::Text(text))) => {
                            let text = text.to_string();
                            if text.contains("\"event\":\"error\"") {
                                warn!("OKX subscription error: {text}");
                                break;
                            }
                            for candle in parse_candle_push(&text) {
                                if candle.timestamp_ms <= last_ts {
                                    continue;
                                }
                                last_ts = candle.timestamp_ms;
                                if tx.send(candle).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            info!("OKX WebSocket closed");
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            warn!("OKX WebSocket error: {e}");
                            break;
                        }
                    }
                }
            }
        });
        self.tasks.push(handle);
        Ok(rx)
    }

    async fn subscribe_depth(
        &mut self,
        symbol: &str,
    ) -> Result<mpsc::Receiver<DepthUpdate>, ExchangeError> {
        Err(ExchangeError::SubscriptionFailed {
            symbol: symbol.to_string(),
            reason: "order book depth is not implemented for OKX yet".to_string(),
        })
    }

    async fn disconnect(&mut self) -> Result<(), ExchangeError> {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        Ok(())
    }
}

impl ExchangeInfo for OkxClient {
    fn name(&self) -> &str {
        "OKX"
    }

    fn supported_intervals(&self) -> &[&str] {
        &["1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "1d", "1w"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::Network;

    #[test]
    fn test_inst_id() {
        assert_eq!(inst_id("BTCUSDT"), "BTC-USDT");
        assert_eq!(inst_id("ethusdc"), "ETH-USDC");
        assert_eq!(inst_id("SOL-USDT"), "SOL-USDT");
        assert_eq!(inst_id("ETHBTC"), "ETH-BTC");
        assert_eq!(inst_id("USDT"), "USDT");
    }

    #[test]
    fn test_candle_channel() {
        assert_eq!(candle_channel("1m").unwrap(), "candle1m");
        assert_eq!(candle_channel("4h").unwrap(), "candle4H");
        assert_eq!(candle_channel("1d").unwrap(), "candle1Dutc");
        assert!(candle_channel("x").is_err());
        assert!(candle_channel("5s").is_err());
    }

    #[test]
    fn test_iso_timestamp() {
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_timestamp(1_607_418_537_715), "2020-12-08T09:08:57.715Z");
        assert_eq!(iso_timestamp(1_709_164_800_001), "2024-02-29T00:00:00.001Z");
    }

    #[test]
    fn test_sign_matches_reference() {
        // Reference: python base64(hmac.new(b"secret", msg, sha256).digest())
        let sig = sign(
            "2020-12-08T09:08:57.715Z",
            "GET",
            "/api/v5/account/balance?ccy=BTC",
            "",
            "secret",
        )
        .unwrap();
        assert_eq!(sig, SIG_REFERENCE);
    }

    const SIG_REFERENCE: &str = "wpDvCwYCprcMQsQkxWJiWy+YADoQE4ep+OEKKLimMoY=";

    #[test]
    fn test_parse_candle_push_keeps_only_closed() {
        let text = r#"{"arg":{"channel":"candle1m","instId":"BTC-USDT"},"data":[
            ["1700000000000","1","2","0.5","1.5","10","0","0","1"],
            ["1700000060000","1.5","2","1","1.8","3","0","0","0"]]}"#;
        let candles = parse_candle_push(text);
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].timestamp_ms, 1_700_000_000_000);
        assert_eq!(candles[0].close, 1.5);
        assert!(parse_candle_push(r#"{"event":"subscribe"}"#).is_empty());
        assert!(parse_candle_push("pong").is_empty());
    }

    #[test]
    fn test_parse_order() {
        let o = json!({"ordId": "312269865356374016", "instId": "BTC-USDT",
            "side": "buy", "ordType": "market", "px": "", "avgPx": "64000.5",
            "sz": "0.01", "accFillSz": "0.01", "state": "filled",
            "cTime": "1700000000000", "uTime": "1700000000100"});
        let order = parse_order(&o).unwrap();
        assert_eq!(order.id, 312_269_865_356_374_016);
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.price, 64000.5);
        assert_eq!(order.filled_quantity, 0.01);
        assert!(parse_order(&json!({"ordId": ""})).is_err());
    }

    #[test]
    fn test_envelope_errors() {
        assert!(check_envelope(&json!({"code": "0", "data": [{"sCode": "0"}]})).is_ok());
        let e = check_envelope(&json!({"code": "1", "msg": "",
            "data": [{"sCode": "51008", "sMsg": "Insufficient balance"}]}))
        .unwrap_err();
        assert!(e.to_string().contains("51008 Insufficient balance"));
        let e = check_envelope(&json!({"code": "50113", "msg": "Invalid Sign"})).unwrap_err();
        assert!(e.to_string().contains("Invalid Sign"));
    }

    #[test]
    fn test_order_body() {
        let m = order_body(&OrderRequest::market("BTCUSDT".into(), Side::Buy, 0.01)).unwrap();
        assert_eq!(m["instId"], "BTC-USDT");
        assert_eq!(m["ordType"], "market");
        assert_eq!(m["tgtCcy"], "base_ccy");
        assert_eq!(m["sz"], "0.01");
        assert_eq!(decimal(0.011956540000000001), "0.01195654");
        assert_eq!(decimal(2.0), "2");
        let l = order_body(&OrderRequest::limit(
            "ETHUSDT".into(),
            Side::Sell,
            1.0,
            3000.5,
        ))
        .unwrap();
        assert_eq!(l["px"], "3000.5");
        assert_eq!(l["side"], "sell");
        assert!(order_body(&OrderRequest::market("BTCUSDT".into(), Side::Buy, 0.0)).is_err());
    }

    #[test]
    fn test_new_requires_passphrase() {
        let creds = ApiCredentials::new("k".into(), "s".into(), Network::Testnet);
        assert!(OkxClient::new(creds.clone(), String::new()).is_err());
        let c = OkxClient::new(creds, "p".into()).unwrap();
        assert_eq!(c.ws_url(), WS_BUSINESS_DEMO);
        assert_eq!(OkxClient::public(false).ws_url(), WS_BUSINESS_LIVE);
        let eea = OkxClient::public(true).with_eea(true);
        assert_eq!(eea.ws_url(), WS_BUSINESS_DEMO_EEA);
        assert_eq!(eea.base_url, REST_URL_EEA);
        assert_eq!(
            OkxClient::public(false).with_eea(true).ws_url(),
            WS_BUSINESS_LIVE_EEA
        );
    }

    /// Real OKX demo socket, public data only. Run with
    /// `cargo test -p omnitrade-exchange okx -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_demo_candle_stream_connects() {
        let mut c = OkxClient::public(true);
        c.connect().await.unwrap();
        let _rx = c.subscribe_candles("BTCUSDT", "1m").await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !c.tasks[0].is_finished(),
            "socket closed right after subscribe"
        );
        c.disconnect().await.unwrap();
    }

    /// Read-only check of the demo keys: `OKX_DEMO_API_KEY`, `_SECRET`,
    /// `_PASSPHRASE`. Prints balances only, never the keys.
    #[tokio::test]
    #[ignore]
    async fn live_demo_balance_readable() {
        let var = |n: &str| std::env::var(format!("OKX_DEMO_API_{n}")).unwrap_or_default();
        let creds = ApiCredentials::new(var("KEY"), var("SECRET"), Network::Testnet);
        let eea = std::env::var("OKX_REGION").is_ok_and(|r| r.eq_ignore_ascii_case("eea"));
        let c = OkxClient::new(creds, var("PASSPHRASE"))
            .unwrap()
            .with_eea(eea);
        let account = c.get_balance().await.unwrap();
        for (asset, b) in &account.balances {
            println!("{asset}: frei {} gesperrt {}", b.free, b.locked);
        }
    }

    #[test]
    fn test_parse_history_sorts_and_drops_open_bar() {
        let text = r#"{"code":"0","data":[
            ["300","1","2","0.5","1.5","3","0","0","0"],
            ["200","1","2","0.5","1.4","3","0","0","1"],
            ["100","1","2","0.5","1.3","3","0","0","1"]]}"#;
        let c = parse_history(text);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].timestamp_ms, 100);
        assert_eq!(c[1].close, 1.4);
    }
}
