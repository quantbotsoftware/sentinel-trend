import { showToast } from './toast.js';
import { showAllocModal } from './positions.js';
import { invoke } from './tauri-bridge.js';

export async function toggleTradingMode({ onResult } = {}) {
    try {
        const result = await invoke('toggle_trading_mode');
        showToast(`Mode: ${result}`, 'success');
        if (onResult) onResult(result);
        if (result === 'Auto') {
            try {
                const w = await invoke('get_wallet_status');
                if (w.bot_balance < 25 && w.user_balance > 0) {
                    showAllocModal(w.user_balance);
                }
            } catch (_) { /* wallet check failed, skip */ }
        }
    } catch (err) {
        const msg = typeof err === 'string' ? err : err?.message || JSON.stringify(err);
        showToast(`Mode toggle failed: ${msg}`, 'error');
    }
}
