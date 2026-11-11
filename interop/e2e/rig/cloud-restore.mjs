// A spec's restore of the cloud through Bunko (shared/cloud-owner.spec.mjs). Kept apart
// so the rule is tested without a server (cloud-restore.test.mjs).
//
// Bunko answers a DELETE from its index of the cloud, which follows the cloud by
// listing it. A 404 while the cloud still holds the file is a file Bunko has not listed
// (a write it has yet to list, the direct reader's straight to the cloud, or a listing
// gone stale), not a file gone: the restore asks again until Bunko finds it. How long
// it did not is kept, so a stale listing is noted even when it clears; a 404 that
// outlasts the wait stands, and the spec's own look at the cloud afterwards catches it.

/**
 * `deleter(target)` → `{status, unseenMs, gaveUpMs}`: `del(target)` (Bunko's status),
 * asked again while it answers 404 and `holds(target)`, every `step` ms. `waitMs` is one
 * budget for every target, from the first such 404: a restore of several files Bunko
 * has not seen waits once. `unseenMs`: from this target's first such 404 to its last
 * answer (null: none). `gaveUpMs`, when its 404 stood: how long the restore's wait had
 * run by then (a later target's own `unseenMs` is ~0 once the budget is spent).
 */
export function cloudDeleter({
    del,
    holds,
    waitMs,
    step = 2000,
    pause = (ms) => new Promise((r) => setTimeout(r, ms)),
    clock = Date.now,
}) {
    let until = null;
    return async (target) => {
        let since = null;
        for (;;) {
            const status = await del(target);
            const unseenMs = since === null ? null : clock() - since;
            if (status !== 404 || !(await holds(target))) return { status, unseenMs, gaveUpMs: null };
            since ??= clock();
            until ??= clock() + waitMs;
            if (clock() >= until) return { status, unseenMs: clock() - since, gaveUpMs: clock() - (until - waitMs) };
            await pause(Math.min(step, until - clock()));
        }
    };
}

/**
 * A `restore-unseen` annotation for `what` from a deleter's answer: when the 404 stood,
 * or when Bunko took `noteAfterMs` or more to list it (longer than its listing lags).
 */
export function unseenNote(what, { unseenMs, gaveUpMs }, noteAfterMs) {
    const s = (ms) => Math.round(ms / 1000);
    if (gaveUpMs !== null) {
        return `${what}: Bunko still answered 404 while the cloud held it, ${s(gaveUpMs)} s into the restore's wait`;
    }
    if (unseenMs !== null && unseenMs >= noteAfterMs) {
        return `${what}: Bunko answered 404 for ${s(unseenMs)} s while the cloud held it, then listed it`;
    }
    return null;
}
