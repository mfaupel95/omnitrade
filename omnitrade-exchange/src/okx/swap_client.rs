//! SWAP-spezifische Teile des OKX-REST-Clients: Instrumentregeln laden, Hebel setzen,
//! Positionsmodus prüfen und Mengen zwischen Basiswährung und Kontrakten umrechnen.

use std::collections::HashMap;
use std::sync::RwLock;

use omnitrade_core::Order;
use reqwest::Method;
use serde_json::Value;

use crate::error::ExchangeError;
use crate::okx::instrument::{InstType, Instrument};
use crate::okx::rest::OkxRestClient;
use crate::okx::swap;
use crate::trading::{OrderRequest, PositionSnapshot};

/// Wertet die Antwort von `/account/positions` für ein SWAP-Instrument aus.
///
/// Fehlt ein Feld, ist das ein Fehler: ein stilles `0` würde das Risiko-Limit
/// unbemerkt außer Kraft setzen. Ein leerer Text (OKX für „nicht zutreffend“) zählt als 0.
pub(crate) fn parse_swap_position(
    data: &[Value],
    inst: &Instrument,
) -> Result<PositionSnapshot, ExchangeError> {
    let mut snapshot = PositionSnapshot {
        is_perpetual: true,
        ..PositionSnapshot::default()
    };
    for item in data {
        if item.get("instId").and_then(Value::as_str) != Some(inst.inst_id.as_str()) {
            continue;
        }
        let contracts = optional_number(item, "pos")?;
        snapshot.base_qty += inst.contracts_to_base(contracts);
        snapshot.unrealized_pnl += optional_number(item, "upl")?;
    }
    Ok(snapshot)
}

fn optional_number(item: &Value, key: &str) -> Result<f64, ExchangeError> {
    let raw = item
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ExchangeError::AccountError(format!("Position ohne Feld '{key}'")))?;
    if raw.is_empty() {
        return Ok(0.0);
    }
    raw.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .ok_or_else(|| ExchangeError::AccountError(format!("Feld '{key}' ist keine Zahl: '{raw}'")))
}

/// Erkennt SWAP-Instrumente an der OKX-Notation `BTC-USDT-SWAP`.
#[must_use]
pub(crate) fn is_swap_id(inst_id: &str) -> bool {
    inst_id.ends_with("-SWAP")
}

/// Cache der Instrumentregeln, befüllt durch [`OkxRestClient::load_instrument`].
pub(crate) type InstrumentCache = RwLock<HashMap<String, Instrument>>;

fn cache_poisoned() -> ExchangeError {
    ExchangeError::ApiError("Instrument-Cache nicht verfügbar".to_string())
}

impl OkxRestClient {
    /// Lädt die Handelsregeln eines Instruments und merkt sie für die Umrechnung vor.
    ///
    /// # Errors
    /// `ApiError`, wenn OKX das Instrument nicht kennt oder die Regeln ungültig sind.
    pub async fn load_instrument(
        &self,
        inst_id: &str,
        inst_type: InstType,
    ) -> Result<Instrument, ExchangeError> {
        let path = format!(
            "/api/v5/public/instruments?instType={}&instId={}",
            inst_type.as_api_str(),
            urlencoding::encode(inst_id)
        );
        let data = self.call(Method::GET, &path, None).await?;
        let inst = data
            .first()
            .ok_or_else(|| ExchangeError::ApiError(format!("{inst_id}: Instrument unbekannt")))
            .and_then(Instrument::parse)?;
        self.instruments
            .write()
            .map_err(|_| cache_poisoned())?
            .insert(inst.inst_id.clone(), inst.clone());
        Ok(inst)
    }

    /// Setzt den Hebel (isolated) für ein Instrument. Deckel: [`swap::MAX_LEVERAGE`].
    ///
    /// # Errors
    /// `InvalidRequest` bei Hebel außerhalb des Bereichs, sonst API-Fehler.
    pub async fn set_leverage(&self, inst_id: &str, lever: u32) -> Result<(), ExchangeError> {
        let body = swap::leverage_body(inst_id, lever)?;
        self.call(Method::POST, "/api/v5/account/set-leverage", Some(&body))
            .await
            .map(|_| ())
    }

    /// Stellt sicher, dass das Konto im one-way-Modus läuft. Fail closed.
    ///
    /// # Errors
    /// `AccountError` im Hedge-Modus, sonst API-Fehler.
    pub async fn ensure_net_mode(&self) -> Result<(), ExchangeError> {
        let data = self
            .call(Method::GET, "/api/v5/account/config", None)
            .await?;
        swap::check_position_mode(&data)
    }

    /// Liefert die gecachte SWAP-Regel oder einen Fehler mit Handlungsanweisung.
    pub(crate) fn swap_instrument(&self, inst_id: &str) -> Result<Instrument, ExchangeError> {
        let cached = self
            .instruments
            .read()
            .map_err(|_| cache_poisoned())?
            .get(inst_id)
            .cloned();
        cached
            .filter(|i| i.inst_type == InstType::Swap)
            .ok_or_else(|| {
                ExchangeError::InvalidRequest(format!(
                    "{inst_id}: SWAP-Instrument nicht geladen (load_instrument zuerst)"
                ))
            })
    }

    /// Baut den Order-Body für SWAP. Die Menge wird auf Kontrakte gerundet, bevor sie
    /// den Adapter verlässt.
    pub(crate) fn swap_order_body(
        &self,
        req: &OrderRequest,
        px: Option<f64>,
    ) -> Result<Value, ExchangeError> {
        let inst = self.swap_instrument(&req.symbol)?;
        let contracts = inst.base_to_contracts(req.quantity)?;
        let text = swap::format_contracts(contracts, inst.lot_sz);
        Ok(swap::order_body(req, &text, px))
    }

    /// Liest die Position eines SWAP-Instruments (`/account/positions`).
    pub(crate) async fn fetch_swap_position(
        &self,
        inst_id: &str,
    ) -> Result<PositionSnapshot, ExchangeError> {
        let inst = self.swap_instrument(inst_id)?;
        let path = format!(
            "/api/v5/account/positions?instType=SWAP&instId={}",
            urlencoding::encode(inst_id)
        );
        let data = self.call(Method::GET, &path, None).await?;
        parse_swap_position(&data, &inst)
    }

    /// Rechnet Kontraktmengen einer SWAP-Order in Basiswährung zurück. SPOT bleibt unberührt.
    pub(crate) fn to_base_units(&self, mut order: Order) -> Result<Order, ExchangeError> {
        if !is_swap_id(&order.symbol) {
            return Ok(order);
        }
        let inst = self.swap_instrument(&order.symbol)?;
        order.quantity = inst.contracts_to_base(order.quantity);
        order.filled_quantity = inst.contracts_to_base(order.filled_quantity);
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn btc_swap() -> Instrument {
        Instrument::parse(&json!({
            "instId": "BTC-USDT-SWAP", "instType": "SWAP", "ctVal": "0.01",
            "lotSz": "0.01", "minSz": "0.01",
        }))
        .expect("fixture parses")
    }

    #[test]
    fn long_position_converts_contracts_and_carries_upl() {
        // Arrange: 5 Kontrakte long, Verlust -12.5 USDT
        let data = [json!({"instId": "BTC-USDT-SWAP", "pos": "5", "upl": "-12.5"})];
        // Act
        let snap = parse_swap_position(&data, &btc_swap()).expect("parses");
        // Assert
        assert!(snap.is_perpetual);
        assert!((snap.base_qty - 0.05).abs() < 1e-12);
        assert!((snap.unrealized_pnl + 12.5).abs() < 1e-12);
    }

    #[test]
    fn short_position_is_negative_in_base() {
        let data = [json!({"instId": "BTC-USDT-SWAP", "pos": "-3", "upl": "4"})];
        let snap = parse_swap_position(&data, &btc_swap()).expect("parses");
        assert!((snap.base_qty + 0.03).abs() < 1e-12);
    }

    #[test]
    fn empty_result_means_flat_perpetual() {
        let snap = parse_swap_position(&[], &btc_swap()).expect("parses");
        assert!(snap.is_perpetual);
        assert_eq!(snap.base_qty, 0.0);
        assert_eq!(snap.unrealized_pnl, 0.0);
    }

    #[test]
    fn empty_text_counts_as_zero_but_missing_field_fails_closed() {
        let flat = [json!({"instId": "BTC-USDT-SWAP", "pos": "0", "upl": ""})];
        assert_eq!(
            parse_swap_position(&flat, &btc_swap())
                .expect("empty is zero")
                .unrealized_pnl,
            0.0
        );
        let missing = [json!({"instId": "BTC-USDT-SWAP", "pos": "1"})];
        assert!(parse_swap_position(&missing, &btc_swap()).is_err());
    }

    #[test]
    fn other_instruments_are_ignored() {
        let data = [json!({"instId": "ETH-USDT-SWAP", "pos": "9", "upl": "99"})];
        let snap = parse_swap_position(&data, &btc_swap()).expect("parses");
        assert_eq!(snap.base_qty, 0.0);
    }
}
