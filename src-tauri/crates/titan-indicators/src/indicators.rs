use std::collections::VecDeque;
use titan_types::Candle;

type TripleSeries = (Vec<Option<f64>>, Vec<Option<f64>>, Vec<Option<f64>>);

#[derive(Debug, Clone, serde::Serialize)]
pub struct Indicators {
    pub ema_9: Option<f64>,
    pub ema_20: Option<f64>,
    pub ema_50: Option<f64>,
    pub ema_200: Option<f64>,
    pub rsi: Option<f64>,
    pub macd: Option<f64>,
    pub macd_signal: Option<f64>,
    pub macd_hist: Option<f64>,
    pub bb_upper: Option<f64>,
    pub bb_lower: Option<f64>,
    pub bb_mid: Option<f64>,
    pub adx: Option<f64>,
    pub vwap: Option<f64>,
    pub stoch_rsi: Option<f64>,
    pub stoch_k: Option<f64>,
    pub stoch_d: Option<f64>,
}

// --- EMA ---
pub fn calculate_ema_series(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut ema_values = Vec::with_capacity(values.len());
    let multiplier = 2.0 / (period as f64 + 1.0);

    let mut current_ema = None;

    for &value in values {
        if let Some(prev) = current_ema {
            let next = (value - prev) * multiplier + prev;
            current_ema = Some(next);
        } else {
            // Initialize with first value (standard approximation) or wait for SMA?
            // Pandas adjust=False initializes with first value.
            current_ema = Some(value);
        }
        ema_values.push(current_ema);
    }
    ema_values
}

// --- RSI ---
pub fn calculate_rsi_series(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut rsi_values = Vec::with_capacity(values.len());

    if values.len() < period + 1 {
        for _ in 0..values.len() {
            rsi_values.push(None);
        }
        return rsi_values;
    }

    // Calculating diffs
    let mut gains = Vec::new();
    let mut losses = Vec::new();

    for i in 1..values.len() {
        let diff = values[i] - values[i - 1];
        if diff > 0.0 {
            gains.push(diff);
            losses.push(0.0);
        } else {
            gains.push(0.0);
            losses.push(-diff); // absolute loss
        }
    }

    // Initial SMA
    let mut avg_gain: f64 = gains.iter().take(period).sum::<f64>() / period as f64;
    let mut avg_loss: f64 = losses.iter().take(period).sum::<f64>() / period as f64;

    // First period + 1 points have None (or first `period` points)
    // RSI available at index `period` (using period diffs)
    for _ in 0..period {
        rsi_values.push(None);
    }

    let first_rs = if avg_loss == 0.0 {
        100.0
    } else {
        avg_gain / avg_loss
    };
    let first_rsi = 100.0 - (100.0 / (1.0 + first_rs));
    rsi_values.push(Some(first_rsi));

    // Wilder's Smoothing
    for i in period..gains.len() {
        avg_gain = ((avg_gain * (period as f64 - 1.0)) + gains[i]) / period as f64;
        avg_loss = ((avg_loss * (period as f64 - 1.0)) + losses[i]) / period as f64;

        let rs = if avg_loss == 0.0 {
            100.0
        } else {
            avg_gain / avg_loss
        };
        let rsi = 100.0 - (100.0 / (1.0 + rs));
        rsi_values.push(Some(rsi));
    }

    rsi_values
}

// --- MACD ---
#[cfg(test)]
#[allow(dead_code)]
pub fn calculate_macd_series(values: &[f64]) -> TripleSeries {
    // Standard 12, 26, 9
    let ema_12 = calculate_ema_series(values, 12);
    let ema_26 = calculate_ema_series(values, 26);

    let mut macd_line = Vec::with_capacity(values.len());
    let mut signal_input = Vec::new();

    for i in 0..values.len() {
        if let (Some(e12), Some(e26)) = (ema_12[i], ema_26[i]) {
            let val = e12 - e26;
            macd_line.push(Some(val));
            signal_input.push(val);
        } else {
            macd_line.push(None);
            // Keep alignment? If None, we can't push to signal calculator easily without tracking indices
            // But EMA function expects contiguous slice.
            // Hack: signal is calculated on valid MACD values only.
        }
    }

    // Calculate Signal Line (EMA 9 of MACD)
    // We need to map back to original length
    // For simplicity here, we re-calculate on the filtered list and pad?
    // Proper way:
    // Create vector of length N.
    // Filter MACD values.

    // Simpler: Just feed the constructed valid series
    let signal_series_packed = calculate_ema_series(&signal_input, 9);

    // Reconstruct full series
    let mut signal_line = vec![None; values.len()];
    let mut histogram = vec![None; values.len()];

    // We started having valid MACD at index 0 (since EMA adjust=False starts at 0)
    // So signal_input[i] corresponds to macd_line[i] which corresponds to values[i]

    for i in 0..signal_input.len() {
        signal_line[i] = signal_series_packed[i];
        if let (Some(m), Some(s)) = (macd_line[i], signal_line[i]) {
            histogram[i] = Some(m - s);
        }
    }

    (macd_line, signal_line, histogram)
}

pub fn calculate_macd_series_with_periods(values: &[f64], fast: usize, slow: usize, signal_period: usize) -> TripleSeries {
    let ema_fast = calculate_ema_series(values, fast);
    let ema_slow = calculate_ema_series(values, slow);

    let mut macd_line = Vec::with_capacity(values.len());
    let mut signal_input = Vec::new();

    for i in 0..values.len() {
        if let (Some(ef), Some(es)) = (ema_fast[i], ema_slow[i]) {
            let val = ef - es;
            macd_line.push(Some(val));
            signal_input.push(val);
        } else {
            macd_line.push(None);
        }
    }

    let signal_series_packed = calculate_ema_series(&signal_input, signal_period);

    let mut signal_line = vec![None; values.len()];
    let mut histogram = vec![None; values.len()];

    for i in 0..signal_input.len() {
        signal_line[i] = signal_series_packed[i];
        if let (Some(m), Some(s)) = (macd_line[i], signal_line[i]) {
            histogram[i] = Some(m - s);
        }
    }

    (macd_line, signal_line, histogram)
}

// --- Bollinger Bands ---
pub fn calculate_bb_series(values: &[f64], period: usize, std_dev_mult: f64) -> TripleSeries {
    if period < 2 {
        let n = values.len();
        return (vec![None; n], vec![None; n], vec![None; n]);
    }
    let mut upper = Vec::with_capacity(values.len());
    let mut lower = Vec::with_capacity(values.len());
    let mut mid = Vec::with_capacity(values.len());

    if values.len() < period {
        for _ in 0..values.len() {
            upper.push(None);
            lower.push(None);
            mid.push(None);
        }
        return (upper, lower, mid);
    }

    for i in 0..values.len() {
        if i < period - 1 {
            upper.push(None);
            lower.push(None);
            mid.push(None);
            continue;
        }

        let window = &values[i + 1 - period..=i];
        let sum: f64 = window.iter().sum();
        let mean = sum / period as f64;

        // Std Dev
        let variance_sum: f64 = window.iter().map(|&x| (x - mean).powi(2)).sum();
        let _variance = variance_sum / period as f64; // Using Population variance usually, or sample (period-1)? Pandas std uses N-1 default.
                                                      // Rust iter std usually implies N-1. Let's stick to N-1 (Sample).
        let std_dev = (variance_sum / (period as f64 - 1.0)).sqrt();

        upper.push(Some(mean + (std_dev_mult * std_dev)));
        lower.push(Some(mean - (std_dev_mult * std_dev)));
        mid.push(Some(mean));
    }

    (upper, lower, mid)
}

// --- ADX ---
pub fn calculate_adx_series(candles: &[Candle], period: usize) -> Vec<Option<f64>> {
    let len = candles.len();
    let adx_values = vec![None; len];

    if len < period * 2 {
        return adx_values;
    }

    let mut tr = Vec::with_capacity(len);
    let mut plus_dm = Vec::with_capacity(len);
    let mut minus_dm = Vec::with_capacity(len);

    // 1. TR, +DM, -DM
    tr.push(0.0);
    plus_dm.push(0.0);
    minus_dm.push(0.0); // First value skipped/zero

    for i in 1..len {
        let h = candles[i].high;
        let l = candles[i].low;
        let prev_c = candles[i - 1].close;

        let hl = h - l;
        let hc = (h - prev_c).abs();
        let lc = (l - prev_c).abs();

        let true_range = hl.max(hc).max(lc);
        tr.push(true_range);

        let prev_h = candles[i - 1].high;
        let prev_l = candles[i - 1].low;

        let up_move = h - prev_h;
        let down_move = prev_l - l;

        let p_dm = if up_move > down_move && up_move > 0.0 {
            up_move
        } else {
            0.0
        };
        let m_dm = if down_move > up_move && down_move > 0.0 {
            down_move
        } else {
            0.0
        };

        plus_dm.push(p_dm);
        minus_dm.push(m_dm);
    }

    // 2. Rolling averages (App.py uses Simple Rolling Mean for ADX inputs?)
    // "df['Plus_DM'].rolling(window=14).mean()"

    // Note: Wilder's ADX uses smoothed moving average (SMMA), but Python script explicitly uses .rolling(window=14).mean().
    // We MUST follow Python logic for parity to maintain "strategy identity".

    let smooth_tr = rolling_mean(&tr, period);
    let smooth_p_dm = rolling_mean(&plus_dm, period);
    let smooth_m_dm = rolling_mean(&minus_dm, period);

    let mut dx_values = vec![0.0; len]; // Intermediate DX

    for i in 0..len {
        if let (Some(atr), Some(pdm), Some(mdm)) = (smooth_tr[i], smooth_p_dm[i], smooth_m_dm[i]) {
            if atr == 0.0 {
                continue;
            }
            let p_di = 100.0 * (pdm / atr);
            let m_di = 100.0 * (mdm / atr);

            let sum = p_di + m_di;
            if sum == 0.0 {
                dx_values[i] = 0.0;
            } else {
                dx_values[i] = 100.0 * (p_di - m_di).abs() / sum;
            }
        }
    }

    // 3. ADX = SMA of DX
    // "df['ADX'] = dx.rolling(window=14).mean()"
    rolling_mean(&dx_values, period)
}

fn rolling_mean(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut result = Vec::with_capacity(values.len());
    let mut sum = 0.0;

    for i in 0..values.len() {
        sum += values[i];
        if i >= period {
            sum -= values[i - period];
        }

        if i >= period - 1 {
            result.push(Some(sum / period as f64));
        } else {
            result.push(None); // Not enough data
        }
    }
    result
}

// --- EHLERS N-POLE GAUSSIAN CHANNEL ---

#[derive(Debug, Clone, serde::Serialize)]
pub struct GaussianChannelBands {
    pub mid: Vec<Option<f64>>,
    pub upper: Vec<Option<f64>>,
    pub lower: Vec<Option<f64>>,
}

pub fn ehlers_npole_filter(values: &[f64], length: usize, poles: usize) -> Vec<Option<f64>> {
    if values.is_empty() || length < 2 {
        return vec![None; values.len()];
    }

    // Calculate beta and alpha
    let pi = std::f64::consts::PI;
    let beta =
        (1.0 - (2.0 * pi / length as f64).cos()) / ((2.0_f64.powf(1.0 / poles as f64)) - 1.0);
    if beta * beta + 2.0 * beta < 0.0 {
        return vec![None; values.len()];
    }
    let alpha = -beta + (beta.powi(2) + 2.0 * beta).sqrt();

    // Apply filtering recursively for N poles
    let mut filtered = values.to_vec();

    for _ in 0..poles {
        let mut result = Vec::with_capacity(values.len());
        result.push(filtered[0]); // First value unchanged

        for i in 1..filtered.len() {
            let filtered_val = alpha * filtered[i] + (1.0 - alpha) * result[i - 1];
            result.push(filtered_val);
        }

        filtered = result;
    }

    filtered.into_iter().map(Some).collect()
}

pub fn calculate_filtered_tr(candles: &[Candle], length: usize, poles: usize) -> Vec<Option<f64>> {
    if candles.len() < 2 {
        return vec![None; candles.len()];
    }

    let mut tr_values = Vec::with_capacity(candles.len());
    tr_values.push(0.0); // First candle has no previous

    for i in 1..candles.len() {
        let high = candles[i].high;
        let low = candles[i].low;
        let prev_close = candles[i - 1].close;

        let hl = high - low;
        let hc = (high - prev_close).abs();
        let lc = (low - prev_close).abs();

        tr_values.push(hl.max(hc).max(lc));
    }

    ehlers_npole_filter(&tr_values, length, poles)
}

pub fn calculate_gaussian_channel_ehlers(
    candles: &[Candle],
    length: usize,
    poles: usize,
    multiplier: f64,
) -> GaussianChannelBands {
    let len = candles.len();
    let empty_bands = GaussianChannelBands {
        mid: vec![None; len],
        upper: vec![None; len],
        lower: vec![None; len],
    };

    if candles.len() < 20 {
        return empty_bands;
    }

    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();

    // Middle band = N-pole filter of close
    let mid = ehlers_npole_filter(&closes, length, poles);

    // Filtered TR for volatility
    let filt_tr = calculate_filtered_tr(candles, length, poles);

    // Calculate bands: mid ± (multiplier × filtered_tr) - TradingView specification
    let mut bands = GaussianChannelBands {
        mid: mid.clone(),
        upper: Vec::with_capacity(len),
        lower: Vec::with_capacity(len),
    };

    for i in 0..len {
        if let (Some(m), Some(tr)) = (mid[i], filt_tr[i]) {
            let deviation = multiplier * tr;
            bands.upper.push(Some(m + deviation));
            bands.lower.push(Some(m - deviation));
        } else {
            bands.upper.push(None);
            bands.lower.push(None);
        }
    }

    bands
}

// --- VWAP ---
pub fn calculate_vwap_series(candles: &[Candle]) -> Vec<Option<f64>> {
    // Cumulative calculation per app.py
    // df['VWAP'] = (df['Volume'] * (df['High'] + df['Low'] + df['Close']) / 3).cumsum() / df['Volume'].cumsum()

    let mut result = Vec::with_capacity(candles.len());
    let mut cum_vol = 0.0;
    let mut cum_vol_price = 0.0;

    for c in candles {
        let typ_price = (c.high + c.low + c.close) / 3.0;
        let vol = c.volume;

        cum_vol += vol;
        cum_vol_price += vol * typ_price;

        if cum_vol == 0.0 {
            result.push(Some(typ_price)); // Fallback
        } else {
            result.push(Some(cum_vol_price / cum_vol));
        }
    }
    result
}

// --- Stoch RSI ---
pub fn calculate_stoch_rsi_series(rsi_values: &[Option<f64>], period: usize) -> TripleSeries {
    // Stoch RSI = (Current RSI - Lowest Low RSI) / (Highest High RSI - Lowest Low RSI)
    // Then K = SMA(Stoch, 3), D = SMA(K, 3)

    let len = rsi_values.len();
    let mut stoch_rsi = vec![None; len];

    for i in 0..len {
        if i < period - 1 {
            continue;
        }

        // Get window of RSI
        // Handle Options. If any None in window, result is None?
        // RSI usually has Nones at start.

        let mut min_rsi = f64::MAX;
        let mut max_rsi = f64::MIN;
        let mut valid = true;
        let mut current_val = 0.0;

        for j in 0..period {
            if let Some(v) = rsi_values[i - j] {
                if j == 0 {
                    current_val = v;
                }
                if v < min_rsi {
                    min_rsi = v;
                }
                if v > max_rsi {
                    max_rsi = v;
                }
            } else {
                valid = false;
                break;
            }
        }

        if valid && max_rsi != min_rsi {
            stoch_rsi[i] = Some((current_val - min_rsi) / (max_rsi - min_rsi));
        } else if valid {
            stoch_rsi[i] = Some(0.0); // Flat line
        }
    }

    // Smooth K (SMA 3)
    // Need to handle Option vector for rolling mean helper
    // Let's make a quick helper that handles options or treats None as skip
    // For simplicity, convert None to 0.0 (bad) or reconstruct.
    // Better: manual loop

    let k_period = 3;
    let d_period = 3;

    let stoch_k = simple_moving_average_opt(&stoch_rsi, k_period);
    // Multiply by 100 per app.py logic: "df['STOCH_K'] = ... * 100"
    let stoch_k_100: Vec<Option<f64>> = stoch_k.iter().map(|v| v.map(|x| x * 100.0)).collect();

    let stoch_d = simple_moving_average_opt(&stoch_k_100, d_period);

    (stoch_rsi, stoch_k_100, stoch_d)
}

fn simple_moving_average_opt(values: &[Option<f64>], period: usize) -> Vec<Option<f64>> {
    let mut res = vec![None; values.len()];
    for i in period - 1..values.len() {
        let mut sum = 0.0;
        let mut valid = true;
        for j in 0..period {
            if let Some(v) = values[i - j] {
                sum += v;
            } else {
                valid = false;
                break;
            }
        }
        if valid {
            res[i] = Some(sum / period as f64);
        }
    }
    res
}

// --- MAIN CALCULATION ENTRY ---
#[derive(Debug, Clone)]
pub struct IndicatorPeriods {
    pub ema_fast: usize,
    pub ema_mid: usize,
    pub ema_slow: usize,
    pub ema_trend: usize,
    pub rsi: usize,
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
    pub bb: usize,
    pub bb_std_dev: f64,
    pub adx: usize,
    pub stoch_rsi: usize,
}

impl Default for IndicatorPeriods {
    fn default() -> Self {
        Self {
            ema_fast: 9, ema_mid: 20, ema_slow: 50, ema_trend: 200,
            rsi: 14, macd_fast: 12, macd_slow: 26, macd_signal: 9,
            bb: 20, bb_std_dev: 2.0, adx: 14, stoch_rsi: 14,
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub fn calculate_all(candles: &[Candle]) -> Indicators {
    calculate_all_with_periods(candles, &IndicatorPeriods::default())
}

pub fn calculate_all_with_periods(candles: &[Candle], periods: &IndicatorPeriods) -> Indicators {
    if candles.is_empty() {
        return Indicators {
            ema_9: None,
            ema_20: None,
            ema_50: None,
            ema_200: None,
            rsi: None,
            macd: None,
            macd_signal: None,
            macd_hist: None,
            bb_upper: None,
            bb_lower: None,
            bb_mid: None,
            adx: None,
            vwap: None,
            stoch_rsi: None,
            stoch_k: None,
            stoch_d: None,
        };
    }

    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();

    let ema9_s = calculate_ema_series(&closes, periods.ema_fast);
    let ema20_s = calculate_ema_series(&closes, periods.ema_mid);
    let ema50_s = calculate_ema_series(&closes, periods.ema_slow);
    let ema200_s = calculate_ema_series(&closes, periods.ema_trend);

    let rsi_s = calculate_rsi_series(&closes, periods.rsi);

    let (macd_s, sig_s, hist_s) = calculate_macd_series_with_periods(&closes, periods.macd_fast, periods.macd_slow, periods.macd_signal);

    let (bb_up_s, bb_lo_s, bb_mid_s) = calculate_bb_series(&closes, periods.bb, periods.bb_std_dev);

    let adx_s = calculate_adx_series(candles, periods.adx);

    let vwap_s = calculate_vwap_series(candles);

    let (stoch_rsi_s, stoch_k_s, stoch_d_s) = calculate_stoch_rsi_series(&rsi_s, periods.stoch_rsi);

    let last_idx = candles.len() - 1;

    Indicators {
        ema_9: ema9_s[last_idx],
        ema_20: ema20_s[last_idx],
        ema_50: ema50_s[last_idx],
        ema_200: ema200_s[last_idx],
        rsi: rsi_s[last_idx],
        macd: macd_s[last_idx],
        macd_signal: sig_s[last_idx],
        macd_hist: hist_s[last_idx],
        bb_upper: bb_up_s[last_idx],
        bb_lower: bb_lo_s[last_idx],
        bb_mid: bb_mid_s[last_idx],
        adx: adx_s[last_idx],
        vwap: vwap_s[last_idx],
        stoch_rsi: stoch_rsi_s[last_idx],
        stoch_k: stoch_k_s[last_idx],
        stoch_d: stoch_d_s[last_idx],
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndicatorsHistory {
    pub ema_9: Vec<Option<f64>>,
    pub ema_20: Vec<Option<f64>>,
    pub ema_50: Vec<Option<f64>>,
    pub ema_200: Vec<Option<f64>>,
    pub rsi: Vec<Option<f64>>,
    pub macd_line: Vec<Option<f64>>,
    pub macd_signal: Vec<Option<f64>>,
    pub macd_hist: Vec<Option<f64>>,
    pub bb_upper: Vec<Option<f64>>,
    pub bb_lower: Vec<Option<f64>>,
    pub bb_mid: Vec<Option<f64>>,
    pub adx: Vec<Option<f64>>,
    pub vwap: Vec<Option<f64>>,
    pub stoch_rsi: Vec<Option<f64>>,
    pub stoch_k: Vec<Option<f64>>,
    pub stoch_d: Vec<Option<f64>>,
    pub gc_bands: Option<GaussianChannelBands>,
}

#[cfg(test)]
#[allow(dead_code)]
pub fn calculate_all_history(candles: &[Candle]) -> IndicatorsHistory {
    calculate_all_history_with_periods(candles, &IndicatorPeriods::default())
}

pub fn calculate_all_history_with_periods(candles: &[Candle], periods: &IndicatorPeriods) -> IndicatorsHistory {
    if candles.is_empty() {
        return IndicatorsHistory {
            ema_9: vec![],
            ema_20: vec![],
            ema_50: vec![],
            ema_200: vec![],
            rsi: vec![],
            macd_line: vec![],
            macd_signal: vec![],
            macd_hist: vec![],
            bb_upper: vec![],
            bb_lower: vec![],
            bb_mid: vec![],
            adx: vec![],
            vwap: vec![],
            stoch_rsi: vec![],
            stoch_k: vec![],
            stoch_d: vec![],
            gc_bands: None,
        };
    }

    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();

    let ema9_s = calculate_ema_series(&closes, periods.ema_fast);
    let ema20_s = calculate_ema_series(&closes, periods.ema_mid);
    let ema50_s = calculate_ema_series(&closes, periods.ema_slow);
    let ema200_s = calculate_ema_series(&closes, periods.ema_trend);
    let rsi_s = calculate_rsi_series(&closes, periods.rsi);
    let (macd_s, sig_s, hist_s) = calculate_macd_series_with_periods(&closes, periods.macd_fast, periods.macd_slow, periods.macd_signal);
    let (bb_up_s, bb_lo_s, bb_mid_s) = calculate_bb_series(&closes, periods.bb, periods.bb_std_dev);
    let adx_s = calculate_adx_series(candles, periods.adx);
    let vwap_s = calculate_vwap_series(candles);
    let (stoch_rsi_s, stoch_k_s, stoch_d_s) = calculate_stoch_rsi_series(&rsi_s, periods.stoch_rsi);
    let gc_bands = calculate_gaussian_channel_ehlers(candles, 144, 4, 1.414);

    IndicatorsHistory {
        ema_9: ema9_s,
        ema_20: ema20_s,
        ema_50: ema50_s,
        ema_200: ema200_s,
        rsi: rsi_s,
        macd_line: macd_s,
        macd_signal: sig_s,
        macd_hist: hist_s,
        bb_upper: bb_up_s,
        bb_lower: bb_lo_s,
        bb_mid: bb_mid_s,
        adx: adx_s,
        vwap: vwap_s,
        stoch_rsi: stoch_rsi_s,
        stoch_k: stoch_k_s,
        stoch_d: stoch_d_s,
        gc_bands: Some(gc_bands),
    }
}

pub fn calculate_atr(candles: &[Candle], period: usize) -> Option<f64> {
    let series = calculate_atr_series(candles, period);
    series.into_iter().rev().find_map(|v| v)
}

pub fn calculate_atr_series(candles: &[Candle], period: usize) -> Vec<Option<f64>> {
    let len = candles.len();
    if len < 2 || period == 0 {
        return vec![None; len];
    }

    let mut tr = Vec::with_capacity(len);
    tr.push(candles[0].high - candles[0].low);

    for i in 1..len {
        let h = candles[i].high;
        let l = candles[i].low;
        let prev_c = candles[i - 1].close;
        let hl = h - l;
        let hc = (h - prev_c).abs();
        let lc = (l - prev_c).abs();
        tr.push(hl.max(hc).max(lc));
    }

    let mut atr_values: Vec<Option<f64>> = vec![None; len];

    if len < period {
        return atr_values;
    }

    let first_atr: f64 = tr[..period].iter().sum::<f64>() / period as f64;
    atr_values[period - 1] = Some(first_atr);

    let mut prev_atr = first_atr;
    for i in period..len {
        let new_atr = (prev_atr * (period as f64 - 1.0) + tr[i]) / period as f64;
        atr_values[i] = Some(new_atr);
        prev_atr = new_atr;
    }

    atr_values
}

/// SMA-ATR helper (legacy parity for sentinel.rs trend metrics).
/// Uses Simple Moving Average of True Range over the last `period` candles.
/// Wilder-smoothed ATR: see `calculate_atr`. Prefer Wilder for new code;
/// SMA preserved for signal calibration parity (entry thresholds tuned on SMA).
pub fn calculate_atr_sma_last(candles: &[Candle], period: usize) -> Option<f64> {
    if period == 0 || candles.len() < period + 1 { return None; }
    let start = candles.len() - period;
    let mut sum_tr = 0.0;
    for idx in start..candles.len() {
        let bar = &candles[idx];
        let prev_close = candles[idx - 1].close;
        let tr = (bar.high - bar.low)
            .max((bar.high - prev_close).abs())
            .max((bar.low - prev_close).abs());
        sum_tr += tr;
    }
    let atr = sum_tr / period as f64;
    if atr.is_finite() && atr > 0.0 { Some(atr) } else { None }
}

/// EMA last value (multiplier=2/(period+1), seed=values[0]).
/// Matches the local `ema()` helper used in sentinel.rs.
pub fn calculate_ema_last(values: &[f64], period: usize) -> f64 {
    if values.is_empty() || period == 0 { return 0.0; }
    let k = 2.0 / (period as f64 + 1.0);
    let mut val = values[0];
    for &p in &values[1..] { val = p * k + val * (1.0 - k); }
    val
}

// ═══════════════════════════════════════════════════════════════════
//  STREAMING INDICATORS — O(1) incremental updates per candle
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct EmaState {
    value: Option<f64>,
    alpha: f64,
}

impl EmaState {
    fn new(period: usize) -> Self {
        Self {
            value: None,
            alpha: 2.0 / (period as f64 + 1.0),
        }
    }

    fn update(&mut self, price: f64) -> Option<f64> {
        self.value = Some(match self.value {
            Some(prev) => (price - prev) * self.alpha + prev,
            None => price,
        });
        self.value
    }
}

#[derive(Debug, Clone)]
struct RsiState {
    period: usize,
    avg_gain: f64,
    avg_loss: f64,
    initialized: bool,
    init_gains: Vec<f64>,
    init_losses: Vec<f64>,
    prev_close: Option<f64>,
    value: Option<f64>,
}

impl RsiState {
    fn new(period: usize) -> Self {
        Self {
            period,
            avg_gain: 0.0,
            avg_loss: 0.0,
            initialized: false,
            init_gains: Vec::with_capacity(period),
            init_losses: Vec::with_capacity(period),
            prev_close: None,
            value: None,
        }
    }

    fn update(&mut self, close: f64) -> Option<f64> {
        let prev = match self.prev_close {
            Some(p) => p,
            None => {
                self.prev_close = Some(close);
                self.value = None;
                return None;
            }
        };
        self.prev_close = Some(close);

        let diff = close - prev;
        let gain = if diff > 0.0 { diff } else { 0.0 };
        let loss = if diff < 0.0 { -diff } else { 0.0 };

        if !self.initialized {
            self.init_gains.push(gain);
            self.init_losses.push(loss);

            if self.init_gains.len() == self.period {
                self.avg_gain = self.init_gains.iter().sum::<f64>() / self.period as f64;
                self.avg_loss = self.init_losses.iter().sum::<f64>() / self.period as f64;
                self.initialized = true;

                let rs = if self.avg_loss == 0.0 { 100.0 } else { self.avg_gain / self.avg_loss };
                self.value = Some(100.0 - (100.0 / (1.0 + rs)));
            } else {
                self.value = None;
            }
        } else {
            let p = self.period as f64;
            self.avg_gain = (self.avg_gain * (p - 1.0) + gain) / p;
            self.avg_loss = (self.avg_loss * (p - 1.0) + loss) / p;

            let rs = if self.avg_loss == 0.0 { 100.0 } else { self.avg_gain / self.avg_loss };
            self.value = Some(100.0 - (100.0 / (1.0 + rs)));
        }

        self.value
    }
}

#[derive(Debug, Clone)]
struct MacdState {
    ema_fast: EmaState,
    ema_slow: EmaState,
    ema_signal: EmaState,
    macd_line: Option<f64>,
    signal_line: Option<f64>,
    histogram: Option<f64>,
}

impl MacdState {
    fn new(fast: usize, slow: usize, signal: usize) -> Self {
        Self {
            ema_fast: EmaState::new(fast),
            ema_slow: EmaState::new(slow),
            ema_signal: EmaState::new(signal),
            macd_line: None,
            signal_line: None,
            histogram: None,
        }
    }

    fn update(&mut self, close: f64) {
        let ef = self.ema_fast.update(close);
        let es = self.ema_slow.update(close);

        match (ef, es) {
            (Some(f), Some(s)) => {
                let macd_val = f - s;
                self.macd_line = Some(macd_val);
                let sig = self.ema_signal.update(macd_val);
                self.signal_line = sig;
                self.histogram = sig.map(|s_val| macd_val - s_val);
            }
            _ => {
                self.macd_line = None;
                self.signal_line = None;
                self.histogram = None;
            }
        }
    }
}

#[derive(Debug, Clone)]
struct BbState {
    period: usize,
    std_dev_mult: f64,
    window: VecDeque<f64>,
    upper: Option<f64>,
    lower: Option<f64>,
    mid: Option<f64>,
}

impl BbState {
    fn new(period: usize, std_dev_mult: f64) -> Self {
        Self {
            period,
            std_dev_mult,
            window: VecDeque::with_capacity(period + 1),
            upper: None,
            lower: None,
            mid: None,
        }
    }

    fn update(&mut self, close: f64) {
        self.window.push_back(close);
        if self.window.len() > self.period {
            self.window.pop_front();
        }

        if self.window.len() < self.period || self.period < 2 {
            self.upper = None;
            self.lower = None;
            self.mid = None;
            return;
        }

        let sum: f64 = self.window.iter().sum();
        let mean = sum / self.period as f64;
        let variance_sum: f64 = self.window.iter().map(|&x| (x - mean).powi(2)).sum();
        let std_dev = (variance_sum / (self.period as f64 - 1.0)).sqrt();

        self.mid = Some(mean);
        self.upper = Some(mean + self.std_dev_mult * std_dev);
        self.lower = Some(mean - self.std_dev_mult * std_dev);
    }
}

#[derive(Debug, Clone)]
struct SlidingMean {
    period: usize,
    window: VecDeque<f64>,
    sum: f64,
}

impl SlidingMean {
    fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period + 1),
            sum: 0.0,
        }
    }

    fn push(&mut self, val: f64) -> Option<f64> {
        self.window.push_back(val);
        self.sum += val;

        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().unwrap_or(0.0);
        }

        if self.window.len() == self.period {
            Some(self.sum / self.period as f64)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
struct AdxState {
    prev_candle: Option<Candle>,
    tr_mean: SlidingMean,
    plus_dm_mean: SlidingMean,
    minus_dm_mean: SlidingMean,
    dx_mean: SlidingMean,
    candle_count: usize,
    value: Option<f64>,
}

impl AdxState {
    fn new(period: usize) -> Self {
        Self {
            prev_candle: None,
            tr_mean: SlidingMean::new(period),
            plus_dm_mean: SlidingMean::new(period),
            minus_dm_mean: SlidingMean::new(period),
            dx_mean: SlidingMean::new(period),
            candle_count: 0,
            value: None,
        }
    }

    fn update(&mut self, candle: &Candle) -> Option<f64> {
        self.candle_count += 1;

        let prev = match &self.prev_candle {
            Some(p) => p.clone(),
            None => {
                self.prev_candle = Some(candle.clone());
                self.tr_mean.push(0.0);
                self.plus_dm_mean.push(0.0);
                self.minus_dm_mean.push(0.0);
                self.value = None;
                return None;
            }
        };
        self.prev_candle = Some(candle.clone());

        let hl = candle.high - candle.low;
        let hc = (candle.high - prev.close).abs();
        let lc = (candle.low - prev.close).abs();
        let true_range = hl.max(hc).max(lc);

        let up_move = candle.high - prev.high;
        let down_move = prev.low - candle.low;
        let p_dm = if up_move > down_move && up_move > 0.0 { up_move } else { 0.0 };
        let m_dm = if down_move > up_move && down_move > 0.0 { down_move } else { 0.0 };

        let smooth_tr = self.tr_mean.push(true_range);
        let smooth_pdm = self.plus_dm_mean.push(p_dm);
        let smooth_mdm = self.minus_dm_mean.push(m_dm);

        if let (Some(atr), Some(pdm), Some(mdm)) = (smooth_tr, smooth_pdm, smooth_mdm) {
            if atr == 0.0 {
                self.value = self.dx_mean.push(0.0);
                return self.value;
            }
            let p_di = 100.0 * (pdm / atr);
            let m_di = 100.0 * (mdm / atr);
            let di_sum = p_di + m_di;
            let dx = if di_sum == 0.0 { 0.0 } else { 100.0 * (p_di - m_di).abs() / di_sum };

            self.value = self.dx_mean.push(dx);
        } else {
            self.value = None;
        }

        self.value
    }
}

#[derive(Debug, Clone)]
struct StochRsiState {
    rsi_period: usize,
    rsi_buffer: VecDeque<Option<f64>>,
    stoch_k_buffer: VecDeque<Option<f64>>,
    stoch_k_100_buffer: VecDeque<Option<f64>>,
    raw: Option<f64>,
    k: Option<f64>,
    d: Option<f64>,
}

impl StochRsiState {
    fn new(period: usize) -> Self {
        Self {
            rsi_period: period,
            rsi_buffer: VecDeque::with_capacity(period + 1),
            stoch_k_buffer: VecDeque::with_capacity(4),
            stoch_k_100_buffer: VecDeque::with_capacity(4),
            raw: None,
            k: None,
            d: None,
        }
    }

    fn update(&mut self, rsi: Option<f64>) {
        self.rsi_buffer.push_back(rsi);
        if self.rsi_buffer.len() > self.rsi_period {
            self.rsi_buffer.pop_front();
        }

        if self.rsi_buffer.len() < self.rsi_period {
            self.raw = None;
            self.k = None;
            self.d = None;
            return;
        }

        let mut min_rsi = f64::MAX;
        let mut max_rsi = f64::MIN;
        let mut all_valid = true;
        let mut current_val = 0.0;

        for (j, v) in self.rsi_buffer.iter().rev().enumerate() {
            if let Some(r) = v {
                if j == 0 { current_val = *r; }
                if *r < min_rsi { min_rsi = *r; }
                if *r > max_rsi { max_rsi = *r; }
            } else {
                all_valid = false;
                break;
            }
        }

        let stoch_raw = if all_valid && max_rsi != min_rsi {
            Some((current_val - min_rsi) / (max_rsi - min_rsi))
        } else if all_valid {
            Some(0.0)
        } else {
            None
        };
        self.raw = stoch_raw;

        self.stoch_k_buffer.push_back(stoch_raw);
        if self.stoch_k_buffer.len() > 3 {
            self.stoch_k_buffer.pop_front();
        }
        let stoch_k = sma_opt_deque(&self.stoch_k_buffer, 3);
        let stoch_k_100 = stoch_k.map(|v| v * 100.0);
        self.k = stoch_k_100;

        self.stoch_k_100_buffer.push_back(stoch_k_100);
        if self.stoch_k_100_buffer.len() > 3 {
            self.stoch_k_100_buffer.pop_front();
        }
        self.d = sma_opt_deque(&self.stoch_k_100_buffer, 3);
    }
}

fn sma_opt_deque(buf: &VecDeque<Option<f64>>, period: usize) -> Option<f64> {
    if buf.len() < period {
        return None;
    }
    let mut sum = 0.0;
    for v in buf.iter().rev().take(period) {
        match v {
            Some(x) => sum += x,
            None => return None,
        }
    }
    Some(sum / period as f64)
}

#[derive(Debug, Clone)]
pub struct StreamingIndicators {
    candle_count: usize,

    ema_fast: EmaState,
    ema_mid: EmaState,
    ema_slow: EmaState,
    ema_trend: EmaState,

    rsi: RsiState,
    macd: MacdState,
    bb: BbState,
    adx: AdxState,

    vwap_cum_vol: f64,
    vwap_cum_vol_price: f64,
    vwap: Option<f64>,

    stoch_rsi: StochRsiState,
}

impl StreamingIndicators {
    pub fn new(periods: IndicatorPeriods) -> Self {
        Self {
            ema_fast: EmaState::new(periods.ema_fast),
            ema_mid: EmaState::new(periods.ema_mid),
            ema_slow: EmaState::new(periods.ema_slow),
            ema_trend: EmaState::new(periods.ema_trend),
            rsi: RsiState::new(periods.rsi),
            macd: MacdState::new(periods.macd_fast, periods.macd_slow, periods.macd_signal),
            bb: BbState::new(periods.bb, periods.bb_std_dev),
            adx: AdxState::new(periods.adx),
            vwap_cum_vol: 0.0,
            vwap_cum_vol_price: 0.0,
            vwap: None,
            stoch_rsi: StochRsiState::new(periods.stoch_rsi),
            candle_count: 0,
        }
    }

    pub fn cold_start(candles: &[Candle], periods: &IndicatorPeriods) -> Self {
        let mut s = Self::new(periods.clone());
        for c in candles {
            s.update(c);
        }
        s
    }

    pub fn update(&mut self, candle: &Candle) -> Indicators {
        self.candle_count += 1;
        let close = candle.close;

        let ema_9 = self.ema_fast.update(close);
        let ema_20 = self.ema_mid.update(close);
        let ema_50 = self.ema_slow.update(close);
        let ema_200 = self.ema_trend.update(close);

        let rsi = self.rsi.update(close);

        self.macd.update(close);

        self.bb.update(close);

        let adx = self.adx.update(candle);

        let typ_price = (candle.high + candle.low + candle.close) / 3.0;
        self.vwap_cum_vol += candle.volume;
        self.vwap_cum_vol_price += candle.volume * typ_price;
        self.vwap = if self.vwap_cum_vol == 0.0 {
            Some(typ_price)
        } else {
            Some(self.vwap_cum_vol_price / self.vwap_cum_vol)
        };

        self.stoch_rsi.update(rsi);

        Indicators {
            ema_9,
            ema_20,
            ema_50,
            ema_200,
            rsi,
            macd: self.macd.macd_line,
            macd_signal: self.macd.signal_line,
            macd_hist: self.macd.histogram,
            bb_upper: self.bb.upper,
            bb_lower: self.bb.lower,
            bb_mid: self.bb.mid,
            adx,
            vwap: self.vwap,
            stoch_rsi: self.stoch_rsi.raw,
            stoch_k: self.stoch_rsi.k,
            stoch_d: self.stoch_rsi.d,
        }
    }

    pub fn candle_count(&self) -> usize {
        self.candle_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use titan_types::Candle;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn ema_output_in_range(
            values in proptest::collection::vec(0.01f64..10_000.0, 1..200),
            period in 2usize..50,
        ) {
            let ema = calculate_ema_series(&values, period);
            let min_val = values.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_val = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

            for v in ema.iter().flatten() {
                prop_assert!(
                    *v >= min_val - 1e-9 && *v <= max_val + 1e-9,
                    "EMA {} outside input range [{}, {}]", v, min_val, max_val,
                );
            }
        }

        #[test]
        fn rsi_output_in_0_100(
            values in proptest::collection::vec(0.01f64..10_000.0, 20..200),
        ) {
            let rsi = calculate_rsi_series(&values, 14);
            for v in rsi.iter().flatten() {
                prop_assert!(
                    *v >= -1e-9 && *v <= 100.0 + 1e-9,
                    "RSI {} outside [0, 100]", v,
                );
            }
        }

        #[test]
        fn atr_non_negative(
            candle_data in proptest::collection::vec(
                (1.0f64..10_000.0, 1.0f64..10_000.0, 0.01f64..1.0, 0.0f64..1_000_000.0),
                5..100,
            ),
        ) {
            let candles: Vec<Candle> = candle_data.iter().map(|(base, spread, close_frac, vol)| {
                let high = base + spread;
                let low = *base;
                let close = low + (high - low) * close_frac;
                Candle {
                    time: 0,
                    open: close,
                    high,
                    low,
                    close,
                    volume: *vol,
                }
            }).collect();

            let atr = calculate_atr_series(&candles, 14);
            for v in atr.iter().flatten() {
                prop_assert!(*v >= -1e-9, "ATR {} is negative", v);
            }
        }
    }

    #[test]
    fn test_ema_empty_input() {
        let result = calculate_ema_series(&[], 10);
        assert!(result.is_empty());
    }

    #[test]
    fn test_ema_single_element() {
        let result = calculate_ema_series(&[100.0], 10);
        assert_eq!(result.len(), 1);
        assert!(result[0].is_some());
    }

    #[test]
    fn test_ema_known_values() {
        let prices = vec![22.27, 22.19, 22.08, 22.17, 22.18, 22.13, 22.23, 22.43, 22.24, 22.29,
                          22.15, 22.39, 22.38, 22.61, 23.36, 24.05, 23.75, 23.83, 23.95, 23.63];
        let result = calculate_ema_series(&prices, 10);
        assert_eq!(result.len(), 20);
        assert!(result[9].is_some());
        let ema_10 = result[9].unwrap();
        assert!(ema_10 > 22.0 && ema_10 < 23.0, "EMA(10) at index 9 should be near 22.x, got {}", ema_10);
    }

    #[test]
    fn test_rsi_overbought() {
        let mut prices = vec![50.0];
        for i in 1..30 {
            prices.push(50.0 + i as f64 * 2.0);
        }
        let result = calculate_rsi_series(&prices, 14);
        let last_rsi = result.last().unwrap().unwrap();
        assert!(last_rsi > 70.0, "trending up should have RSI > 70, got {}", last_rsi);
    }

    #[test]
    fn test_rsi_oversold() {
        let mut prices = vec![100.0];
        for i in 1..30 {
            prices.push(100.0 - i as f64 * 2.0);
        }
        let result = calculate_rsi_series(&prices, 14);
        let last_rsi = result.last().unwrap().unwrap();
        assert!(last_rsi < 30.0, "trending down should have RSI < 30, got {}", last_rsi);
    }

    #[test]
    fn test_atr_flat_candles() {
        let candles: Vec<Candle> = (0..20).map(|i| Candle {
            time: i, open: 100.0, high: 100.0, low: 100.0, close: 100.0, volume: 1000.0,
        }).collect();
        let atr = calculate_atr(&candles, 14);
        assert!(atr.is_some());
        assert!((atr.unwrap()).abs() < 0.01);
    }

    #[test]
    fn test_atr_volatile_candles() {
        let candles: Vec<Candle> = (0..20).map(|i| Candle {
            time: i, open: 100.0, high: 110.0, low: 90.0, close: 100.0, volume: 1000.0,
        }).collect();
        let atr = calculate_atr(&candles, 14);
        assert!(atr.is_some());
        assert!(atr.unwrap() > 5.0);
    }

    #[test]
    fn test_bb_constant_price() {
        let prices: Vec<f64> = vec![100.0; 30];
        let (mid, upper, lower) = calculate_bb_series(&prices, 20, 2.0);
        if let (Some(m), Some(u), Some(l)) = (mid.last().unwrap(), upper.last().unwrap(), lower.last().unwrap()) {
            assert!((m - 100.0).abs() < 0.01);
            assert!((u - 100.0).abs() < 0.01, "upper band should equal mid for constant price");
            assert!((l - 100.0).abs() < 0.01, "lower band should equal mid for constant price");
        }
    }

    #[test]
    fn test_adx_trending() {
        let candles: Vec<Candle> = (0..50).map(|i| {
            let base = 100.0 + i as f64 * 2.0;
            Candle { time: i, open: base, high: base + 1.0, low: base - 0.5, close: base + 0.5, volume: 1000.0 }
        }).collect();
        let result = calculate_adx_series(&candles, 14);
        let last_adx = result.iter().rev().find_map(|v| *v);
        assert!(last_adx.is_some());
        assert!(last_adx.unwrap() > 20.0, "strong trend should have ADX > 20");
    }

    #[test]
    fn test_atr_series_length() {
        let candles: Vec<Candle> = (0..30).map(|i| Candle {
            time: i, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0,
        }).collect();
        let series = calculate_atr_series(&candles, 14);
        assert_eq!(series.len(), 30);
    }

    #[test]
    fn test_vwap_basic() {
        let candles: Vec<Candle> = (0..10).map(|i| Candle {
            time: i, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0,
        }).collect();
        let result = calculate_vwap_series(&candles);
        assert_eq!(result.len(), 10);
        let last = result.last().unwrap().unwrap();
        assert!((last - 100.0).abs() < 1.0);
    }

    // ═══════════════════════════════════════════════════════════════
    //  StreamingIndicators tests — proptest + unit
    // ═══════════════════════════════════════════════════════════════

    fn make_candles(prices: &[(f64, f64, f64, f64)]) -> Vec<Candle> {
        prices.iter().enumerate().map(|(i, &(o, h, l, c))| Candle {
            time: i as i64,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 1000.0 + (i as f64) * 10.0,
        }).collect()
    }

    fn assert_opt_eq(label: &str, a: Option<f64>, b: Option<f64>, eps: f64) {
        match (a, b) {
            (Some(va), Some(vb)) => {
                assert!(
                    (va - vb).abs() < eps,
                    "{}: streaming={} vs batch={}, diff={}",
                    label, va, vb, (va - vb).abs()
                );
            }
            (None, None) => {}
            _ => panic!("{}: mismatch Some/None: streaming={:?} vs batch={:?}", label, a, b),
        }
    }

    fn random_candle_strategy() -> impl Strategy<Value = Vec<Candle>> {
        proptest::collection::vec(
            (1.0f64..500.0, 0.1f64..20.0, 0.01f64..1.0, 100.0f64..100_000.0),
            50..201,
        ).prop_map(|data| {
            data.iter().enumerate().map(|(i, &(base, spread, close_frac, vol))| {
                let high = base + spread;
                let low = base;
                let close = low + (high - low) * close_frac;
                let open = low + (high - low) * 0.5;
                Candle { time: i as i64, open, high, low, close, volume: vol }
            }).collect()
        })
    }

    proptest! {
        #[test]
        fn streaming_ema_matches_batch(candles in random_candle_strategy()) {
            let periods = IndicatorPeriods::default();
            let batch = calculate_all_with_periods(&candles, &periods);

            let mut s = StreamingIndicators::new(periods);
            let mut last = None;
            for c in &candles {
                last = Some(s.update(c));
            }
            let streaming = last.unwrap();

            let eps = 1e-10;
            assert_opt_eq("ema_9", streaming.ema_9, batch.ema_9, eps);
            assert_opt_eq("ema_20", streaming.ema_20, batch.ema_20, eps);
            assert_opt_eq("ema_50", streaming.ema_50, batch.ema_50, eps);
            assert_opt_eq("ema_200", streaming.ema_200, batch.ema_200, eps);
        }

        #[test]
        fn streaming_rsi_matches_batch(candles in random_candle_strategy()) {
            let periods = IndicatorPeriods::default();
            let batch = calculate_all_with_periods(&candles, &periods);

            let mut s = StreamingIndicators::new(periods);
            let mut last = None;
            for c in &candles {
                last = Some(s.update(c));
            }
            let streaming = last.unwrap();

            assert_opt_eq("rsi", streaming.rsi, batch.rsi, 1e-10);
        }

        #[test]
        fn streaming_macd_bb_adx_matches_batch(candles in random_candle_strategy()) {
            let periods = IndicatorPeriods::default();
            let batch = calculate_all_with_periods(&candles, &periods);

            let mut s = StreamingIndicators::new(periods);
            let mut last = None;
            for c in &candles {
                last = Some(s.update(c));
            }
            let streaming = last.unwrap();

            let eps = 1e-10;
            assert_opt_eq("macd", streaming.macd, batch.macd, eps);
            assert_opt_eq("macd_signal", streaming.macd_signal, batch.macd_signal, eps);
            assert_opt_eq("macd_hist", streaming.macd_hist, batch.macd_hist, eps);
            assert_opt_eq("bb_upper", streaming.bb_upper, batch.bb_upper, eps);
            assert_opt_eq("bb_lower", streaming.bb_lower, batch.bb_lower, eps);
            assert_opt_eq("bb_mid", streaming.bb_mid, batch.bb_mid, eps);
            assert_opt_eq("adx", streaming.adx, batch.adx, eps);
            assert_opt_eq("vwap", streaming.vwap, batch.vwap, eps);
            assert_opt_eq("stoch_rsi", streaming.stoch_rsi, batch.stoch_rsi, eps);
            assert_opt_eq("stoch_k", streaming.stoch_k, batch.stoch_k, eps);
            assert_opt_eq("stoch_d", streaming.stoch_d, batch.stoch_d, eps);
        }
    }

    #[test]
    fn cold_start_equals_batch_full() {
        let prices: Vec<(f64, f64, f64, f64)> = (0..200).map(|i| {
            let base = 100.0 + (i as f64) * 0.5;
            (base - 0.2, base + 1.5, base - 0.8, base + 0.3)
        }).collect();
        let candles = make_candles(&prices);
        let periods = IndicatorPeriods::default();

        let batch = calculate_all_with_periods(&candles, &periods);
        let s = StreamingIndicators::cold_start(&candles, &periods);

        let mut s2 = StreamingIndicators::new(periods.clone());
        let mut streaming = s2.update(&candles[0]);
        for c in &candles[1..] {
            streaming = s2.update(c);
        }

        let eps = 1e-10;
        assert_opt_eq("ema_9", streaming.ema_9, batch.ema_9, eps);
        assert_opt_eq("ema_20", streaming.ema_20, batch.ema_20, eps);
        assert_opt_eq("ema_50", streaming.ema_50, batch.ema_50, eps);
        assert_opt_eq("ema_200", streaming.ema_200, batch.ema_200, eps);
        assert_opt_eq("rsi", streaming.rsi, batch.rsi, eps);
        assert_opt_eq("macd", streaming.macd, batch.macd, eps);
        assert_opt_eq("macd_signal", streaming.macd_signal, batch.macd_signal, eps);
        assert_opt_eq("macd_hist", streaming.macd_hist, batch.macd_hist, eps);
        assert_opt_eq("bb_upper", streaming.bb_upper, batch.bb_upper, eps);
        assert_opt_eq("bb_lower", streaming.bb_lower, batch.bb_lower, eps);
        assert_opt_eq("bb_mid", streaming.bb_mid, batch.bb_mid, eps);
        assert_opt_eq("adx", streaming.adx, batch.adx, eps);
        assert_opt_eq("vwap", streaming.vwap, batch.vwap, eps);
        assert_opt_eq("stoch_rsi", streaming.stoch_rsi, batch.stoch_rsi, eps);
        assert_opt_eq("stoch_k", streaming.stoch_k, batch.stoch_k, eps);
        assert_opt_eq("stoch_d", streaming.stoch_d, batch.stoch_d, eps);
        assert_eq!(s.candle_count(), 200);
    }

    #[test]
    fn cold_start_then_one_update() {
        let prices: Vec<(f64, f64, f64, f64)> = (0..201).map(|i| {
            let base = 50.0 + (i as f64) * 0.3;
            (base, base + 2.0, base - 1.0, base + 0.5)
        }).collect();
        let all_candles = make_candles(&prices);
        let periods = IndicatorPeriods::default();

        let batch_201 = calculate_all_with_periods(&all_candles, &periods);

        let mut s = StreamingIndicators::cold_start(&all_candles[..200], &periods);
        let streaming = s.update(&all_candles[200]);

        let eps = 1e-10;
        assert_opt_eq("ema_9", streaming.ema_9, batch_201.ema_9, eps);
        assert_opt_eq("rsi", streaming.rsi, batch_201.rsi, eps);
        assert_opt_eq("macd", streaming.macd, batch_201.macd, eps);
        assert_opt_eq("bb_mid", streaming.bb_mid, batch_201.bb_mid, eps);
        assert_opt_eq("adx", streaming.adx, batch_201.adx, eps);
        assert_opt_eq("vwap", streaming.vwap, batch_201.vwap, eps);
        assert_eq!(s.candle_count(), 201);
    }

    #[test]
    fn incremental_10_updates() {
        let prices: Vec<(f64, f64, f64, f64)> = (0..200).map(|i| {
            let base = 80.0 + (i as f64).sin() * 10.0;
            (base - 0.5, base + 3.0, base - 2.0, base + 1.0)
        }).collect();
        let all_candles = make_candles(&prices);
        let periods = IndicatorPeriods::default();

        let batch_200 = calculate_all_with_periods(&all_candles, &periods);

        let mut s = StreamingIndicators::cold_start(&all_candles[..190], &periods);
        let mut last = s.update(&all_candles[190]);
        for c in &all_candles[191..] {
            last = s.update(c);
        }

        let eps = 1e-10;
        assert_opt_eq("ema_9", last.ema_9, batch_200.ema_9, eps);
        assert_opt_eq("ema_20", last.ema_20, batch_200.ema_20, eps);
        assert_opt_eq("rsi", last.rsi, batch_200.rsi, eps);
        assert_opt_eq("macd_hist", last.macd_hist, batch_200.macd_hist, eps);
        assert_opt_eq("bb_upper", last.bb_upper, batch_200.bb_upper, eps);
        assert_opt_eq("adx", last.adx, batch_200.adx, eps);
        assert_opt_eq("stoch_k", last.stoch_k, batch_200.stoch_k, eps);
        assert_eq!(s.candle_count(), 200);
    }

    #[test]
    fn streaming_empty_candles() {
        let periods = IndicatorPeriods::default();
        let s = StreamingIndicators::cold_start(&[], &periods);
        assert_eq!(s.candle_count(), 0);
    }

    #[test]
    fn streaming_single_candle() {
        let candle = Candle { time: 0, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0 };
        let periods = IndicatorPeriods::default();
        let mut s = StreamingIndicators::new(periods);
        let ind = s.update(&candle);

        assert!(ind.ema_9.is_some());
        assert_eq!(ind.ema_9.unwrap(), 100.0);
        assert!(ind.rsi.is_none());
        assert!(ind.bb_mid.is_none());
        assert!(ind.adx.is_none());
        assert!(ind.vwap.is_some());
        assert_eq!(s.candle_count(), 1);
    }

    #[test]
    fn streaming_rsi_not_available_until_period_plus_one() {
        let periods = IndicatorPeriods { rsi: 14, ..IndicatorPeriods::default() };
        let mut s = StreamingIndicators::new(periods);

        for i in 0..15 {
            let c = Candle { time: i, open: 100.0, high: 102.0, low: 98.0, close: 100.0 + (i as f64) * 0.1, volume: 100.0 };
            let ind = s.update(&c);
            if i < 14 {
                assert!(ind.rsi.is_none(), "RSI should be None at candle {}", i);
            }
        }
        let c15 = Candle { time: 15, open: 101.0, high: 103.0, low: 99.0, close: 101.5, volume: 100.0 };
        let ind = s.update(&c15);
        assert!(ind.rsi.is_some(), "RSI should be available at candle 15 (16th)");
    }

    #[test]
    fn streaming_bb_not_available_until_period() {
        let periods = IndicatorPeriods { bb: 20, ..IndicatorPeriods::default() };
        let mut s = StreamingIndicators::new(periods);

        for i in 0..19 {
            let c = Candle { time: i, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0 };
            let ind = s.update(&c);
            assert!(ind.bb_mid.is_none(), "BB should be None at candle {}", i);
        }
        let c20 = Candle { time: 19, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0 };
        let ind = s.update(&c20);
        assert!(ind.bb_mid.is_some(), "BB should be available at candle 19 (20th)");
    }

    #[test]
    fn streaming_adx_not_available_until_double_period() {
        let periods = IndicatorPeriods { adx: 14, ..IndicatorPeriods::default() };
        let mut s = StreamingIndicators::new(periods);

        // ADX needs: period values in TR/DM means (first valid at index period-1=13),
        // then period DX values in DX mean (first ADX at index 13+period-1=26).
        for i in 0..26 {
            let base = 100.0 + (i as f64) * 2.0;
            let c = Candle { time: i, open: base, high: base + 1.0, low: base - 0.5, close: base + 0.5, volume: 1000.0 };
            let ind = s.update(&c);
            assert!(ind.adx.is_none(), "ADX should be None at candle {} (0-indexed)", i);
        }
        let c27 = Candle { time: 26, open: 153.0, high: 154.0, low: 152.5, close: 153.5, volume: 1000.0 };
        let ind = s.update(&c27);
        assert!(ind.adx.is_some(), "ADX should be available at candle 26 (27th, = 2*period-1)");
    }

    #[test]
    fn streaming_vwap_always_available() {
        let periods = IndicatorPeriods::default();
        let mut s = StreamingIndicators::new(periods);

        for i in 0..5 {
            let c = Candle { time: i, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0 };
            let ind = s.update(&c);
            assert!(ind.vwap.is_some(), "VWAP should always be Some, failed at candle {}", i);
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  calculate_atr_sma_last / calculate_ema_last — parity tests
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn atr_sma_last_parity() {
        let candles: Vec<Candle> = (0..20).map(|i| {
            let base = 100.0 + (i as f64) * 0.5;
            Candle {
                time: i,
                open: base,
                high: base + 3.0 + (i as f64) * 0.1,
                low: base - 2.0 - (i as f64) * 0.05,
                close: base + 1.0,
                volume: 1000.0,
            }
        }).collect();

        let period = 14;
        let result = calculate_atr_sma_last(&candles, period).unwrap();

        let start = candles.len() - period;
        let mut expected_sum = 0.0;
        for idx in start..candles.len() {
            let bar = &candles[idx];
            let prev_close = candles[idx - 1].close;
            let tr = (bar.high - bar.low)
                .max((bar.high - prev_close).abs())
                .max((bar.low - prev_close).abs());
            expected_sum += tr;
        }
        let expected = expected_sum / period as f64;

        assert!(
            (result - expected).abs() < 1e-12,
            "SMA ATR mismatch: got {} expected {}",
            result, expected,
        );
    }

    #[test]
    fn atr_sma_last_too_few_candles() {
        let candles: Vec<Candle> = (0..5).map(|i| Candle {
            time: i, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0,
        }).collect();
        assert!(calculate_atr_sma_last(&candles, 14).is_none());
        assert!(calculate_atr_sma_last(&candles, 5).is_none());
        assert!(calculate_atr_sma_last(&candles, 4).is_some());
    }

    #[test]
    fn atr_sma_last_period_zero() {
        let candles: Vec<Candle> = (0..10).map(|i| Candle {
            time: i, open: 100.0, high: 105.0, low: 95.0, close: 100.0, volume: 1000.0,
        }).collect();
        assert!(calculate_atr_sma_last(&candles, 0).is_none());
    }

    #[test]
    fn ema_last_parity() {
        let values = vec![22.27, 22.19, 22.08, 22.17, 22.18, 22.13, 22.23, 22.43, 22.24, 22.29];
        let period = 5;
        let result = calculate_ema_last(&values, period);

        let k = 2.0 / (period as f64 + 1.0);
        let mut expected = values[0];
        for &p in &values[1..] {
            expected = p * k + expected * (1.0 - k);
        }

        assert!(
            (result - expected).abs() < 1e-12,
            "EMA last mismatch: got {} expected {}",
            result, expected,
        );
    }

    #[test]
    fn ema_last_empty_returns_zero() {
        assert_eq!(calculate_ema_last(&[], 10), 0.0);
    }

    #[test]
    fn ema_last_single_value() {
        assert_eq!(calculate_ema_last(&[42.0], 10), 42.0);
    }

    #[test]
    fn ema_last_period_zero() {
        assert_eq!(calculate_ema_last(&[1.0, 2.0, 3.0], 0), 0.0);
    }
}
