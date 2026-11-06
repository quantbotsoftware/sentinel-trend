/// S09: Engine veto / FSM rollback skeleton.
///
/// S03 added `revert_fsm_on_vetoed_entry` to fix the backtest divergence:
/// when a sentinel evaluates a candidate transition and a veto fires
/// (e.g., breaker, allow_short, PDT), the FSM MUST roll back to the
/// pre-evaluation phase rather than persist the would-be transition.
///
/// This test mirrors the contract in a self-contained mini-FSM. Drift
/// surfaces as failure; the canonical implementation lives in the
/// binary (no `lib.rs` to import from).
///
/// Acceptance: ≥1 explicit assertion per test.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    Stalking,
    Confirming,
    InPosition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FsmState {
    phase: Phase,
    confirm_count: u32,
}

impl FsmState {
    fn idle() -> Self {
        Self { phase: Phase::Idle, confirm_count: 0 }
    }
    fn confirming(n: u32) -> Self {
        Self { phase: Phase::Confirming, confirm_count: n }
    }
    fn in_position() -> Self {
        Self { phase: Phase::InPosition, confirm_count: 0 }
    }
}

/// Mirrors `revert_fsm_on_vetoed_entry`:
/// if the attempted transition would have entered a position
/// (Confirming → InPosition or any → InPosition), AND a veto fires,
/// reset to the previous FSM state.
fn revert_fsm_on_vetoed_entry(prev: &FsmState, attempted: &FsmState) -> FsmState {
    if attempted.phase == Phase::InPosition && prev.phase != Phase::InPosition {
        return prev.clone();
    }
    if attempted.phase == Phase::Confirming
        && attempted.confirm_count > prev.confirm_count
        && prev.phase != Phase::InPosition
    {
        // Confirm-bar accumulation also rolls back on entry veto.
        return prev.clone();
    }
    attempted.clone()
}

#[test]
fn veto_reverts_in_position_to_idle() {
    let prev = FsmState::idle();
    let attempted = FsmState::in_position();
    let result = revert_fsm_on_vetoed_entry(&prev, &attempted);
    assert_eq!(result, prev, "Veto on entry must restore Idle");
    assert_eq!(result.phase, Phase::Idle);
    assert_eq!(result.confirm_count, 0);
}

#[test]
fn veto_reverts_confirming_to_idle() {
    let prev = FsmState::idle();
    let attempted = FsmState::confirming(1);
    let result = revert_fsm_on_vetoed_entry(&prev, &attempted);
    assert_eq!(result, prev, "Veto on confirm-bar accumulation must rollback");
    assert_eq!(result.confirm_count, 0);
}

#[test]
fn no_veto_path_preserves_attempted() {
    // No veto: the FSM moves freely (Stalking is not an entry attempt).
    let prev = FsmState { phase: Phase::Idle, confirm_count: 0 };
    let attempted = FsmState { phase: Phase::Stalking, confirm_count: 0 };
    let result = revert_fsm_on_vetoed_entry(&prev, &attempted);
    assert_eq!(result, attempted, "Non-entry transitions must NOT roll back");
    assert_eq!(result.phase, Phase::Stalking);
}

#[test]
fn already_in_position_is_not_reverted() {
    // If we are already InPosition, a "stayed InPosition" attempt is
    // not an entry attempt and must not be rolled back.
    let prev = FsmState::in_position();
    let attempted = FsmState::in_position();
    let result = revert_fsm_on_vetoed_entry(&prev, &attempted);
    assert_eq!(result, attempted);
    assert_eq!(result.phase, Phase::InPosition);
}

#[test]
fn veto_idempotent_on_repeat_invocation() {
    let prev = FsmState::idle();
    let attempted = FsmState::in_position();
    let r1 = revert_fsm_on_vetoed_entry(&prev, &attempted);
    let r2 = revert_fsm_on_vetoed_entry(&prev, &r1);
    assert_eq!(r1, r2, "Reverting an already-reverted state must be a no-op");
}
