//! MERIDIAN order application (R6, 2026-09-27) — runs the planner's actions
//! against the broker with two guarantees the pre-R6 executor lacked:
//!
//!   * **Re-plan after any cancel** (E1, chat 9 P1.2). "No longer open" can
//!     mean *cancelled or filled*; quantities computed before the cancel are
//!     stale either way. `apply` stops with [`Outcome::Replan`] and the next
//!     poll re-plans from broker truth.
//!   * **Phase deadline on every submit** (E2, P1.1/P1.5). A plan valid when
//!     built is not valid when sent if a confirmation wait crossed 15:50 or
//!     16:00. Every broker call is bounded by the phase deadline on the Tokio
//!     clock, checked before *and after* the response; the wait budget never
//!     exceeds the time left in the phase.
//!
//! The broker is abstracted by [`BrokerApi`] so the transition tests in
//! `exec_tests.rs` (`apply_e1_e2_tests`, `claude_injection_tests`) run against
//! a scripted fake. Design and first implementation: GPT-6 Astra (red team
//! round 4), compiled and tested unchanged; split out of `exec.rs` for R2.

use super::broker::{Broker, BrokerOrder, OrderSpec};
use super::exec::{whole, Action};

#[allow(async_fn_in_trait)]
pub trait BrokerApi {
    async fn position(&self, symbol: &str) -> Result<f64, String>;

    async fn open_orders(
        &self,
        symbol: &str,
    ) -> Result<Vec<BrokerOrder>, String>;

    async fn submit(&self, spec: &OrderSpec) -> Result<String, String>;

    async fn cancel(&self, order_id: &str) -> Result<(), String>;
}

impl BrokerApi for Broker<'_> {
    async fn position(&self, symbol: &str) -> Result<f64, String> {
        Broker::position(self, symbol).await
    }

    async fn open_orders(
        &self,
        symbol: &str,
    ) -> Result<Vec<BrokerOrder>, String> {
        Broker::open_orders(self, symbol).await
    }

    async fn submit(&self, spec: &OrderSpec) -> Result<String, String> {
        Broker::submit(self, spec).await
    }

    async fn cancel(&self, order_id: &str) -> Result<(), String> {
        Broker::cancel(self, order_id).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    Replan,
}

const CANCEL_CONFIRM_SECS: u64 = 10;
const COVER_CONFIRM_SECS: u64 = 20;

fn apply_limit_error(
    label: &str,
    stop: tokio::time::Instant,
    phase_deadline: tokio::time::Instant,
) -> String {
    if stop == phase_deadline {
        format!(
            "{label}: phase deadline expired; \
             re-plan from fresh broker state"
        )
    } else {
        format!(
            "{label}: confirmation timeout; \
             re-plan from fresh broker state"
        )
    }
}

/// Bound an operation by an absolute Tokio deadline.
///
/// The future is not polled if the deadline has already expired.
/// A response received at or after the deadline does not authorize
/// execution of any subsequent action.
async fn apply_until<T, F>(
    stop: tokio::time::Instant,
    phase_deadline: tokio::time::Instant,
    label: &str,
    future: F,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    if tokio::time::Instant::now() >= stop {
        return Err(apply_limit_error(label, stop, phase_deadline));
    }

    let result = tokio::time::timeout_at(stop, future).await;

    if tokio::time::Instant::now() >= stop {
        return Err(apply_limit_error(label, stop, phase_deadline));
    }

    match result {
        Ok(result) => result,
        Err(_) => Err(apply_limit_error(label, stop, phase_deadline)),
    }
}

/// Sleep for at most one second, without extending the operation's
/// absolute budget. Reaching the deadline is an error, not permission
/// to perform one final broker request.
async fn apply_wait_tick(
    stop: tokio::time::Instant,
    phase_deadline: tokio::time::Instant,
    label: &str,
) -> Result<(), String> {
    let now = tokio::time::Instant::now();

    if now >= stop {
        return Err(apply_limit_error(label, stop, phase_deadline));
    }

    let wake =
        (now + std::time::Duration::from_secs(1)).min(stop);

    tokio::time::sleep_until(wake).await;

    if tokio::time::Instant::now() >= stop {
        return Err(apply_limit_error(label, stop, phase_deadline));
    }

    Ok(())
}

async fn confirm_cancelled<B: BrokerApi + ?Sized>(
    broker: &B,
    symbol: &str,
    ids: &[String],
    phase_deadline: tokio::time::Instant,
) -> Result<(), String> {
    let stop = (
        tokio::time::Instant::now()
            + std::time::Duration::from_secs(CANCEL_CONFIRM_SECS)
    )
        .min(phase_deadline);

    let label = format!("{symbol}: cancel confirmation");

    loop {
        apply_wait_tick(stop, phase_deadline, &label).await?;

        let open = apply_until(
            stop,
            phase_deadline,
            &label,
            broker.open_orders(symbol),
        )
        .await?;

        if !open.iter().any(|order| ids.contains(&order.id)) {
            return Ok(());
        }
    }
}

async fn confirm_position<B: BrokerApi + ?Sized>(
    broker: &B,
    symbol: &str,
    qty: f64,
    phase_deadline: tokio::time::Instant,
) -> Result<f64, String> {
    let stop = (
        tokio::time::Instant::now()
            + std::time::Duration::from_secs(COVER_CONFIRM_SECS)
    )
        .min(phase_deadline);

    let label =
        format!("{symbol}: cover confirmation (wanted {qty:+.0})");

    loop {
        apply_wait_tick(stop, phase_deadline, &label).await?;

        let pos = apply_until(
            stop,
            phase_deadline,
            &label,
            broker.position(symbol),
        )
        .await?;

        // Preserve the existing position comparison.
        if whole(pos) == whole(qty) {
            return Ok(pos);
        }
    }
}

/// Execute a batch until completion, an error, or its first cancellation.
///
/// After a cancellation, no later action from this batch is executed:
/// an intervening fill may have invalidated every precomputed quantity.
///
/// `deadline` is the absolute end of the current execution phase.
/// It must not be renewed between actions or between symbols that
/// share the same phase deadline.
pub async fn apply<B: BrokerApi + ?Sized>(
    broker: &B,
    actions: &[Action],
    deadline: std::time::Instant,
) -> Result<(Vec<String>, Outcome), String> {
    let phase_deadline = tokio::time::Instant::from_std(deadline);
    let mut done = Vec::new();

    for action in actions {
        match action {
            Action::Cancel {
                id,
                client_order_id,
                symbol,
            } => {
                let label =
                    format!("{symbol}: cancel {client_order_id}");

                apply_until(
                    phase_deadline,
                    phase_deadline,
                    &label,
                    broker.cancel(id),
                )
                .await?;

                done.push(format!(
                    "cancel requested {client_order_id}"
                ));

                confirm_cancelled(
                    broker,
                    symbol,
                    std::slice::from_ref(id),
                    phase_deadline,
                )
                .await?;

                // "No longer open" can mean canceled OR filled.
                // Do not infer that the position is unchanged.
                done.push(format!(
                    "{client_order_id} no longer open; re-plan"
                ));

                return Ok((done, Outcome::Replan));
            }

            Action::Submit(spec) => {
                let label = format!(
                    "{}: submit {}",
                    spec.symbol,
                    spec.client_order_id
                );

                // apply_until checks the deadline before polling
                // broker.submit, and again after its response.
                let id = apply_until(
                    phase_deadline,
                    phase_deadline,
                    &label,
                    broker.submit(spec),
                )
                .await?;

                done.push(format!(
                    "submitted {} {:+.0} {} -> {id}",
                    spec.symbol,
                    spec.qty,
                    spec.tif.as_str()
                ));
            }

            Action::AwaitPosition { symbol, qty } => {
                let pos = confirm_position(
                    broker,
                    symbol,
                    *qty,
                    phase_deadline,
                )
                .await?;

                done.push(format!(
                    "{symbol} at {pos:+.0} (cover leg filled)"
                ));
            }
        }
    }

    Ok((done, Outcome::Complete))
}
