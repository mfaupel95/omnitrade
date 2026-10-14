//! Tests des Schleifenablaufs: Signale, Kill-Switch, Fehlerpfade, Mainnet-Sperre.

use super::test_support::*;
use super::*;

#[tokio::test]
async fn buy_signal_places_market_order_with_configured_quantity() {
    // Arrange
    let h = Harness::new("buy", BUY_BELOW_40K);

    // Act
    let result = h
        .run(&[35000.0], Some(1000.0), Network::Testnet, 0.002)
        .await;

    // Assert
    assert!(result.is_ok());
    let placed = h.placed.lock().expect("placed");
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].symbol, "BTCUSDT");
    assert!(matches!(placed[0].side, Side::Buy));
    assert!((placed[0].quantity - 0.002).abs() < f64::EPSILON);
}

#[tokio::test]
async fn subscribes_before_connect_and_disconnects_at_end() {
    // Arrange
    let h = Harness::new("order", BUY_BELOW_40K);

    // Act
    let result = h
        .run(&[45000.0], Some(1000.0), Network::Testnet, 0.001)
        .await;

    // Assert
    assert!(result.is_ok());
    assert_eq!(
        *h.log.lock().expect("log"),
        vec!["subscribe", "connect", "disconnect"]
    );
}

#[tokio::test]
async fn engaged_kill_switch_blocks_orders_cancels_open_orders_and_disconnects() {
    let h = Harness::new("kill", BUY_BELOW_40K);
    kill_switch::engage(&h.kill).expect("engage");

    h.run(&[30_000.0], Some(1000.0), Network::Testnet, 0.001)
        .await
        .expect("halt is a graceful end");

    assert!(h.placed.lock().expect("placed").is_empty());
    assert_eq!(*h.cancelled.lock().expect("cancelled"), vec!["77"]);
    assert_eq!(
        h.log.lock().expect("log").last().map(String::as_str),
        Some("disconnect")
    );
}

#[tokio::test]
async fn without_kill_switch_nothing_is_cancelled() {
    let h = Harness::new("nokill", BUY_BELOW_40K);

    h.run(&[30_000.0], Some(1000.0), Network::Testnet, 0.001)
        .await
        .expect("run");

    assert_eq!(h.placed.lock().expect("placed").len(), 1);
    assert!(h.cancelled.lock().expect("cancelled").is_empty());
}

#[tokio::test]
async fn hold_signal_places_no_order() {
    // Arrange
    let h = Harness::new("hold", BUY_BELOW_40K);

    // Act
    let result = h
        .run(&[45000.0], Some(1000.0), Network::Testnet, 0.001)
        .await;

    // Assert
    assert!(result.is_ok());
    assert!(h.placed.lock().expect("placed").is_empty());
}

#[tokio::test]
async fn balance_failure_blocks_order_fail_closed() {
    // Arrange
    let h = Harness::new("nobalance", BUY_BELOW_40K);

    // Act
    let result = h.run(&[35000.0], None, Network::Testnet, 0.001).await;

    // Assert
    assert!(result.is_ok(), "Einzelfehler beenden die Schleife nicht");
    assert!(h.placed.lock().expect("placed").is_empty());
}

#[tokio::test]
async fn oversized_order_is_rejected_by_risk_guard_and_loop_continues() {
    // Arrange: 1.0 BTC * 35000 > 10000 USDT Testnet-Limit
    let h = Harness::new("oversize", BUY_BELOW_40K);

    // Act
    let result = h
        .run(&[35000.0, 35000.0], Some(1000.0), Network::Testnet, 1.0)
        .await;

    // Assert
    assert!(result.is_ok());
    assert!(h.placed.lock().expect("placed").is_empty());
    assert_eq!(
        h.log.lock().expect("log").last().map(String::as_str),
        Some("disconnect")
    );
}

#[tokio::test]
async fn mainnet_is_rejected_before_any_exchange_call() {
    // Arrange
    let h = Harness::new("mainnet", BUY_BELOW_40K);

    // Act
    let result = h
        .run(&[35000.0], Some(1000.0), Network::Mainnet, 0.001)
        .await;

    // Assert
    assert!(matches!(result, Err(LiveLoopError::NetworkError(_))));
    assert!(h.log.lock().expect("log").is_empty());
}

#[tokio::test]
async fn non_positive_quantity_is_rejected() {
    // Arrange
    let h = Harness::new("qty", BUY_BELOW_40K);

    // Act
    let zero = h.run(&[35000.0], Some(1000.0), Network::Testnet, 0.0).await;
    let negative = h
        .run(&[35000.0], Some(1000.0), Network::Testnet, -1.0)
        .await;

    // Assert
    assert!(matches!(zero, Err(LiveLoopError::ExchangeError(_))));
    assert!(matches!(negative, Err(LiveLoopError::ExchangeError(_))));
    assert!(h.log.lock().expect("log").is_empty());
}

#[tokio::test]
async fn missing_script_file_returns_script_file_error() {
    // Arrange
    let h = Harness::new("missing", BUY_BELOW_40K);
    fs::remove_file(&h.script).expect("remove script");

    // Act
    let result = h
        .run(&[35000.0], Some(1000.0), Network::Testnet, 0.001)
        .await;

    // Assert
    assert!(matches!(result, Err(LiveLoopError::ScriptFileError(_))));
}

#[test]
fn test_live_loop_error_display() {
    let err = LiveLoopError::ScriptFileError(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "file not found",
    ));
    assert!(err.to_string().contains("Script file error"));
}
