use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::config::UIConfig;
use crate::state::{FsmPhase, MarkerKind, SmartTrendMarker};

use crate::smarttrend::engine::{EvalResult, PillarDiagnostics};

#[derive(Debug, Clone, Serialize)]
pub struct SentinelAlert {
    pub symbol: String,
    pub phase: String,
    pub prev_phase: String,
    pub score: f64,
    pub direction: String,
    pub bar_time: i64,
    pub timeframe: String,
    pub markers: Vec<MarkerPayload>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarkerPayload {
    pub kind: String,
    pub direction: String,
    pub price: f64,
    pub score: f64,
    pub bar_time: i64,
    /// Vetoed Entry → frontend MUST render as SKIP, not GO BUY.
    pub display_only: bool,
    /// F3b — `apex` / `frontier` / `fsm` / …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conviction: Option<f64>,
    /// `GO` | `VETO` | `HOLD` | `FSM` | `SHADOW`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SentinelMetrics {
    pub symbols_tracked: u32,
    pub symbols_idle: u32,
    pub symbols_hot: u32,
    pub evals_total: u64,
    pub last_eval_duration_us: u64,
    pub last_eval_ts: Option<i64>,
}

pub(super) fn emit_eval_event(app: &AppHandle, symbol: &str, diag: &PillarDiagnostics) {
    let payload = serde_json::json!({
        "symbol": symbol,
        "diagnostics": diag,
    });
    let _ = app.emit_all("smarttrend-eval", &payload);
}

pub(super) fn emit_alert_event(
    app: &AppHandle,
    symbol: &str,
    result: &EvalResult,
    prev_phase: FsmPhase,
    timeframe: &str,
    bar_time: i64,
) {
    let markers: Vec<MarkerPayload> = result
        .marker
        .as_ref()
        .map(|m| vec![MarkerPayload {
            kind: format!("{:?}", m.kind),
            direction: format!("{:?}", m.direction),
            price: m.price,
            score: m.score,
            bar_time: m.bar_time,
            display_only: m.display_only,
            source: m.source.clone().or_else(|| Some("fsm".into())),
            setup: m.setup.clone(),
            conviction: m.conviction,
            verdict: m.verdict.clone().or_else(|| {
                Some(if m.display_only {
                    "VETO".into()
                } else {
                    "FSM".into()
                })
            }),
        }])
        .unwrap_or_default();

    let alert = SentinelAlert {
        symbol: symbol.to_string(),
        phase: format!("{:?}", result.new_fsm.phase),
        prev_phase: format!("{:?}", prev_phase),
        score: result.diagnostics.combined_score,
        direction: format!("{:?}", result.diagnostics.combined_direction),
        bar_time,
        timeframe: timeframe.to_string(),
        markers,
    };
    let _ = app.emit_all("smarttrend-alert", &alert);
}

pub(super) fn emit_status_event(app: &AppHandle, metrics: &SentinelMetrics) {
    let _ = app.emit_all("sentinel-status", metrics);
}

pub(super) fn send_desktop_notification(
    app: &AppHandle,
    symbol: &str,
    markers: &[SmartTrendMarker],
    ui_cfg: &UIConfig,
) {
    if !ui_cfg.desktop_notifications {
        return;
    }
    for marker in markers {
        // Never notify "ENTRY" on vetoed (display_only) markers — that
        // trained operators to chase SKIP arrows as if they were fills.
        if marker.display_only && marker.kind == MarkerKind::Entry {
            continue;
        }
        let (title, body) = match marker.kind {
            MarkerKind::Entry => (
                format!("GO BUY {}", symbol),
                format!("{:?} @ ${:.2} (gates passed — not a fill)", marker.direction, marker.price),
            ),
            MarkerKind::Exit => (
                format!("FSM SELL {}", symbol),
                format!("{:?} @ ${:.2}", marker.direction, marker.price),
            ),
            _ => continue,
        };
        let _ = tauri::api::notification::Notification::new(
            &app.config().tauri.bundle.identifier,
        )
        .title(&title)
        .body(&body)
        .show();
    }
}
