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
use crate::trading::OrderRequest;

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
