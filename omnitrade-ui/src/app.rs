//! Main application layout with `egui_tiles` docking and central workspace panels.

use std::sync::{Arc, RwLock};

use omnitrade_engine::L2OrderBook;

use crate::feed;
use crate::state::UIState;
use crate::widgets::chart::CandleChartWidget;
use crate::widgets::depth::DepthWidget;

/// Identifies the type of content rendered in a workspace tile pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKind {
    /// Candlestick price chart.
    Chart,
    /// Level-2 order book depth visualization.
    OrderBook,
    /// Active positions summary.
    Positions,
    /// System and trade execution logs.
    Console,
}

impl PanelKind {
    /// Returns the human-readable display title for the panel tab.
    pub fn title(&self) -> &'static str {
        match self {
            Self::Chart => "Chart",
            Self::OrderBook => "Order Book",
            Self::Positions => "Positions",
            Self::Console => "Console",
        }
    }
}

/// Custom `egui_tiles::Behavior` implementation for rendering tile contents.
pub struct OmniTradeAppBehavior<'a> {
    /// Mutable reference to the chart widget.
    pub chart: &'a mut CandleChartWidget,
    /// Reference to the order book depth widget.
    pub depth: &'a DepthWidget,
    /// Optional snapshot of the active symbol's L2 order book.
    pub orderbook: Option<&'a L2OrderBook>,
    /// Lines for the positions panel (status summary).
    pub summary: &'a [String],
    /// Lines for the console panel (recent orders, newest first).
    pub orders: &'a [String],
}

impl<'a> egui_tiles::Behavior<PanelKind> for OmniTradeAppBehavior<'a> {
    fn tab_title_for_pane(&mut self, pane: &PanelKind) -> egui::WidgetText {
        pane.title().into()
    }

    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        _tile_id: egui_tiles::TileId,
        pane: &mut PanelKind,
    ) -> egui_tiles::UiResponse {
        match pane {
            PanelKind::Chart => {
                self.chart.show(ui);
            }
            PanelKind::OrderBook => {
                self.depth.show(ui, self.orderbook);
            }
            PanelKind::Positions => {
                show_lines(ui, self.summary, "no status yet");
            }
            PanelKind::Console => {
                show_lines(ui, self.orders, "no orders yet");
            }
        }
        egui_tiles::UiResponse::None
    }
}

/// Renders text lines, or a hint when there are none.
fn show_lines(ui: &mut egui::Ui, lines: &[String], empty: &str) {
    if lines.is_empty() {
        ui.weak(empty);
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for line in lines {
            ui.monospace(line);
        }
    });
}

/// Main `omnitrade` application state holding UI tree and sub-widgets.
pub struct OmniTradeApp {
    /// Shared reference to the thread-safe passive `UIState`.
    pub state: Arc<RwLock<UIState>>,
    /// Tile layout tree managing dockable panels.
    pub tree: egui_tiles::Tree<PanelKind>,
    /// Candlestick chart widget.
    pub chart: CandleChartWidget,
    /// Order book depth widget.
    pub depth: DepthWidget,
}

impl OmniTradeApp {
    /// Creates a new `OmniTradeApp`: chart on the left, positions above console on the right.
    pub fn new(state: Arc<RwLock<UIState>>) -> Self {
        let mut tiles = egui_tiles::Tiles::default();
        let chart_pane = tiles.insert_pane(PanelKind::Chart);
        let positions_pane = tiles.insert_pane(PanelKind::Positions);
        let console_pane = tiles.insert_pane(PanelKind::Console);
        let side = tiles.insert_vertical_tile(vec![positions_pane, console_pane]);
        let root = tiles.insert_horizontal_tile(vec![chart_pane, side]);
        let tree = egui_tiles::Tree::new("omnitrade_tile_tree", root, tiles);

        Self {
            state,
            tree,
            chart: CandleChartWidget::new(Vec::new()),
            depth: DepthWidget::new(15),
        }
    }
}

impl eframe::App for OmniTradeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut current_orderbook = None;
        let mut summary = Vec::new();
        let mut orders = Vec::new();

        let guard = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let symbol = guard
            .status
            .as_ref()
            .map_or_else(|| "BTCUSDT".to_string(), |s| s.symbol.clone());
        let candles = guard.latest_candles(&symbol, 100).to_vec();
        self.chart.set_candles(candles);
        if let Some(book) = guard.orderbook.get(&symbol) {
            current_orderbook = Some(book.clone());
        }
        if let Some(doc) = &guard.status {
            summary = feed::summary_lines(doc);
            orders = feed::order_lines(doc);
        }
        if let Some(err) = &guard.feed_error {
            summary.insert(0, format!("feed: {err}"));
        }

        let mut behavior = OmniTradeAppBehavior {
            chart: &mut self.chart,
            depth: &self.depth,
            orderbook: current_orderbook.as_ref(),
            summary: &summary,
            orders: &orders,
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            self.tree.ui(&mut behavior, ui);
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnitrade_core::Candle;

    #[test]
    fn test_panel_kind_titles() {
        // Arrange & Act & Assert
        assert_eq!(PanelKind::Chart.title(), "Chart");
        assert_eq!(PanelKind::OrderBook.title(), "Order Book");
        assert_eq!(PanelKind::Positions.title(), "Positions");
        assert_eq!(PanelKind::Console.title(), "Console");
    }

    #[test]
    fn test_omnitrade_app_default_initialization() {
        // Arrange
        let state = Arc::new(RwLock::new(UIState::new()));

        // Act
        let app = OmniTradeApp::new(state);

        // Assert
        assert_eq!(app.chart.candles.len(), 0);
        assert_eq!(app.depth.max_levels, 15);
        assert!(app.tree.root().is_some());
    }

    #[test]
    fn test_uistate_snapshot_reading() {
        // Arrange
        let state = Arc::new(RwLock::new(UIState::new()));
        {
            let mut guard = state.write().expect("Failed to acquire write lock in test");
            guard.update_candle("BTCUSDT", Candle::new(100, 100.0, 105.0, 99.0, 104.0, 10.0));
        }
        let mut app = OmniTradeApp::new(state);

        // Act
        let guard = app.state.read().unwrap_or_else(|p| p.into_inner());
        let candles = guard.latest_candles("BTCUSDT", 10).to_vec();
        app.chart.set_candles(candles);

        // Assert
        assert_eq!(app.chart.candles.len(), 1);
        assert_eq!(app.chart.candles[0].close, 104.0);
    }
}
