const _listeners = new Map();

export function on(event, fn) {
    if (!_listeners.has(event)) {
        _listeners.set(event, new Set());
    }
    _listeners.get(event).add(fn);
}

export function off(event, fn) {
    const set = _listeners.get(event);
    if (set) {
        set.delete(fn);
        if (set.size === 0) _listeners.delete(event);
    }
}

export function emit(event, data) {
    const set = _listeners.get(event);
    if (!set) return;
    for (const fn of set) {
        try {
            fn(data);
        } catch (err) {
            console.error(`[bus] Error in listener for "${event}":`, err);
        }
    }
}
