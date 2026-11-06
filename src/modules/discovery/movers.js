import { escapeHtml } from '../esc.js';
import { fmtNum } from '../fmt.js';
import { $id, disc, switchSymbol } from './index.js';
import { filterTodayRows } from './session-today.js';

export function renderMovers() {
    const container = $id('st-movers-list');
    if (!container) return;

    const todayMovers = filterTodayRows(disc.movers);
    disc.movers = todayMovers;

    if (todayMovers.length === 0) {
        container.innerHTML = '<div class="st-discovery-empty">No today movers yet</div>';
        return;
    }

    container.innerHTML = todayMovers
        .slice(0, 30)
        .map((m) => {
            const sign = m.change_pct >= 0 ? '+' : '';
            const cls = m.change_pct >= 0 ? 'st-mover--up' : 'st-mover--down';
            const sym = escapeHtml(m.symbol);
            return `<div class="st-mover-cell ${cls}" data-symbol="${sym}">
                <span class="st-mover-sym">${sym}</span>
                <span class="st-mover-change">${sign}${fmtNum(m.change_pct, 1)}%</span>
            </div>`;
        })
        .join('');

    container.querySelectorAll('.st-mover-cell').forEach((cell) => {
        cell.addEventListener('click', () => {
            const sym = cell.dataset.symbol;
            if (sym) switchSymbol(sym);
        });
    });
}
