import { summarizeActionableAlert } from './alert-fmt.js';
import { on, emit } from './bus.js';
import { escapeHtml } from './esc.js';
import { fmtNum } from './fmt.js';

import { invoke, listen } from './tauri-bridge.js';

const $id = (id) => document.getElementById(id);

// ─── Tab Switching ──────────────────────────────────────────────────

function setupTabs() {
    const tabBar = document.querySelector('.st-tab-bar');
    if (!tabBar) return;

    tabBar.addEventListener('click', (e) => {
        const btn = e.target.closest('.st-tab-btn');
        if (!btn) return;

        const tabName = btn.getAttribute('data-tab');
        if (!tabName) return;

        tabBar.querySelectorAll('.st-tab-btn').forEach((b) => {
            b.classList.toggle('st-tab-btn--active', b === btn);
        });

        document.querySelectorAll('.st-tab-content').forEach((panel) => {
            const isTarget = panel.id === `st-tab-${tabName}`;
            panel.classList.toggle('st-tab-content--hidden', !isTarget);
        });

        if (tabName === 'engine') {
            loadEngineDetails();
        }
        if (tabName === 'journal') {
            emit('tab-journal-active');
        }
        if (tabName === 'config') {
            emit('tab-config-active');
        }
        if (tabName === 'prime') {
            emit('tab-prime-active');
        }
    });
}

// ─── Score Color Gradient ───────────────────────────────────────────

function scoreColor(score) {
    if (score >= 70) return 'hud-value--green';
    if (score >= 55) return 'hud-value--cyan';
    if (score >= 40) return 'hud-value--gold';
    return '';
}

function dirColor(dir) {
    if (dir === 'Bullish') return 'hud-value--green';
    if (dir === 'Bearish') return 'hud-value--red';
    return '';
}

const PHASE_CLASS_MAP = {
    Idle: 'st-fsm--idle',
    Stalking: 'st-fsm--stalking',
    Confirming: 'st-fsm--confirming',
    Active: 'st-fsm--active',
    Exit: 'st-fsm--exit',
    Pullback: 'st-fsm--pullback',
};

function renderEnginePhase(phase, details = {}) {
    const phaseEl = $id('st-engine-phase');
    if (!phaseEl) return;

    phaseEl.textContent = phase || '---';
    phaseEl.className = 'hud-value hud-value--mono st-fsm-phase';
    const cls = PHASE_CLASS_MAP[phase];
    if (cls) phaseEl.classList.add(cls);

    const transition =
        details.prevPhase && phase && details.prevPhase !== phase
            ? `${details.prevPhase} -> ${phase}`
            : (phase || '');
    const markerSummary = summarizeActionableAlert(details.markers);
    const tooltipParts = [
        transition ? `Phase: ${transition}` : '',
        details.direction ? `Dir: ${details.direction}` : '',
        Number.isFinite(details.score) ? `Score: ${fmtNum(details.score, 1)}` : '',
        markerSummary ? `Action: ${markerSummary}` : '',
    ].filter(Boolean);
    phaseEl.title = tooltipParts.join(' | ');
}

// ─── Render Pillar ──────────────────────────────────────────────────

function renderPillar(prefix, pillar) {
    const scoreEl = $id(`st-pillar-${prefix}-score`);
    const dirEl = $id(`st-pillar-${prefix}-dir`);
    if (!pillar) {
        if (scoreEl) scoreEl.textContent = '---';
        if (dirEl) dirEl.textContent = '---';
        return;
    }

    if (scoreEl) {
        scoreEl.textContent = fmtNum(pillar.score, 1);
        scoreEl.className = 'st-pillar-score hud-value--mono';
        const cls = scoreColor(pillar.score);
        if (cls) scoreEl.classList.add(cls);
        if (pillar.stale) scoreEl.classList.add('st-pillar-score--stale');
    }

    if (dirEl) {
        dirEl.textContent = pillar.direction;
        dirEl.className = 'st-pillar-dir hud-value--mono';
        const cls = dirColor(pillar.direction);
        if (cls) dirEl.classList.add(cls);
    }
}

// ─── Render Safety ──────────────────────────────────────────────────

function renderSafety(checks) {
    const summaryEl = $id('st-engine-safety');
    const listEl = $id('st-safety-list');
    if (!checks || checks.length === 0) {
        if (summaryEl) summaryEl.textContent = '---';
        if (listEl) listEl.innerHTML = '';
        return;
    }

    const passed = checks.filter((c) => c.passed).length;
    const total = checks.length;
    const allPass = passed === total;

    if (summaryEl) {
        summaryEl.textContent = `${passed}/${total} ${allPass ? 'PASS' : 'FAIL'}`;
        summaryEl.className = 'hud-value hud-value--mono';
        summaryEl.classList.add(allPass ? 'hud-value--green' : 'hud-value--red');
    }

    if (listEl) {
        listEl.innerHTML = checks
            .map((c) => {
                const cls = c.passed ? 'st-safety--pass' : 'st-safety--fail';
                return `<div class="st-safety-row ${cls}">
                    <span class="st-safety-name">${escapeHtml(c.name)}</span>
                    <span class="st-safety-detail">${escapeHtml(c.detail)}</span>
                </div>`;
            })
            .join('');
    }
}

// ─── Load Full Details ──────────────────────────────────────────────

async function loadEngineDetails() {
    try {
        const d = await invoke('get_engine_details');
        if (!d) return;

        renderPillar('momentum', d.momentum_pillar);

        renderEnginePhase(d.phase);

        const confEl = $id('st-engine-confirms');
        if (confEl) confEl.textContent = `${d.confirm_count}/${d.confirm_target}`;

        const holdEl = $id('st-engine-maxhold');
        if (holdEl) holdEl.textContent = `${d.max_hold_minutes}m`;

        renderSafety(d.safety_checks);

        const evalEl = $id('st-engine-last-eval');
        if (evalEl) {
            if (d.last_eval_ts) {
                const dt = new Date(d.last_eval_ts * 1000);
                evalEl.textContent = dt.toLocaleTimeString('en-US', { timeZone: 'America/New_York', hour12: false });
            } else {
                evalEl.textContent = '---';
            }
        }

        const evalsEl = $id('st-engine-evals');
        if (evalsEl) evalsEl.textContent = String(d.evals_total);

        const barsDrpEl = $id('st-engine-bars-dropped');
        if (barsDrpEl) {
            const n = d?.validation_metrics?.blockages?.bars_dropped ?? 0;
            barsDrpEl.textContent = String(n);
        }
    } catch (err) {
        console.error('[smarttrend] get_engine_details failed:', err);
    }
}

// ─── Live Updates via smarttrend-eval ────────────────────────────────

function setupLiveUpdates() {
    listen('smarttrend-eval', (event) => {
        const payload = event.payload;
        const activeSymbol = $id('st-header-symbol')?.textContent?.trim();
        if (payload?.symbol && activeSymbol && payload.symbol !== activeSymbol) return;
        const d = payload && payload.diagnostics;
        if (!d) return;

        const enginePanel = $id('st-tab-engine');
        if (!enginePanel || enginePanel.classList.contains('st-tab-content--hidden')) return;

        if (d.momentum_score) renderPillar('momentum', d.momentum_score);
    });

    listen('smarttrend-alert', (event) => {
        const payload = event.payload;
        if (!payload) return;
        const activeSymbol = $id('st-header-symbol')?.textContent?.trim();
        if (payload?.symbol && activeSymbol && payload.symbol !== activeSymbol) return;

        const enginePanel = $id('st-tab-engine');
        if (!enginePanel || enginePanel.classList.contains('st-tab-content--hidden')) return;

        renderEnginePhase(payload.phase, {
            prevPhase: payload.prev_phase,
            direction: payload.direction,
            score: payload.score,
            markers: payload.markers,
        });
    });
}

// ─── APEX-only tab pruning ──────────────────────────────────────────
//
// When `evolution_protocol.apex_only` is set (default since the
// 2026-07-04 audit), the legacy TOURNAMENT / D-PLUS / OMEGA subsystems
// no longer originate live orders — their tabs are dead UI and are
// hidden. Setting `apex_only: false` in settings.json restores them.

const APEX_HIDDEN_TABS = ['tournament', 'dplus', 'omega'];

async function applyApexOnlyTabs() {
    try {
        const cfg = await invoke('get_config');
        if (!cfg?.apex_only) return;
        for (const tab of APEX_HIDDEN_TABS) {
            document
                .querySelector(`.st-tab-btn[data-tab="${tab}"]`)
                ?.classList.add('st-tab-btn--hidden');
            $id(`st-tab-${tab}`)?.classList.add('st-tab-content--hidden');
        }
    } catch (err) {
        console.warn('[smarttrend] apex_only tab pruning failed:', err);
    }
}

// ─── Public Init ────────────────────────────────────────────────────

export function initSmartTrend() {
    setupTabs();
    setupLiveUpdates();
    applyApexOnlyTabs();
}
