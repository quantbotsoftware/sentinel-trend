// Catalyst news modal — click NEWS pill → headlines + open article URL.
import { escapeHtml, escapeAttr } from '../esc.js';
import { showToast } from '../toast.js';
import { invoke } from '../tauri-bridge.js';
import { $id } from './index.js';

let _bound = false;

function modalEls() {
    return {
        root: $id('st-news-modal'),
        title: $id('st-news-modal-title'),
        body: $id('st-news-modal-body'),
        empty: $id('st-news-modal-empty'),
        close: $id('st-news-modal-close'),
    };
}

function hideNewsModal() {
    const { root } = modalEls();
    if (root) root.classList.add('st-news-modal--hidden');
}

async function openArticle(url) {
    const u = String(url || '').trim();
    if (!u) {
        showToast('No article URL for this headline', 'warn');
        return;
    }
    try {
        await invoke('open_external_url', { url: u });
    } catch (err) {
        showToast(`Open link failed: ${err}`, 'error');
    }
}

function renderHeadlines(symbol, items) {
    const { title, body, empty } = modalEls();
    if (title) title.textContent = `${symbol} — CATALYST NEWS`;
    if (!body) return;
    const list = Array.isArray(items) ? items : [];
    if (list.length === 0) {
        body.innerHTML = '';
        if (empty) {
            empty.classList.remove('st-news-modal-empty--hidden');
            empty.textContent = 'No recent news cached for this symbol';
        }
        return;
    }
    if (empty) empty.classList.add('st-news-modal-empty--hidden');
    body.innerHTML = list.map((n) => {
        const headline = escapeHtml(n.headline || '(no headline)');
        const source = escapeHtml(n.source || n.author || 'wire');
        const when = escapeHtml(n.created_at || '');
        const url = String(n.url || '').trim();
        const hasUrl = url.startsWith('http://') || url.startsWith('https://');
        const btn = hasUrl
            ? `<button type="button" class="st-news-open" data-url="${escapeAttr(url)}">READ</button>`
            : `<span class="st-news-nourl">NO LINK</span>`;
        return `<div class="st-news-item">
            <div class="st-news-item-main">
                <div class="st-news-headline">${headline}</div>
                <div class="st-news-meta">${source}${when ? ` · ${when}` : ''}</div>
            </div>
            ${btn}
        </div>`;
    }).join('');

    body.querySelectorAll('.st-news-open').forEach((btn) => {
        btn.addEventListener('click', (e) => {
            e.stopPropagation();
            openArticle(btn.dataset.url);
        });
    });
}

export async function openNewsModal(symbol, fallback) {
    const sym = String(symbol || '').trim().toUpperCase();
    if (!sym) return;
    const { root, body, empty } = modalEls();
    if (!root) return;
    root.classList.remove('st-news-modal--hidden');
    if (body) {
        body.innerHTML = '<div class="st-news-loading">Loading headlines…</div>';
    }
    if (empty) empty.classList.add('st-news-modal-empty--hidden');

    try {
        const items = await invoke('get_news_for_symbol', { symbol: sym });
        if (Array.isArray(items) && items.length > 0) {
            renderHeadlines(sym, items);
            return;
        }
        // Scan row may already carry the top headline/URL even if cache miss
        // races the modal — surface that single item.
        if (fallback?.headline) {
            renderHeadlines(sym, [{
                headline: fallback.headline,
                url: fallback.url || '',
                source: 'scan',
                created_at: '',
                author: '',
            }]);
            return;
        }
        renderHeadlines(sym, []);
    } catch (err) {
        if (body) body.innerHTML = '';
        if (empty) {
            empty.classList.remove('st-news-modal-empty--hidden');
            empty.textContent = `News fetch failed: ${err}`;
        }
    }
}

/** Delegate clicks on NEWS pills inside a table/container. */
export function wireNewsPillClicks(container, resolveFallback) {
    if (!container) return;
    container.addEventListener('click', (e) => {
        const pill = e.target.closest('[data-news-symbol]');
        if (!pill) return;
        e.preventDefault();
        e.stopPropagation();
        const sym = pill.dataset.newsSymbol;
        const fb = typeof resolveFallback === 'function' ? resolveFallback(sym) : null;
        openNewsModal(sym, fb);
    });
}

export function initNewsModal() {
    if (_bound) return;
    _bound = true;
    const { root, close } = modalEls();
    close?.addEventListener('click', hideNewsModal);
    root?.addEventListener('click', (e) => {
        if (e.target === root) hideNewsModal();
    });
}
