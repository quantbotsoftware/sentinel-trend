import { invoke } from './tauri-bridge.js';
import { emit } from './bus.js';
import { showToast } from './toast.js';

function cloneConfig(cfg) {
    return JSON.parse(JSON.stringify(cfg));
}

function isPlainObject(value) {
    return !!value && typeof value === 'object' && !Array.isArray(value);
}

function hasOwn(obj, key) {
    return Object.prototype.hasOwnProperty.call(obj, key);
}

function isCompatibleValue(baseValue, incomingValue) {
    if (incomingValue === null || incomingValue === undefined) return false;
    if (Array.isArray(baseValue)) return Array.isArray(incomingValue);
    if (isPlainObject(baseValue)) return isPlainObject(incomingValue);
    if (typeof baseValue === 'number') {
        return typeof incomingValue === 'number' && Number.isFinite(incomingValue);
    }
    if (typeof baseValue === 'boolean') return typeof incomingValue === 'boolean';
    if (typeof baseValue === 'string') return typeof incomingValue === 'string';
    return typeof incomingValue === typeof baseValue;
}

function assignIfCompatible(target, key, value) {
    if (!hasOwn(target, key)) return;
    if (!isCompatibleValue(target[key], value)) return;
    target[key] = value;
}

function resolveSection(incomingRoot, sectionKey) {
    if (!hasOwn(incomingRoot, sectionKey)) return null;
    return isPlainObject(incomingRoot[sectionKey]) ? incomingRoot[sectionKey] : null;
}

const SMARTTREND_ALIAS_MAP = {
    exit_confirm_bars: ['exitConfirmBars'],
    risk_per_trade: ['riskPerTrade'],
    max_drawdown_pct: ['maxDrawdownPct'],
    max_hold_minutes: ['maxHoldMinutes'],
};

const EXECUTION_ALIAS_MAP = {
    max_positions: ['maxPositions'],
    allow_short: ['allowShort'],
    bracket_atr_mult: ['bracketAtrMult'],
    order_timeout_secs: ['orderTimeoutSecs'],
};

const UI_ALIAS_MAP = {
    active_symbol: ['activeSymbol'],
    default_timeframe: ['defaultTimeframe'],
    chart_show_volume: ['chartShowVolume'],
};

const MARKET_ALIAS_MAP = {
    alpaca_key_id: ['alpacaKeyId'],
    alpaca_secret: ['alpacaSecret'],
    paper_mode: ['paperMode'],
    sip_feed: ['sipFeed'],
    discovery_poll_secs: ['discoveryPollSecs'],
    stale_data_secs: ['staleDataSecs'],
    fmp_api_key: ['fmpApiKey'],
};

function mergeSectionWithPrecedence(baseSection, incomingSection, aliasMap, blockedKeys = []) {
    const base = isPlainObject(baseSection) ? baseSection : {};
    const incoming = isPlainObject(incomingSection) ? incomingSection : {};
    const merged = { ...base };
    const blocked = new Set(blockedKeys);

    // New/canonical keys win when both old + new are present.
    for (const key of Object.keys(incoming)) {
        if (blocked.has(key)) continue;
        assignIfCompatible(merged, key, incoming[key]);
    }

    for (const [canonical, aliases] of Object.entries(aliasMap)) {
        if (blocked.has(canonical)) continue;
        if (hasOwn(incoming, canonical)) {
            assignIfCompatible(merged, canonical, incoming[canonical]);
            continue;
        }
        const legacyKey = aliases.find((alias) => hasOwn(incoming, alias));
        if (legacyKey) {
            assignIfCompatible(merged, canonical, incoming[legacyKey]);
        }
    }

    return merged;
}

function mergeImportedConfig(currentConfig, importedConfig) {
    const base = cloneConfig(currentConfig);
    const incoming = isPlainObject(importedConfig) ? importedConfig : {};

    const incomingSmarttrend = resolveSection(incoming, 'smarttrend');
    if (incomingSmarttrend) {
        base.smarttrend = mergeSectionWithPrecedence(
            base.smarttrend,
            incomingSmarttrend,
            SMARTTREND_ALIAS_MAP
        );
    }
    const incomingExecution = resolveSection(incoming, 'execution');
    if (incomingExecution) {
        base.execution = mergeSectionWithPrecedence(
            base.execution,
            incomingExecution,
            EXECUTION_ALIAS_MAP
        );
    }
    const incomingUi = resolveSection(incoming, 'ui');
    if (incomingUi) {
        base.ui = mergeSectionWithPrecedence(base.ui, incomingUi, UI_ALIAS_MAP);
        if (Array.isArray(incomingUi.favorites)) {
            base.ui.favorites = [...incomingUi.favorites];
        }
    }
    const incomingMarket = resolveSection(incoming, 'market');
    if (incomingMarket) {
        base.market = mergeSectionWithPrecedence(
            base.market,
            incomingMarket,
            MARKET_ALIAS_MAP,
            ['scan_params', 'auto_scan_active']
        );
        if (isPlainObject(incomingMarket.scan_params) && isPlainObject(base.market.scan_params)) {
            const mergedScanParams = { ...base.market.scan_params };
            for (const [key, value] of Object.entries(incomingMarket.scan_params)) {
                assignIfCompatible(mergedScanParams, key, value);
            }
            base.market.scan_params = mergedScanParams;
        }
        if (typeof incomingMarket.auto_scan_active === 'boolean') {
            base.market.auto_scan_active = incomingMarket.auto_scan_active;
        }
    }

    return base;
}

export function __testMergeImportedConfig(currentConfig, importedConfig) {
    return mergeImportedConfig(currentConfig, importedConfig);
}

export function __runConfigIoMergeSelfTests() {
    const base = {
        smarttrend: {
            exit_confirm_bars: 2,
            risk_per_trade: 0.01,
            max_drawdown_pct: 5.0,
            max_hold_minutes: 120,
        },
        execution: {
            max_positions: 5,
            allow_short: false,
            bracket_atr_mult: 2.0,
            order_timeout_secs: 30,
        },
        ui: {
            active_symbol: 'AAPL',
            favorites: ['AAPL', 'MSFT'],
            default_timeframe: '5m',
            chart_show_volume: true,
        },
        market: {
            alpaca_key_id: '',
            alpaca_secret: '',
            paper_mode: true,
            sip_feed: true,
            discovery_poll_secs: 60,
            stale_data_secs: 30,
            fmp_api_key: '',
            auto_scan_active: false,
            scan_params: {
                min_price: 5,
                max_price: 1000,
            },
        },
    };

    const mergedWithNullSections = __testMergeImportedConfig(base, {
        smarttrend: null,
        execution: 'oops',
        ui: 42,
        market: false,
    });
    if (JSON.stringify(mergedWithNullSections) !== JSON.stringify(base)) {
        throw new Error('Null/wrong-type sections must fallback to current config');
    }

    const mergedPrecedence = __testMergeImportedConfig(base, {
        smarttrend: {
            risk_per_trade: 0.02,
            riskPerTrade: 0.05,
        },
    });
    if (mergedPrecedence.smarttrend.risk_per_trade !== 0.02) {
        throw new Error('Canonical key must win over legacy alias');
    }

    const mergedWrongTypes = __testMergeImportedConfig(base, {
        smarttrend: {
            exit_confirm_bars: '3',
            risk_per_trade: null,
            max_hold_minutes: 90,
        },
        ui: {
            favorites: 'AAPL',
            chart_show_volume: 'yes',
        },
        market: {
            auto_scan_active: 'true',
            scan_params: {
                min_price: 'low',
                max_price: 900,
            },
        },
    });
    if (mergedWrongTypes.smarttrend.exit_confirm_bars !== 2) {
        throw new Error('Wrong type must not overwrite smarttrend numeric fields');
    }
    if (mergedWrongTypes.smarttrend.max_hold_minutes !== 90) {
        throw new Error('Valid smarttrend field should merge');
    }
    if (!Array.isArray(mergedWrongTypes.ui.favorites)) {
        throw new Error('Wrong type favorites must fallback to base array');
    }
    if (mergedWrongTypes.market.auto_scan_active !== false) {
        throw new Error('Wrong type boolean must fallback to base value');
    }
    if (mergedWrongTypes.market.scan_params.max_price !== 900) {
        throw new Error('Valid nested scan_params number should merge');
    }
    if (mergedWrongTypes.market.scan_params.min_price !== 5) {
        throw new Error('Invalid nested scan_params type must fallback');
    }

    const mergedMissingSections = __testMergeImportedConfig(base, {
        smarttrend: {
            exitConfirmBars: 4,
        },
    });
    if (mergedMissingSections.smarttrend.exit_confirm_bars !== 4) {
        throw new Error('Legacy alias should merge when canonical missing');
    }
    if (mergedMissingSections.execution.max_positions !== base.execution.max_positions) {
        throw new Error('Missing sections must remain unchanged');
    }

    return {
        passed: 4,
    };
}

export async function exportConfig() {
    try {
        const cfg = await invoke('get_config');
        const json = JSON.stringify(cfg, null, 2);
        const blob = new Blob([json], { type: 'application/json' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = `sentineltrend-config-${new Date().toISOString().slice(0, 10)}.json`;
        a.click();
        URL.revokeObjectURL(url);
        showToast('Config exported', 'success');
    } catch (e) {
        showToast(`Export failed: ${e}`, 'error');
    }
}

export async function importConfig() {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.json';
    input.addEventListener('change', async () => {
        const file = input.files?.[0];
        if (!file) return;
        try {
            const text = await file.text();
            const importedCfg = JSON.parse(text);
            if (!isPlainObject(importedCfg)) {
                throw new Error('Top-level JSON object is required');
            }

            const currentCfg = await invoke('get_config');
            const candidateCfg = mergeImportedConfig(currentCfg, importedCfg);
            const errors = await invoke('validate_config', { candidateConfig: candidateCfg });
            if (errors && errors.length > 0) {
                showToast(`Invalid config: ${errors.join('; ')}`, 'error');
                return;
            }
            emit('config-imported', candidateCfg);
            showToast('Config loaded from file — review and Save', 'warning');
        } catch (e) {
            showToast(`Import failed: ${e}`, 'error');
        }
    });
    input.click();
}
