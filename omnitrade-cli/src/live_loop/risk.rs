//! Risiko- und Eigenkapital-Helfer der Live-Schleife: Symbolzerlegung, Bewertung,
//! Liquidationsabstand und Positionsdeckel.

use omnitrade_core::Side;
use omnitrade_exchange::{Account, PositionSnapshot};

use super::{KNOWN_QUOTES, MAINNET_HARD_CAP};

/// Zerlegt ein Börsensymbol in (Basis, Quote): `BTC-USDT`, `BTC-USDT-SWAP`, `BTC/USD`
/// oder `BTCUSDT`. Das Suffix `-SWAP` (Perpetual) bezeichnet dasselbe Paar.
pub(crate) fn split_symbol(symbol: &str) -> Option<(String, String)> {
    let upper = symbol.to_uppercase();
    let pair = upper.strip_suffix("-SWAP").unwrap_or(upper.as_str());
    if let Some((base, quote)) = pair.split_once(['-', '/']) {
        return (!base.is_empty() && !quote.is_empty() && !quote.contains(['-', '/']))
            .then(|| (base.to_string(), quote.to_string()));
    }
    KNOWN_QUOTES.iter().find_map(|quote| {
        pair.strip_suffix(quote)
            .filter(|base| !base.is_empty())
            .map(|base| (base.to_string(), (*quote).to_string()))
    })
}

/// Eigenkapital des Handelspaars: Quote-Bestand plus Basis-Bestand zum aktuellen Preis
/// (jeweils frei + gesperrt). Nur das Quote-Guthaben zu zählen ließe jeden Kauf wie
/// einen Verlust in Höhe des Kaufpreises aussehen.
pub(crate) fn pair_equity(account: &Account, base: &str, quote: &str, price: f64) -> f64 {
    let total = |asset: &str| {
        account
            .balances
            .get(asset)
            .map_or(0.0, |b| b.free + b.locked)
    };
    total(quote) + total(base) * price
}

/// Eigenkapital für das Verlustlimit: Kontostände plus unrealisierter PnL offener
/// Perpetuals. Ohne den PnL erscheint ein offener Verlust erst beim Schließen.
pub(crate) fn account_equity(
    account: &Account,
    base: &str,
    quote: &str,
    price: f64,
    position: &PositionSnapshot,
) -> f64 {
    pair_equity(account, base, quote, price) + position.unrealized_pnl
}

/// Mindestabstand Markpreis zu Liquidationspreis (relativ zum Markpreis), bevor eine
/// Position weiter vergrößert werden darf. Bei 10x isolated liegt der Abstand direkt
/// nach dem Einstieg bei rund 9 %; 5 % lässt nur noch wenig Spielraum.
pub(crate) const MIN_LIQUIDATION_DISTANCE: f64 = 0.05;

/// Lehnt Orders ab, die eine offene Perpetual-Position vergrößern, wenn sie der
/// Liquidation zu nahe ist. Reduzierende Orders bleiben erlaubt, damit man aussteigen
/// kann. Fehlen die Preise bei offener Position, wird abgelehnt (fail closed).
pub(crate) fn check_liquidation_distance(
    position: &PositionSnapshot,
    side: Side,
) -> Result<(), String> {
    if !position.is_perpetual || position.base_qty == 0.0 {
        return Ok(());
    }
    let long = position.base_qty > 0.0;
    let increases = (side == Side::Buy) == long;
    if !increases {
        return Ok(());
    }
    let (Some(liq), Some(mark)) = (position.liq_price, position.mark_price) else {
        return Err("Liquidationspreis oder Markpreis fehlt bei offener Position".to_string());
    };
    if !mark.is_finite() || mark <= 0.0 {
        return Err(format!("ungültiger Markpreis {mark}"));
    }
    let distance = (mark - liq).abs() / mark;
    if distance < MIN_LIQUIDATION_DISTANCE {
        return Err(format!(
            "Abstand zur Liquidation {:.2} % unter Mindestwert {:.0} %",
            distance * 100.0,
            MIN_LIQUIDATION_DISTANCE * 100.0
        ));
    }
    Ok(())
}

/// Menge, auf die der Positionsdeckel wirkt: bei Perpetuals die Positionsmenge,
/// bei Spot der Bestand. Sonst wäre der Deckel bei Perpetuals immer 0 und wirkungslos.
pub(crate) fn held_base(account: &Account, base: &str, position: &PositionSnapshot) -> f64 {
    if position.is_perpetual {
        return position.base_qty;
    }
    account
        .balances
        .get(base)
        .map_or(0.0, |b| b.free + b.locked)
}

/// Prüft einen Positionsdeckel für die Position, die ein Lauf selbst aufbaut.
///
/// Bestände, die schon vor dem Lauf im Konto lagen, zählen nicht dazu und werden nie
/// verkauft: Beim ersten Check wird der Basis-Bestand als Ausgangspunkt gemerkt, der
/// Lauf darf nur darüber hinaus kaufen und höchstens das Gekaufte wieder verkaufen.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExposureLimit {
    cap: f64,
    start_base: Option<f64>,
}

impl ExposureLimit {
    pub(crate) fn new(cap: f64) -> Self {
        Self {
            cap,
            start_base: None,
        }
    }

    /// `held_base` ist der aktuelle Gesamtbestand (frei + gesperrt) der Basis-Währung.
    pub(crate) fn check(
        &mut self,
        side: Side,
        quantity: f64,
        price: f64,
        held_base: f64,
    ) -> Result<(), String> {
        let start = *self.start_base.get_or_insert(held_base);
        let run_base = (held_base - start).max(0.0);
        match side {
            Side::Buy => {
                let after = (run_base + quantity) * price;
                if after > self.cap {
                    return Err(format!(
                        "Positionsdeckel überschritten: {after:.2} > {:.2}",
                        self.cap
                    ));
                }
            }
            Side::Sell => {
                // Toleranz 1 % der Ordermenge: Gebühren in der Basis-Währung lassen den
                // Bestand leicht unter der gekauften Menge liegen.
                if quantity > run_base + quantity * 0.01 {
                    return Err(format!(
                        "Verkauf {quantity} übersteigt die in diesem Lauf gekaufte Menge {run_base}"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// `true`, wenn der Deckel für Mainnet gesetzt, endlich, positiv und unter dem Höchstwert liegt.
pub(crate) fn valid_mainnet_cap(cap: Option<f64>) -> bool {
    cap.is_some_and(|c| c.is_finite() && c > 0.0 && c <= MAINNET_HARD_CAP)
}
