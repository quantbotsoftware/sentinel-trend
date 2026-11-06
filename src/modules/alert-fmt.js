export function summarizeActionableAlert(markers) {
    if (!Array.isArray(markers) || markers.length === 0) return '';
    const labels = markers
        .map((m) => {
            if (!m || !m.kind) return '';
            if (m.kind === 'Entry' && m.direction === 'Long') {
                return m.display_only === true ? 'SKIP (vetoed)' : 'GO BUY (gates ok)';
            }
            if (m.kind === 'Exit') return 'FSM SELL';
            return '';
        })
        .filter(Boolean);
    return labels.join(', ');
}
