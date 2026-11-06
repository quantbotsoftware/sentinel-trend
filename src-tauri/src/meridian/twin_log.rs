//! Twin log (`<data_dir>/meridian/live/intents.jsonl`): one line per
//! engine decision with the code hash and a config fingerprint (manifest
//! per decision, plan §0.0.1), plus the restart seed that keeps a relaunch
//! from re-announcing the morning's decisions.

use std::io::Write;

use chrono::Utc;

use super::history;
use super::shadow::{Intent, ReplaySnapshot};

/// Short, stable fingerprint of the MERIDIAN config (manifest per decision).
pub fn config_fingerprint(cfg: &super::MeridianConfig) -> String {
    use std::hash::{Hash, Hasher};
    let json = serde_json::to_string(cfg).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn append_twin_log(intents: &[Intent], snap: &ReplaySnapshot, cfg_fp: &str) {
    if intents.is_empty() {
        return;
    }
    let path = history::cache_dir().join("intents.jsonl");
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    for i in intents {
        let line = serde_json::json!({
            "ts": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "date": snap.date,
            "symbol": i.symbol,
            "bar_idx": i.bar_idx,
            "delta_shares": i.delta_shares,
            "fill_price": i.fill_price,
            "sleeves": i.sleeves,
            "net_after": i.net_after,
            "equity": snap.equity,
            "code": env!("ST_GIT_HASH"),
            // the paper counter runs on this one (plan §0.0.1): crate + host
            // module, so UI / legacy / daemon commits do not reset it
            "strategy": env!("ST_MERIDIAN_HASH"),
            "config": cfg_fp,
        });
        let _ = writeln!(f, "{line}");
    }
}

/// Seed the "already reported" set from today's twin-log lines so a restart
/// does not re-announce (and re-log) the morning's decisions.
pub fn seed_from_twin_log(today: chrono::NaiveDate) -> ReplaySnapshot {
    let mut snap = ReplaySnapshot { date: Some(today), ..Default::default() };
    let Ok(text) = std::fs::read_to_string(history::cache_dir().join("intents.jsonl")) else { return snap };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v.get("date").and_then(|d| d.as_str()) != Some(&today.to_string()) {
            continue;
        }
        let (Some(symbol), Some(bar_idx), Some(delta)) =
            (v.get("symbol").and_then(|x| x.as_str()), v.get("bar_idx").and_then(|x| x.as_u64()), v.get("delta_shares").and_then(|x| x.as_f64()))
        else {
            continue;
        };
        snap.intents_today.push(Intent {
            symbol: symbol.to_string(),
            bar_idx: bar_idx as usize,
            delta_shares: delta,
            fill_price: v.get("fill_price").and_then(|x| x.as_f64()).unwrap_or(0.0),
            sleeves: Vec::new(),
            bar_time: 0,
            time_et: String::new(),
            net_after: v.get("net_after").and_then(|x| x.as_f64()).unwrap_or(0.0),
            label: String::new(),
        });
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_fingerprint_is_stable_and_sensitive() {
        let cfg = super::super::MeridianConfig::default();
        let fp = config_fingerprint(&cfg);
        assert_eq!(fp.len(), 16);
        assert_eq!(fp, config_fingerprint(&cfg.clone()));
        assert_ne!(fp, config_fingerprint(&super::super::MeridianConfig { capital: 1.0, ..cfg }));
    }

    #[test]
    fn strategy_hash_is_baked_into_the_binary() {
        let h = env!("ST_MERIDIAN_HASH");
        assert_eq!(h.len(), 12, "12 hex chars from build.rs, got {h:?}");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(h, env!("ST_GIT_HASH"), "strategy hash is not the repo hash");
    }

    #[test]
    fn twin_log_seed_dedups_restart() {
        // no log file → empty seed dated today (never panics)
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 3).unwrap_or_default();
        let s = seed_from_twin_log(today);
        assert_eq!(s.date, Some(today));
    }
}
