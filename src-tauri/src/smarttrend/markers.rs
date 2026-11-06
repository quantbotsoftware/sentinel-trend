use std::fmt;

use crate::state::{normalize_tf, Direction, MarkerKind, SmartTrendMarker};

#[allow(dead_code)] // Used by engine.rs and sentinel (Session 6)
pub fn create_marker(
    kind: MarkerKind,
    direction: Direction,
    bar_time: i64,
    price: f64,
    score: f64,
    timeframe: &str,
    symbol: &str,
) -> SmartTrendMarker {
    SmartTrendMarker {
        kind,
        direction,
        bar_time,
        price,
        score,
        timeframe: normalize_tf(timeframe).to_string(),
        symbol: symbol.to_string(),
        display_only: false,
        source: None,
        setup: None,
        conviction: None,
        verdict: None,
    }
}

#[allow(dead_code)] // Used by sentinel logging and UI (Sessions 6-7)
pub fn marker_display_text(m: &SmartTrendMarker) -> String {
    #[allow(deprecated)]
    let kind = match m.kind {
        MarkerKind::Stalking => "STALK",
        MarkerKind::Entry => "ENTRY",
        MarkerKind::Exit => "EXIT",
        MarkerKind::Trail => "TRAIL",
    };
    let dir = match m.direction {
        Direction::Long => "LONG",
        Direction::Short => "SHORT",
    };
    format!(
        "{kind} {dir} {} @ ${:.2} [score={:.1}] {}",
        m.symbol, m.price, m.score, m.timeframe
    )
}

impl fmt::Display for SmartTrendMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&marker_display_text(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_marker_fields() {
        let m = create_marker(
            MarkerKind::Entry,
            Direction::Long,
            1700000000,
            185.50,
            72.3,
            "5m",
            "AAPL",
        );
        assert_eq!(m.kind, MarkerKind::Entry);
        assert_eq!(m.direction, Direction::Long);
        assert_eq!(m.bar_time, 1700000000);
        assert!((m.price - 185.50).abs() < f64::EPSILON);
        assert_eq!(m.symbol, "AAPL");
        assert_eq!(m.timeframe, "5m");
    }

    #[test]
    fn create_marker_normalizes_timeframe() {
        let m = create_marker(
            MarkerKind::Entry,
            Direction::Long,
            1700000000,
            185.50,
            72.3,
            "5Min",
            "AAPL",
        );
        assert_eq!(m.timeframe, "5m");

        let m2 = create_marker(
            MarkerKind::Entry,
            Direction::Long,
            1700000000,
            185.50,
            72.3,
            "1D",
            "AAPL",
        );
        assert_eq!(m2.timeframe, "1d");
    }

    #[test]
    fn display_text_format() {
        let m = create_marker(
            MarkerKind::Exit,
            Direction::Short,
            1700000300,
            180.00,
            35.5,
            "1m",
            "NVDA",
        );
        let text = marker_display_text(&m);
        assert!(text.contains("EXIT"));
        assert!(text.contains("SHORT"));
        assert!(text.contains("NVDA"));
        assert!(text.contains("$180.00"));
    }
}
