let _audioCtx = null;

function ctx() {
    if (!_audioCtx) _audioCtx = new (window.AudioContext || window.webkitAudioContext)();
    return _audioCtx;
}

function tone(freq, startAt, dur, gainVal) {
    const c = ctx();
    const osc = c.createOscillator();
    const gain = c.createGain();
    osc.type = 'sine';
    osc.frequency.value = freq;
    gain.gain.value = gainVal;
    osc.connect(gain);
    gain.connect(c.destination);
    osc.start(c.currentTime + startAt);
    osc.stop(c.currentTime + startAt + dur);
}

export function playFillBeep() {
    try {
        tone(800, 0, 0.2, 0.15);
    } catch (_) { /* Web Audio not available */ }
}

// F3b/F5 — Frontier/APEX GO alert. Two rising tones, distinct from the
// single fill beep: at seconds-scale setups the operator's reaction
// time matters.
export function playGoAlert() {
    try {
        tone(880, 0, 0.12, 0.18);
        tone(1320, 0.14, 0.18, 0.18);
    } catch (_) { /* Web Audio not available */ }
}
