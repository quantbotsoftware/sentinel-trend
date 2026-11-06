import { showToast } from '../toast.js';
import { invoke } from '../tauri-bridge.js';
import { $id, disc } from './index.js';
import { wireTooltipHover } from './watchlist.js';

// ─── Wallet Bar ─────────────────────────────────────────────────────

export function initWalletBar() {
    wireWalletButtons('user');
    wireWalletButtons('bot');
    wireMaxAssetEdit();
    wireTooltipHover('user');
    wireTooltipHover('bot');
    updateWalletBalances();
    disc.walletPollTimer = setInterval(updateWalletBalances, 3000);
}

export function wireWalletButtons(target) {
    const topupBtn = $id(`st-wallet-${target}-topup-btn`);
    const withdrawBtn = $id(`st-wallet-${target}-withdraw-btn`);
    const inputRow = $id(`st-wallet-${target}-input-row`);
    const amountInput = $id(`st-wallet-${target}-amount`);
    const confirmBtn = $id(`st-wallet-${target}-confirm`);
    const cancelBtn = $id(`st-wallet-${target}-cancel`);
    if (!topupBtn || !withdrawBtn || !inputRow || !amountInput || !confirmBtn) return;

    const showInput = (action) => {
        disc.activeWalletOp = { target, action };
        inputRow.classList.remove('st-wallet-input-row--hidden');
        amountInput.value = '';
        amountInput.placeholder = action === 'topup' ? 'Topup $' : 'Withdraw $';
        amountInput.focus();
    };

    const hideInput = () => {
        disc.activeWalletOp = null;
        inputRow.classList.add('st-wallet-input-row--hidden');
        amountInput.value = '';
    };

    topupBtn.addEventListener('click', () => {
        if (disc.activeWalletOp?.target === target && disc.activeWalletOp?.action === 'topup') { hideInput(); return; }
        showInput('topup');
    });

    withdrawBtn.addEventListener('click', () => {
        if (disc.activeWalletOp?.target === target && disc.activeWalletOp?.action === 'withdraw') { hideInput(); return; }
        showInput('withdraw');
    });

    if (cancelBtn) cancelBtn.addEventListener('click', hideInput);

    const doConfirm = async () => {
        if (!disc.activeWalletOp || disc.activeWalletOp.target !== target) return;
        const amount = parseFloat(amountInput.value);
        if (!amount || amount <= 0) {
            showToast('Amount must be greater than 0', 'error');
            return;
        }
        const cmd = `${disc.activeWalletOp.action}_${target}`;
        try {
            await invoke(cmd, { amount });
            showToast(`${disc.activeWalletOp.action === 'topup' ? 'Topup' : 'Withdraw'} $${amount.toFixed(2)} — ${target} account`, 'success');
            hideInput();
            await updateWalletBalances();
        } catch (err) {
            showToast(`${cmd} failed: ${err}`, 'error');
        }
    };

    confirmBtn.addEventListener('click', doConfirm);
    amountInput.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); doConfirm(); }
        if (e.key === 'Escape') hideInput();
    });
}

export function wireMaxAssetEdit() {
    const display = $id('st-wallet-max-display');
    const input = $id('st-wallet-max-input');
    if (!display || !input) return;

    display.addEventListener('click', () => {
        display.classList.add('st-wallet-input-row--hidden');
        input.classList.remove('st-wallet-input-row--hidden');
        input.value = parseFloat(display.textContent.replace(/[$,]/g, '')) || 500;
        input.focus();
        input.select();
    });

    const commitMaxAsset = async () => {
        const val = parseFloat(input.value);
        input.classList.add('st-wallet-input-row--hidden');
        display.classList.remove('st-wallet-input-row--hidden');
        if (!val || val < 100 || val > 50000) {
            showToast('MAX/ASSET must be between $100 and $50,000', 'error');
            return;
        }
        try {
            const cfg = await invoke('get_config');
            cfg.execution.max_usd_per_asset = val;
            await invoke('set_config', { newConfig: cfg });
            display.textContent = `$${val.toLocaleString('en-US', { maximumFractionDigits: 0 })}`;
            showToast(`MAX/ASSET set to $${val}`, 'success');
        } catch (err) {
            showToast(`MAX/ASSET update failed: ${err}`, 'error');
        }
    };

    input.addEventListener('blur', commitMaxAsset);
    input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); input.blur(); }
        if (e.key === 'Escape') {
            input.classList.add('st-wallet-input-row--hidden');
            display.classList.remove('st-wallet-input-row--hidden');
        }
    });
}

export function formatPnl(pnl, pctVal) {
    const sign = pnl >= 0 ? '+' : '';
    const pctSign = pctVal >= 0 ? '+' : '';
    return `P/L: ${sign}$${Math.abs(pnl).toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })} (${pctSign}${pctVal.toFixed(1)}%)`;
}

export function updatePnlEl(el, pnl, pctVal) {
    if (!el) return;
    el.textContent = formatPnl(pnl, pctVal);
    el.className = 'st-disc-wallet-pnl ' + (pnl >= 0 ? 'st-disc-wallet-pnl--up' : 'st-disc-wallet-pnl--down');
}

export function updateTooltip(prefix, dep, wth, pnl) {
    const depEl = $id(`st-wallet-${prefix}-tip-dep`);
    const wthEl = $id(`st-wallet-${prefix}-tip-wth`);
    const pnlEl = $id(`st-wallet-${prefix}-tip-pnl`);
    const fmt = (v) => `$${Math.abs(v).toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
    if (depEl) depEl.textContent = fmt(dep);
    if (wthEl) wthEl.textContent = fmt(wth);
    if (pnlEl) {
        const sign = pnl >= 0 ? '+' : '-';
        pnlEl.textContent = `${sign}${fmt(pnl)}`;
        pnlEl.className = pnl >= 0 ? 'st-disc-wallet-pnl--up' : 'st-disc-wallet-pnl--down';
    }
}

export async function updateWalletBalances() {
    try {
        const w = await invoke('get_wallet_status');
        const userBal = $id('st-wallet-user-bal');
        const botBal = $id('st-wallet-bot-bal');
        const maxDisp = $id('st-wallet-max-display');
        const maxInput = $id('st-wallet-max-input');
        if (userBal) userBal.textContent = `$${w.user_balance.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
        if (botBal) botBal.textContent = `$${w.bot_balance.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
        if (maxDisp && !maxInput?.matches(':focus')) {
            maxDisp.textContent = `$${w.max_usd_per_asset.toLocaleString('en-US', { maximumFractionDigits: 0 })}`;
        }

        // S16.1: True equity-based P/L includes open-position market value.
        //   pnl = (cash + positions_value) - total_deposited + total_withdrawn
        // This treats deployed capital as still belonging to the wallet, so
        // money sitting in stocks is not reported as a loss. Legacy/recon
        // positions (empty source) are attributed to the bot since the bot is
        // the dominant trading actor; user manual orders set source explicitly.
        const userPosVal = Number(w.user_positions_value) || 0;
        const botPosVal = (Number(w.bot_positions_value) || 0)
                       + (Number(w.unattributed_positions_value) || 0);

        const userEquity = w.user_balance + userPosVal;
        const userPnl = userEquity - w.user_total_deposited + w.user_total_withdrawn;
        const userPct = w.user_total_deposited > 0 ? (userPnl / w.user_total_deposited) * 100 : 0;
        updatePnlEl($id('st-wallet-user-pnl'), userPnl, userPct);

        const botEquity = w.bot_balance + botPosVal;
        const botPnl = botEquity - w.bot_total_deposited + w.bot_total_withdrawn;
        const botPct = w.bot_total_deposited > 0 ? (botPnl / w.bot_total_deposited) * 100 : 0;
        updatePnlEl($id('st-wallet-bot-pnl'), botPnl, botPct);

        updateTooltip('user', w.user_total_deposited, w.user_total_withdrawn, userPnl);
        updateTooltip('bot', w.bot_total_deposited, w.bot_total_withdrawn, botPnl);
    } catch (err) {
        console.error('[discovery] wallet poll failed:', err);
    }
}
