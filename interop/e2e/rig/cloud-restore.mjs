// A spec's restore of the cloud through Bunko (shared/cloud-owner.spec.mjs). Kept apart
// so the rule is tested without a server (cloud-restore.test.mjs).
//
// Bunko answers a DELETE from its index of the cloud, which follows the cloud by
// listing it. A 404 while the cloud still holds the file is a file Bunko has not listed
// (a write it has yet to list, the direct reader's straight to the cloud, or a listing
// gone stale), not a file gone: the restore asks again until Bunko finds it. How long
// it did not is kept, so a stale listing is noted even when it clears; a 404 that
// outlasts the wait stands, and the spec's own look at the cloud afterwards catches it.
//
// A cloud that holds several files of one name (Drive, MEGA) can hold several copies of
// a per-user file, and every copy counts: a look at the tree keyed by path sees one of
// them, and the next spec reads them all (S1's merge moves each copy of
// `volume-data.json`, and the reader folds them). Two things leave such a copy behind
// (reader PR #368, 2026-11-13), and both are Bunko product issues for their own fix, not
// the spec's: on MEGA, Bunko writes each per-user root op of the cloud-root owner TWICE
// (the same op and bytes, a second node 3-8 s after the first), and its Replace of a
// per-user file supersedes only the copy its index holds, leaving a copy another client
// wrote at the path beside it. Until they are fixed a restore waits for Bunko's writes to
// land (`untilQuiet`) and removes the copies left over (`copyProblems`), noting each one
// (`restore-copies`) so the double write stays visible in every run that meets it.

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

/** The reader-root files that are an account's own (bunko-core reader_tree.rs). */
export const PER_USER_FILES = ['volume-data.json', 'profiles.json', 'goals.json', 'positions.json'];

/** Whether `rel` (a path below a reader root) is one of an account's own files. */
export const perUserPath = (rel) => PER_USER_FILES.includes(rel) || rel.startsWith('history/');

/**
 * `files` (`[{path, id, sha256, modified}]`, every copy) as `{path → copies}`, the copies
 * oldest first, for `paths` only.
 */
export function copiesByPath(files, paths = () => true) {
    const at = (f) => Date.parse(f.modified ?? '') || 0;
    const out = {};
    for (const f of files) if (paths(f.path)) (out[f.path] ??= []).push(f);
    for (const copies of Object.values(out)) copies.sort((a, b) => at(a) - at(b) || String(a.id).localeCompare(b.id));
    return out;
}

/**
 * What the tree `now` (`{path → copies}`) lacks against `want` (`{path → sha256}`, one
 * copy of each, and nothing else): `[{path, why, extra}]`. `extra` says what to remove:
 * `'copies'` when every copy is the wanted bytes and there are just too many of them
 * (Bunko's doubled write: removing any but one leaves the right bytes), `'all'` when
 * a copy holds other bytes (a foreign copy beside the one Bunko indexes, or a stale
 * one: everything goes and the wanted bytes go back up once), `null` when nothing is
 * there to remove (the file is missing).
 */
export function copyProblems(want, now) {
    const out = [];
    for (const [path, sha] of Object.entries(want)) {
        const copies = now[path] ?? [];
        const n = copies.length;
        if (n === 1 && copies[0].sha256 === sha) continue;
        if (n === 0) out.push({ path, why: 'gone', extra: null });
        else if (copies.every((c) => c.sha256 === sha)) out.push({ path, why: `${n} copies`, extra: 'copies' });
        else out.push({ path, why: n === 1 ? 'changed' : `${n} copies, other bytes`, extra: 'all' });
    }
    for (const [path, copies] of Object.entries(now)) {
        if (path in want) continue;
        out.push({ path, why: copies.length === 1 ? 'added' : `added, ${copies.length} copies`, extra: 'all' });
    }
    return out;
}

/** `copyProblems` as the lines a spec reports: `[]` once the tree is `want`. */
export function copyLines(want, now) {
    return copyProblems(want, now).map(({ path, why }) => `${why} ${path}`);
}

/**
 * `read()` every `step` ms until its answer has held still for `quietMs`: `{value,
 * waitedMs}`, `value` the latest answer. What must hold still is `key(value)` (by
 * default the whole answer), compared as JSON. Bunko's writes reach the cloud on
 * its own time (the second of a doubled per-user write 3-8 s after the first, see
 * above), so a look at the cloud that must see the last of them waits until the tree
 * holds still. Throws after `timeoutMs`.
 */
export async function untilQuiet(
    read,
    {
        quietMs,
        timeoutMs,
        key = (v) => v,
        step = 1000,
        pause = (ms) => new Promise((r) => setTimeout(r, ms)),
        clock = Date.now,
    },
) {
    const start = clock();
    let value = await read();
    let was = JSON.stringify(key(value));
    let since = clock();
    for (;;) {
        if (clock() - since >= quietMs) return { value, waitedMs: clock() - start };
        if (clock() - start >= timeoutMs) {
            throw new Error(`the cloud still changed ${timeoutMs} ms on (last ${clock() - since} ms quiet): ${was}`);
        }
        await pause(step);
        value = await read();
        const is = JSON.stringify(key(value));
        if (is !== was) {
            was = is;
            since = clock();
        }
    }
}
