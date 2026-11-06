// SC11 — client-side TODAY-ONLY helpers (US/Eastern calendar date).

/** @returns {string} YYYY-MM-DD in America/New_York */
export function todayEtDateStr(d = new Date()) {
    try {
        return new Intl.DateTimeFormat('en-CA', {
            timeZone: 'America/New_York',
            year: 'numeric',
            month: '2-digit',
            day: '2-digit',
        }).format(d);
    } catch {
        return d.toISOString().slice(0, 10);
    }
}

export function isTodaySessionRow(row) {
    if (!row) return false;
    const sid = String(row.session_date_et || '').trim();
    if (!sid) return false;
    return sid === todayEtDateStr();
}

export function filterTodayRows(rows) {
    if (!Array.isArray(rows)) return [];
    return rows.filter(isTodaySessionRow);
}
