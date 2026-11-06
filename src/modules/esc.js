const ESC_MAP = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
const ESC_RE = /[&<>"']/g;

export function escapeHtml(s) {
    if (s == null) return '';
    return String(s).replace(ESC_RE, (ch) => ESC_MAP[ch]);
}

export function escapeAttr(s) {
    return escapeHtml(s);
}
