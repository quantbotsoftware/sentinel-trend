//! Markdown rendering for `frontier backtest`.

use titan_frontier::curve::{beta, max_drawdown_dollars, sharpe};
use titan_frontier::gate::{Combination, GateResult};
use titan_frontier::{Curve, TResult};

pub struct Variant<'a> {
    pub label: String,
    pub res: &'a TResult,
    /// SPY $ P/L on the same dates (buy-and-hold on `capital`), for beta
    pub spy_pnl: Option<Vec<f64>>,
}

pub fn fmt_money(x: f64) -> String {
    format!("{}${:.0}", if x < 0.0 { "−" } else { "+" }, x.abs())
}

fn years(n: usize) -> f64 {
    (n as f64 / 252.0).max(1e-9)
}

/// Summary line of one T variant.
pub fn t_summary(v: &Variant, capital: f64) -> String {
    let r = v.res;
    let n = r.pnl.len();
    let gross: f64 = r.gross_pnl.iter().sum();
    let costs: f64 = r.costs.iter().sum();
    let net: f64 = r.pnl.iter().sum();
    let curve = Curve::new(r.dates.clone(), r.pnl.clone());
    let worst_month = curve.by_month().iter().map(|m| m.2).fold(0.0f64, f64::min);
    let shares: i64 = r.trades.iter().map(|t| t.delta_shares.abs()).sum();
    let notional: f64 = r.trades.iter().map(|t| (t.delta_shares as f64 * t.price).abs()).sum();
    let avg_gross = r.gross_exposure.iter().sum::<f64>() / n.max(1) as f64;
    let max_gross = r.gross_exposure.iter().copied().fold(0.0, f64::max);
    let avg_active = r.n_active.iter().sum::<usize>() as f64 / n.max(1) as f64;
    let b = v.spy_pnl.as_ref().map(|s| beta(&r.pnl, s)).unwrap_or(f64::NAN);
    let vol = titan_frontier::curve::ann_vol(&r.pnl, capital);
    format!(
        "| {} | {} → {} ({n} d) | **{:.2}** | {:.1} % | {} | {} | {} | {:.1} % | {} | {:.2}× / {:.2}× | {:.1} | {} | {}/an, ${:.0}k/an | {:.2} |",
        v.label,
        r.dates.first().map(|d| d.to_string()).unwrap_or_default(),
        r.dates.last().map(|d| d.to_string()).unwrap_or_default(),
        sharpe(&r.pnl, capital),
        vol * 100.0,
        fmt_money(net / years(n)),
        fmt_money(gross / years(n)),
        fmt_money(-costs / years(n)),
        if gross > 0.0 { costs / gross * 100.0 } else { f64::INFINITY },
        fmt_money(max_drawdown_dollars(&r.pnl)),
        avg_gross,
        max_gross,
        avg_active,
        fmt_money(worst_month),
        (r.trades.len() as f64 / years(n)).round(),
        notional / years(n) / 1000.0,
        b,
    ) + &format!(" {} acț./an", (shares as f64 / years(n)).round())
}

pub fn t_header() -> String {
    "| Variantă | Fereastră | Sharpe net | vol | net/an | brut/an | cost/an | cost/brut | MDD $ | brut mediu / max | instr. active | cea mai proastă lună | tranzacții | β SPY |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|".into()
}

pub fn yearly_table(variants: &[Variant], ab: Option<&Curve>) -> String {
    let mut years: Vec<i32> = Vec::new();
    let curves: Vec<Curve> = variants.iter().map(|v| Curve::new(v.res.dates.clone(), v.res.pnl.clone())).collect();
    for c in &curves {
        for (y, _) in c.by_year() {
            if !years.contains(&y) {
                years.push(y);
            }
        }
    }
    years.sort_unstable();
    let mut s = format!("| An | {} |{}\n|---|{}|{}\n", variants.iter().map(|v| v.label.clone()).collect::<Vec<_>>().join(" | "), if ab.is_some() { " A+B |" } else { "" }, variants.iter().map(|_| "---").collect::<Vec<_>>().join("|"), if ab.is_some() { "---|" } else { "" });
    let ab_years: Vec<(i32, f64)> = ab.map(|c| c.by_year()).unwrap_or_default();
    for y in years {
        let cells: Vec<String> = curves.iter().map(|c| c.by_year().iter().find(|(yy, _)| *yy == y).map(|(_, p)| fmt_money(*p)).unwrap_or_else(|| "—".into())).collect();
        let abc = if ab.is_some() { format!(" {} |", ab_years.iter().find(|(yy, _)| *yy == y).map(|(_, p)| fmt_money(*p)).unwrap_or_else(|| "—".into())) } else { String::new() };
        s.push_str(&format!("| {y} | {} |{abc}\n", cells.join(" | ")));
    }
    s
}

pub fn gate_table(label: &str, c: &Combination, g: &GateResult, primary: bool, capital: f64) -> String {
    let pf = |b: bool| if b { "PASS" } else { "**FAIL**" };
    let mut s = format!(
        "### Poarta F1 — {label}{}\n\nFereastra comună: {} sesiuni · A+B: Sharpe {:.2}, vol {:.1} %, {}/an, MDD {} · T: Sharpe {:.2}, vol {:.1} %, {}/an, MDD {} · Comb (s = {:.3}): Sharpe {:.2}, vol {:.1} %, {}/an, MDD {}\n\n",
        if primary { " (varianta primară — decide)" } else { " (varianta declarată)" },
        c.n,
        c.sharpe_ab,
        c.vol_ab * 100.0,
        fmt_money(c.mean_ab_per_year),
        fmt_money(c.mdd_ab),
        c.sharpe_t,
        c.vol_t * 100.0,
        fmt_money(c.mean_t_per_year),
        fmt_money(c.mdd_t),
        c.scale,
        c.sharpe_comb,
        c.vol_comb * 100.0,
        fmt_money(c.mean_comb_per_year),
        fmt_money(c.mdd_comb),
    );
    s.push_str("| # | Condiție | Valoare | Prag | Verdict |\n|---|---|---|---|---|\n");
    s.push_str(&format!("| G1 | Sharpe T net | {:.2} | ≥ 0,50 | {} |\n", g.g1.value, pf(g.g1.pass)));
    s.push_str(&format!("| G2 | corr(ΔT, ΔAB) | {:.2} | ≤ 0,40 | {} |\n", g.g2.value, pf(g.g2.pass)));
    s.push_str(&format!("| G3 | Sharpe(Comb) − Sharpe(AB) | {:+.2} | ≥ +0,10 | {} |\n", g.g3.value, pf(g.g3.pass)));
    s.push_str(&format!("| G4 | MDD(Comb) − MDD(AB) | {} | ≥ 0 (nu mai adânc) | {} |\n", fmt_money(g.g4.value), pf(g.g4.pass)));
    s.push_str(&format!("| G5 | cost T / brut T | {:.1} % | ≤ 20 % | {} |\n", g.g5.value * 100.0, pf(g.g5.pass)));
    s.push_str(&format!("| G6 | DSR(Comb), N = 66 | {:.3} | ≥ 0,95 | {} |\n", g.g6.value, pf(g.g6.pass)));
    s.push_str(&format!(
        "\n**Verdict {label}: {}** · informativ: PSR(T singur vs 0) {:.3}; Sharpe(Comb) față de pragul absolut 1,37: {}; |MDD(Comb)| față de 12,1 %: {}; skew Comb {:+.2}, kurtoză {:.1}, n {}\n",
        if g.pass { "PASS" } else { "FAIL" },
        g.psr_t,
        if c.sharpe_comb >= 1.37 { "peste" } else { "sub" },
        if c.mdd_comb.abs() <= 0.121 * capital { "în limită" } else { "peste" },
        g.moments_comb.skew,
        g.moments_comb.kurtosis,
        g.moments_comb.n,
    ));
    s
}
