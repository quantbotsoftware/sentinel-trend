//! Minimal Alpaca trading client for MERIDIAN (paper or live by config).
//!
//! Deliberately independent of `execution/` (legacy wallets, brackets, PDT
//! gates, flat-by-close): MERIDIAN orders are tagged `mrd-*` client ids and
//! reconciled against broker truth by `exec.rs`. Pure builders/parsers are
//! separated from the async calls so they can be unit-tested.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tif {
    Day,
    /// Market-on-close (accepted until 15:50 ET).
    Cls,
    /// Market-on-open (accepted until 09:28 ET, or queued after 19:00 ET).
    Opg,
}

impl Tif {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tif::Day => "day",
            Tif::Cls => "cls",
            Tif::Opg => "opg",
        }
    }
    pub fn parse(s: &str) -> Option<Tif> {
        match s {
            "day" => Some(Tif::Day),
            "cls" => Some(Tif::Cls),
            "opg" => Some(Tif::Opg),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderSpec {
    pub symbol: String,
    /// Signed shares: + buy / − sell. Whole shares only.
    pub qty: f64,
    pub tif: Tif,
    pub client_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerOrder {
    pub id: String,
    pub client_order_id: String,
    pub symbol: String,
    /// Signed shares (+ buy / − sell).
    pub qty: f64,
    pub tif: Tif,
    pub status: String,
}

pub fn order_body(spec: &OrderSpec) -> serde_json::Value {
    serde_json::json!({
        "symbol": spec.symbol,
        "qty": format!("{}", spec.qty.abs().round() as i64),
        "side": if spec.qty > 0.0 { "buy" } else { "sell" },
        "type": "market",
        "time_in_force": spec.tif.as_str(),
        "client_order_id": spec.client_order_id,
    })
}

#[derive(Debug, Deserialize)]
struct RawOrder {
    id: String,
    #[serde(default)]
    client_order_id: String,
    symbol: String,
    #[serde(default)]
    qty: Option<String>,
    #[serde(default)]
    filled_qty: Option<String>,
    side: String,
    time_in_force: String,
    status: String,
}

/// Parse `GET /v2/orders?status=open` — keeps only MERIDIAN (`mrd-`) orders.
/// `qty` is the **remaining** quantity (`qty − filled_qty`): the filled part
/// is already in the position, so counting the full order would double it.
pub fn parse_open_orders(v: &serde_json::Value) -> Vec<BrokerOrder> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|o| serde_json::from_value::<RawOrder>(o.clone()).ok())
        .filter(|o| o.client_order_id.starts_with("mrd-"))
        .filter_map(|o| {
            let total: f64 = o.qty.as_deref().unwrap_or("0").parse().ok()?;
            let filled: f64 = o.filled_qty.as_deref().unwrap_or("0").parse().unwrap_or(0.0);
            let q = (total - filled).max(0.0);
            let signed = if o.side == "sell" { -q } else { q };
            Some(BrokerOrder { id: o.id, client_order_id: o.client_order_id, symbol: o.symbol, qty: signed, tif: Tif::parse(&o.time_in_force)?, status: o.status })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct RawPosition {
    qty: String,
    #[serde(default)]
    side: String,
}

/// Signed position from `GET /v2/positions/{symbol}` (404 ⇒ 0).
pub fn parse_position(v: &serde_json::Value) -> f64 {
    let Ok(p) = serde_json::from_value::<RawPosition>(v.clone()) else { return 0.0 };
    let q: f64 = p.qty.parse().unwrap_or(0.0);
    if p.side == "short" { -q.abs() } else { q }
}

/// `(equity, last_equity)` from `GET /v2/account` (strings in the API).
pub fn parse_account(v: &serde_json::Value) -> Option<(f64, f64)> {
    let f = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
    Some((f("equity")?, f("last_equity")?))
}

pub struct Broker<'a> {
    pub client: &'a reqwest::Client,
    pub base: &'static str,
    pub key: &'a str,
    pub secret: &'a str,
}

impl<'a> Broker<'a> {
    pub fn new(client: &'a reqwest::Client, paper: bool, key: &'a str, secret: &'a str) -> Self {
        Self { client, base: if paper { "https://paper-api.alpaca.markets" } else { "https://api.alpaca.markets" }, key, secret }
    }

    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client.request(m, format!("{}{}", self.base, path)).header("APCA-API-KEY-ID", self.key).header("APCA-API-SECRET-KEY", self.secret)
    }

    pub async fn position(&self, symbol: &str) -> Result<f64, String> {
        let r = self.req(reqwest::Method::GET, &format!("/v2/positions/{symbol}")).send().await.map_err(|e| e.to_string())?;
        if r.status().as_u16() == 404 {
            return Ok(0.0);
        }
        if !r.status().is_success() {
            return Err(format!("position {symbol}: HTTP {}", r.status()));
        }
        Ok(parse_position(&r.json::<serde_json::Value>().await.map_err(|e| e.to_string())?))
    }

    /// `(equity, last_equity)` — the marks for the live risk rail.
    pub async fn account(&self) -> Result<(f64, f64), String> {
        let r = self.req(reqwest::Method::GET, "/v2/account").send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("account: HTTP {}", r.status()));
        }
        parse_account(&r.json::<serde_json::Value>().await.map_err(|e| e.to_string())?).ok_or_else(|| "account: equity fields missing".to_string())
    }

    /// Daily equity closes over the last month (`portfolio/history`), raw JSON.
    pub async fn portfolio_history(&self) -> Result<serde_json::Value, String> {
        let r = self
            .req(reqwest::Method::GET, "/v2/account/portfolio/history")
            .query(&[("period", "1M"), ("timeframe", "1D")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("portfolio history: HTTP {}", r.status()));
        }
        r.json::<serde_json::Value>().await.map_err(|e| e.to_string())
    }

    /// `GET /v2/calendar` for one date (raw JSON array; empty on holidays).
    pub async fn calendar(&self, date: chrono::NaiveDate) -> Result<serde_json::Value, String> {
        let d = date.to_string();
        let r = self.req(reqwest::Method::GET, "/v2/calendar").query(&[("start", d.as_str()), ("end", d.as_str())]).send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("calendar: HTTP {}", r.status()));
        }
        r.json::<serde_json::Value>().await.map_err(|e| e.to_string())
    }

    pub async fn open_orders(&self, symbol: &str) -> Result<Vec<BrokerOrder>, String> {
        let r = self
            .req(reqwest::Method::GET, "/v2/orders")
            .query(&[("status", "open"), ("symbols", symbol), ("limit", "200")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("orders {symbol}: HTTP {}", r.status()));
        }
        Ok(parse_open_orders(&r.json::<serde_json::Value>().await.map_err(|e| e.to_string())?))
    }

    pub async fn submit(&self, spec: &OrderSpec) -> Result<String, String> {
        let r = self.req(reqwest::Method::POST, "/v2/orders").json(&order_body(spec)).send().await.map_err(|e| e.to_string())?;
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("submit {} {:+.0}: HTTP {status} {body}", spec.symbol, spec.qty));
        }
        Ok(serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())).unwrap_or_default())
    }

    pub async fn cancel(&self, order_id: &str) -> Result<(), String> {
        let r = self.req(reqwest::Method::DELETE, &format!("/v2/orders/{order_id}")).send().await.map_err(|e| e.to_string())?;
        // 204 cancelled · 404 already gone · 422 already filled/cancelled
        if r.status().is_success() || matches!(r.status().as_u16(), 404 | 422) {
            Ok(())
        } else {
            Err(format!("cancel {order_id}: HTTP {}", r.status()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_body_is_whole_share_market_with_tif() {
        let b = order_body(&OrderSpec { symbol: "SPY".into(), qty: -560.0, tif: Tif::Cls, client_order_id: "mrd-cls-2026-09-16-SPY".into() });
        assert_eq!(b["side"], "sell");
        assert_eq!(b["qty"], "560");
        assert_eq!(b["type"], "market");
        assert_eq!(b["time_in_force"], "cls");
        assert_eq!(b["client_order_id"], "mrd-cls-2026-09-16-SPY");
    }

    #[test]
    fn parses_only_meridian_open_orders_with_signed_qty() {
        let v = serde_json::json!([
            {"id":"1","client_order_id":"mrd-cls-2026-09-16-SPY","symbol":"SPY","qty":"560","side":"sell","time_in_force":"cls","status":"accepted"},
            {"id":"2","client_order_id":"legacy-abc","symbol":"SPY","qty":"5","side":"buy","time_in_force":"day","status":"new"},
            {"id":"3","client_order_id":"mrd-2026-09-16-QQQ-29-0","symbol":"QQQ","qty":"381","side":"buy","time_in_force":"day","status":"new"}
        ]);
        let o = parse_open_orders(&v);
        assert_eq!(o.len(), 2);
        assert_eq!((o[0].qty, o[0].tif), (-560.0, Tif::Cls));
        assert_eq!((o[1].qty, o[1].tif), (381.0, Tif::Day));
        // partially filled: only the remainder is in flight (400 of 1000 already in the position)
        let p = serde_json::json!([{"id":"4","client_order_id":"mrd-x","symbol":"SPY","qty":"1000","filled_qty":"400","side":"buy","time_in_force":"day","status":"partially_filled"}]);
        assert_eq!(parse_open_orders(&p)[0].qty, 600.0);
    }

    #[test]
    fn account_equity_fields_are_strings() {
        let v = serde_json::json!({"equity":"98153.02","last_equity":"98300.5","cash":"247770.19"});
        assert_eq!(parse_account(&v), Some((98_153.02, 98_300.5)));
        assert_eq!(parse_account(&serde_json::json!({"equity":"1"})), None);
    }

    #[test]
    fn position_sign_follows_side() {
        assert_eq!(parse_position(&serde_json::json!({"qty":"560","side":"long"})), 560.0);
        assert_eq!(parse_position(&serde_json::json!({"qty":"498","side":"short"})), -498.0);
        assert_eq!(parse_position(&serde_json::json!({"qty":"-498","side":"short"})), -498.0);
        assert_eq!(parse_position(&serde_json::json!({"code":40410000})), 0.0);
    }
}
