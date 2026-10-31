// A spec's restore of the cloud through Bunko (shared/cloud-owner.spec.mjs). Kept apart
// so the rule is tested without a server (cloud-restore.test.mjs).
//
// Bunko answers a DELETE from its index of the cloud, which follows the cloud by
// listing it. A 404 while the cloud still holds the file is a write Bunko has not
// listed yet (the direct reader's, straight to the cloud), not a file gone: the restore
// asks again until Bunko finds it. A 404 that outlasts the wait stands, so a Bunko that
// never sees the file is still caught by the spec's own look at the cloud afterwards.

/**
 * `deleter(target)` → `{status, unseenMs}`: `del(target)` (Bunko's status), asked again
 * while it answers 404 and `holds(target)`, every `step` ms. `waitMs` is one budget for
 * every target, from the first such 404: a restore of several files Bunko has not seen
 * waits once. `unseenMs`, when the budget ran out: how long this target went unseen.
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
        const since = clock();
        for (;;) {
            const status = await del(target);
            if (status !== 404 || !(await holds(target))) return { status, unseenMs: null };
            until ??= clock() + waitMs;
            if (clock() >= until) return { status, unseenMs: clock() - since };
            await pause(Math.min(step, until - clock()));
        }
    };
}
