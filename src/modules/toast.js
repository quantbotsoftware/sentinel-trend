import { escapeHtml } from './esc.js';

let _container = null;

function ensureContainer() {
    if (_container) return;
    _container = document.createElement('div');
    _container.className = 'st-toast-container';
    document.body.appendChild(_container);
}

export function showToast(msg, type = 'success') {
    ensureContainer();
    const el = document.createElement('div');
    el.className = `st-toast st-toast--${type}`;
    el.textContent = msg;
    _container.appendChild(el);
    setTimeout(() => el.remove(), 4000);
}

export function showToastWithAction(message, actionLabel, onAction) {
    ensureContainer();
    const toast = document.createElement('div');
    toast.className = 'st-toast st-toast--warning st-toast--action';
    toast.innerHTML = `<span>${escapeHtml(message)}</span>
        <button class="st-toast-add-btn">${escapeHtml(actionLabel)}</button>`;
    _container.appendChild(toast);
    toast.querySelector('.st-toast-add-btn').addEventListener('click', async () => {
        await onAction();
        toast.remove();
    });
    setTimeout(() => toast.remove(), 10000);
}
