import { emit } from './bus.js';
import { handleLiquidateAll, handleEmergencyLiquidate } from './positions.js';
import { toggleTradingMode } from './strategy.js';

import { invoke } from './tauri-bridge.js';

const TF_ORDER = ['1m', '5m', '10m', '15m', '30m', '1h', '4h', '1D'];

function isInputFocused() {
    const tag = document.activeElement?.tagName?.toLowerCase();
    return tag === 'input' || tag === 'textarea' || tag === 'select';
}

function getCurrentTF() {
    const active = document.querySelector('.st-tf-btn--active[data-tf]');
    return active ? active.dataset.tf : '5m';
}

function cycleTF(direction) {
    const current = getCurrentTF();
    const idx = TF_ORDER.indexOf(current);
    if (idx < 0) return;

    const next = direction > 0
        ? (idx + 1) % TF_ORDER.length
        : (idx - 1 + TF_ORDER.length) % TF_ORDER.length;

    const btn = document.querySelector(`.st-tf-btn[data-tf="${TF_ORDER[next]}"]`);
    if (btn) btn.click();
}

function handleKeydown(e) {
    if (isInputFocused()) return;

    // Ctrl+Shift+X — Emergency liquidate (no confirm dialog)
    if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key.toLowerCase() === 'x') {
        e.preventDefault();
        handleEmergencyLiquidate();
        return;
    }

    // Ctrl+[ — Previous timeframe
    if ((e.ctrlKey || e.metaKey) && e.key === '[') {
        e.preventDefault();
        cycleTF(-1);
        return;
    }

    // Ctrl+] — Next timeframe
    if ((e.ctrlKey || e.metaKey) && e.key === ']') {
        e.preventDefault();
        cycleTF(1);
        return;
    }

    // Skip single-char shortcuts when modifier keys are held
    if (e.ctrlKey || e.metaKey || e.altKey) return;

    // Escape — Liquidate all (with confirm)
    if (e.key === 'Escape') {
        e.preventDefault();
        handleLiquidateAll();
        return;
    }

    // M — Toggle AUTO/MANUAL
    if (e.key.toLowerCase() === 'm') {
        e.preventDefault();
        toggleTradingMode();
        return;
    }
}

export function initKeyboard() {
    document.addEventListener('keydown', handleKeydown);
}
