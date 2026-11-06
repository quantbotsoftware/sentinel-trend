import { emit } from './bus.js';
import { showToast } from './toast.js';

import { invoke, listen } from './tauri-bridge.js';

// ═══════════════════════════════════════════════════════════════════
//  Module state
// ═══════════════════════════════════════════════════════════════════

let _isOpen = false;
let _side = 'buy';
let _orderType = 'market';
let _amountMode = 'usd';
let _walletSource = 'user';
let _currentSymbol = '';
let _currentPrice = 0;

// DOM refs (cached on init)
let _panel, _symbolInput, _priceEl, _amountInput, _stoplossInput, _takeprofitInput;
let _btnBuy, _btnSell, _modeToggle, _btnWalletUser, _btnWalletBot;
let _btnTypeMarket, _btnTypeLimit, _limitPriceRow, _limitPriceInput;
let _estShares, _estCost, _errorEl, _submitBtn;

// ═══════════════════════════════════════════════════════════════════
//  Init — called once from discovery.js
// ═══════════════════════════════════════════════════════════════════

export function initManualOrder() {
    _panel       = document.getElementById('st-order-panel');
    _symbolInput = document.getElementById('st-order-symbol');
    _priceEl     = document.getElementById('st-order-price');
    _amountInput = document.getElementById('st-order-amount');
    _stoplossInput = document.getElementById('st-order-stoploss');
    _takeprofitInput = document.getElementById('st-order-takeprofit');
    _btnBuy      = document.getElementById('st-order-buy');
    _btnSell     = document.getElementById('st-order-sell');
    _modeToggle  = document.getElementById('st-order-mode-toggle');
    _btnWalletUser = document.getElementById('st-order-wallet-user');
    _btnWalletBot  = document.getElementById('st-order-wallet-bot');
    _btnTypeMarket = document.getElementById('st-order-type-market');
    _btnTypeLimit  = document.getElementById('st-order-type-limit');
    _limitPriceRow = document.getElementById('st-order-limitprice-row');
    _limitPriceInput = document.getElementById('st-order-limitprice');
    _estShares   = document.getElementById('st-order-est-shares');
    _estCost     = document.getElementById('st-order-est-cost');
    _errorEl     = document.getElementById('st-order-error');
    _submitBtn   = document.getElementById('st-order-submit');

    if (!_panel) return;

    wireButtons();
    wireLivePrice();
    document.addEventListener('keydown', handleOrderKeydown);
}

// ═══════════════════════════════════════════════════════════════════
//  Open / Close
// ═══════════════════════════════════════════════════════════════════

export function openOrderForm(symbol, price) {
    if (!_panel) return;
    _currentSymbol = (symbol || '').toUpperCase();
    _currentPrice = price || 0;
    _side = 'buy';
    _orderType = 'market';
    _amountMode = 'usd';
    _walletSource = 'user';

    _symbolInput.value = _currentSymbol;
    _priceEl.textContent = _currentPrice > 0 ? `$${_currentPrice.toFixed(2)}` : '--';
    _amountInput.value = '';
    _stoplossInput.value = '';
    _takeprofitInput.value = '';
    if (_limitPriceInput) _limitPriceInput.value = '';
    _modeToggle.textContent = 'USD';
    applyTypeUI();

    applySideUI();
    applyWalletUI();
    updateEstimates();
    clearError();

    _panel.classList.remove('st-order-panel--hidden');
    _isOpen = true;
    _amountInput.focus();
}

export function closeOrderForm() {
    if (!_panel) return;
    _panel.classList.add('st-order-panel--hidden');
    _isOpen = false;
    clearError();
}

// ═══════════════════════════════════════════════════════════════════
//  Button wiring
// ═══════════════════════════════════════════════════════════════════

function wireButtons() {
    document.getElementById('st-order-close').addEventListener('click', closeOrderForm);

    _btnBuy.addEventListener('click', () => { _side = 'buy'; applySideUI(); });
    _btnSell.addEventListener('click', () => { _side = 'sell'; applySideUI(); });

    _modeToggle.addEventListener('click', () => {
        _amountMode = _amountMode === 'usd' ? 'shares' : 'usd';
        _modeToggle.textContent = _amountMode === 'usd' ? 'USD' : 'SHARES';
        updateEstimates();
    });

    _btnWalletUser.addEventListener('click', () => { _walletSource = 'user'; applyWalletUI(); });
    _btnWalletBot.addEventListener('click', () => { _walletSource = 'bot'; applyWalletUI(); });

    if (_btnTypeMarket) _btnTypeMarket.addEventListener('click', () => { _orderType = 'market'; applyTypeUI(); });
    if (_btnTypeLimit) _btnTypeLimit.addEventListener('click', () => { _orderType = 'limit'; applyTypeUI(); });

    _amountInput.addEventListener('input', updateEstimates);
    _symbolInput.addEventListener('input', () => {
        _currentSymbol = _symbolInput.value.toUpperCase().trim();
    });

    _submitBtn.addEventListener('click', handleSubmit);

    // Quick-size buttons
    document.querySelectorAll('.st-order-qsize-btn').forEach((btn) => {
        btn.addEventListener('click', async () => {
            const pct = parseInt(btn.dataset.pct, 10);
            try {
                const w = await invoke('get_wallet_status');
                const bal = _walletSource === 'bot' ? w.bot_balance : w.user_balance;
                const amt = (bal * pct / 100);
                _amountInput.value = amt.toFixed(2);
                updateEstimates();
            } catch (err) {
                console.error('[manual-order] quick-size failed:', err);
            }
        });
    });

    // NEW ORDER button
    const newOrderBtn = document.getElementById('st-scan-btn-new-order');
    if (newOrderBtn) {
        newOrderBtn.addEventListener('click', () => openOrderForm('', 0));
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Live price listener
// ═══════════════════════════════════════════════════════════════════

function wireLivePrice() {
    listen('price-update', (event) => {
        if (!_isOpen) return;
        const { symbol, price } = event.payload;
        if (symbol === _currentSymbol && price > 0) {
            _currentPrice = price;
            _priceEl.textContent = `$${price.toFixed(2)}`;
            updateEstimates();
        }
    });
}

// ═══════════════════════════════════════════════════════════════════
//  UI state helpers
// ═══════════════════════════════════════════════════════════════════

function applySideUI() {
    _btnBuy.classList.toggle('st-order-side-btn--active', _side === 'buy');
    _btnSell.classList.toggle('st-order-side-btn--active', _side === 'sell');
}

function applyWalletUI() {
    _btnWalletUser.classList.toggle('st-order-wallet-btn--active', _walletSource === 'user');
    _btnWalletBot.classList.toggle('st-order-wallet-btn--active', _walletSource === 'bot');
}

function applyTypeUI() {
    if (_btnTypeMarket) _btnTypeMarket.classList.toggle('st-order-type-btn--active', _orderType === 'market');
    if (_btnTypeLimit) _btnTypeLimit.classList.toggle('st-order-type-btn--active', _orderType === 'limit');
    if (_limitPriceRow) {
        _limitPriceRow.classList.toggle('st-order-row--hidden', _orderType !== 'limit');
    }
}

function updateEstimates() {
    const raw = parseFloat(_amountInput.value);
    const amt = isNaN(raw) || raw <= 0 ? 0 : raw;

    if (amt === 0 || _currentPrice <= 0) {
        _estShares.textContent = 'Est. Shares: --';
        _estCost.textContent = 'Est. Cost: --';
        return;
    }

    if (_amountMode === 'usd') {
        const shares = +(amt / _currentPrice).toFixed(4);
        const cost = shares * _currentPrice;
        _estShares.textContent = `Est. Shares: ${shares}`;
        _estCost.textContent = `Est. Cost: $${cost.toFixed(2)}`;
    } else {
        const shares = +amt.toFixed(4);
        const cost = shares * _currentPrice;
        _estShares.textContent = `Est. Shares: ${shares}`;
        _estCost.textContent = `Est. Cost: $${cost.toFixed(2)}`;
    }
}

function clearError() {
    _errorEl.textContent = '';
    _errorEl.classList.add('st-order-error--hidden');
}

function showError(msg) {
    _errorEl.textContent = msg;
    _errorEl.classList.remove('st-order-error--hidden');
}

// ═══════════════════════════════════════════════════════════════════
//  Validation
// ═══════════════════════════════════════════════════════════════════

async function validate() {
    const symbol = _symbolInput.value.toUpperCase().trim();
    if (!symbol) return 'Symbol is required';

    const raw = parseFloat(_amountInput.value);
    if (isNaN(raw) || raw <= 0) return 'Amount must be greater than 0';

    if (_currentPrice <= 0) return 'No live price available — wait for data';

    if (_orderType === 'limit') {
        const lp = parseFloat(_limitPriceInput?.value);
        if (isNaN(lp) || lp <= 0) return 'Limit price must be greater than 0';
    }

    let qty, cost;
    if (_amountMode === 'usd') {
        qty = +(raw / _currentPrice).toFixed(4);
        cost = qty * _currentPrice;
        if (qty < 0.01) return 'Amount too small — results in less than 0.01 shares';
    } else {
        qty = +raw.toFixed(4);
        cost = qty * _currentPrice;
        if (qty < 0.01) return 'Shares must be at least 0.01';
    }

    try {
        const w = await invoke('get_wallet_status');
        const balance = _walletSource === 'user' ? w.user_balance : w.bot_balance;
        if (cost > balance) {
            const label = _walletSource === 'user' ? 'User' : 'Bot';
            return `Insufficient ${label} balance: need $${cost.toFixed(2)}, have $${balance.toFixed(2)}`;
        }
    } catch (err) {
        return `Wallet check failed: ${err}`;
    }

    return null;
}

// ═══════════════════════════════════════════════════════════════════
//  Submit
// ═══════════════════════════════════════════════════════════════════

async function handleSubmit() {
    clearError();
    _submitBtn.disabled = true;

    const errMsg = await validate();
    if (errMsg) {
        showError(errMsg);
        _submitBtn.disabled = false;
        return;
    }

    const symbol = _symbolInput.value.toUpperCase().trim();
    const raw = parseFloat(_amountInput.value);

    let qty, notionalUsd;
    if (_amountMode === 'usd') {
        qty = +(raw / _currentPrice).toFixed(4);
        notionalUsd = qty * _currentPrice;
    } else {
        qty = +raw.toFixed(4);
        notionalUsd = qty * _currentPrice;
    }

    const sl = parseFloat(_stoplossInput.value);
    const tp = parseFloat(_takeprofitInput.value);

    const lp = _limitPriceInput ? parseFloat(_limitPriceInput.value) : NaN;

    const params = {
        symbol,
        side: _side,
        qty,
        stop_loss: isNaN(sl) || sl <= 0 ? null : sl,
        take_profit: isNaN(tp) || tp <= 0 ? null : tp,
        notional_usd: notionalUsd,
        wallet_source: _walletSource,
        order_type: _orderType,
        limit_price: _orderType === 'limit' && !isNaN(lp) && lp > 0 ? lp : null,
    };

    try {
        const resp = await invoke('submit_manual_order', { params });
        const sideLabel = resp.side.toUpperCase();
        showToast(`ORDER FILLED: ${sideLabel} ${resp.qty} ${resp.symbol} @ $${_currentPrice.toFixed(2)}`, 'success');
        emit('wallet-changed');
        closeOrderForm();
    } catch (err) {
        const msg = typeof err === 'string' ? err : (err && err.message ? err.message : JSON.stringify(err));
        showError(`ORDER FAILED: ${msg}`);
        showToast(`ORDER FAILED: ${msg}`, 'error');
    } finally {
        _submitBtn.disabled = false;
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Keyboard shortcuts (global)
// ═══════════════════════════════════════════════════════════════════

function handleOrderKeydown(e) {
    const inInput = ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement?.tagName);

    if (e.key === 'Escape' && _isOpen) {
        e.preventDefault();
        e.stopPropagation();
        closeOrderForm();
        return;
    }

    if ((e.ctrlKey || e.metaKey) && e.key === 'Enter' && _isOpen) {
        e.preventDefault();
        e.stopPropagation();
        handleSubmit();
        return;
    }

    if (inInput) return;

    if (e.key.toLowerCase() === 'n' && !_isOpen) {
        e.preventDefault();
        openOrderForm('', 0);
        return;
    }

    if (_isOpen) {
        if (e.key.toLowerCase() === 'b') {
            e.preventDefault();
            _side = 'buy';
            applySideUI();
            return;
        }
        if (e.key.toLowerCase() === 's') {
            e.preventDefault();
            _side = 'sell';
            applySideUI();
            return;
        }
        if (e.key.toLowerCase() === 'l') {
            e.preventDefault();
            _orderType = _orderType === 'limit' ? 'market' : 'limit';
            applyTypeUI();
            if (_orderType === 'limit' && _limitPriceInput) _limitPriceInput.focus();
            return;
        }
    }
}
