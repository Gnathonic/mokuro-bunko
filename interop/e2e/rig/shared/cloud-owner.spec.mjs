// S3 with a cloud-root owner (`storage.library.progress_user`, the Readers setting's
// "Reading progress linked to the cloud") on a cloud a Bunko holds, a reader on the
// cloud itself. The owner's history, positions.json and `<Vol>.edit.mokuro` ARE the
// cloud's own files (`Core::owns_cloud_root`): the owner reads through WebDAV what the
// direct reader wrote; the owner's own reader, signed in to Bunko, merges it and its page
// turn, history and correction land in the cloud; and the direct reader's next sync
// merges those in turn, dropping none of them. Every other account still sees none of
// it, and its own write of the same path stays its own.
// The correction goes on a series the spec seeds itself, and the spec leaves the cloud's
// own tree (the per-user root files and the history) as it found it, so no spec after
// it reads any of this state.
import { createHash } from 'node:crypto';
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, davPath } from '../bunko.mjs';
import { cloudFiles } from '../cloud-direct.mjs';
import {
    cloudDeleter,
    copiesByPath,
    copyLines,
    copyProblems,
    perUserPath,
    unseenNote,
    untilQuiet,
} from '../cloud-restore.mjs';
import { monthFile, positionsOf, seedSentinel } from '../history-accounts.mjs';
import { bunkoSeenAlive, cloudBytes, cloudJson, connect, recordReaderWrites, volumeNamed } from './shared.mjs';

test.use({ needs: ['history', 'positions', 'edit-layers-personal'] });

const PASSWORD = 'rig-owner-3';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/**
 * The cloud's own tree (the per-user root files and the history) as it stands, every
 * copy at a path: `{path → [{path, id, sha256, modified}]}`, oldest first.
 */
async function ownCopies(rig) {
    return copiesByPath(await cloudFiles(rig), perUserPath);
}

/**
 * The cloud's own tree as the spec found it: `{bytes, want}`, `bytes` the last written
 * copy at each path (what a restore puts back), `want` its sha256 (one copy of each).
 * Copies an earlier spec left beside it are noted: the restore leaves one.
 */
async function ownTreeFound(rig) {
    const copies = await ownCopies(rig);
    const extra = Object.entries(copies).filter(([, c]) => c.length > 1);
    if (extra.length) {
        test.info().annotations.push({
            type: 'found-copies',
            description: JSON.stringify(Object.fromEntries(extra)),
        });
    }
    const read = async (p) => [p, await cloudBytes(rig, p)];
    const bytes = Object.fromEntries(await Promise.all(Object.keys(copies).map(read)));
    const want = Object.fromEntries(Object.entries(bytes).map(([p, b]) => [p, sha256(b)]));
    return { bytes, want };
}

/** How long the cloud's own tree must hold still: past Bunko's re-put delay (3-8 s). */
const QUIET_MS = 15_000;
/** The longest a restore waits for it to (Bunko's listing lags 2 s; an op retries). */
const QUIET_WAIT_MS = 120_000;

/** The cloud's own tree once nothing writes it any more (cloud-restore.mjs `untilQuiet`). */
async function ownCopiesQuiet(rig) {
    const { value } = await untilQuiet(() => ownCopies(rig), { quietMs: QUIET_MS, timeoutMs: QUIET_WAIT_MS });
    return value;
}

/**
 * What differs from the cloud as the spec found it (`found`, ownTreeFound's): every copy
 * at a per-user path counts (a second copy is one Bunko's double write or a foreign copy
 * left beside its own), and every file left under the spec's own series: `[]` once the
 * cloud is as it was.
 */
async function leftBehind(rig, found, series) {
    const out = copyLines(found.want, await ownCopiesQuiet(rig));
    for (const f of await cloudFiles(rig)) if (f.path.startsWith(`${series}/`)) out.push(`left ${f.path}`);
    return out;
}

/** `call()` (a request's status) again while Bunko answers 503 (it lists a changed file again). */
async function settled(call) {
    for (let i = 0; ; i++) {
        const status = await call();
        if (status !== 503 || i >= 30) return status;
        await new Promise((r) => setTimeout(r, 1000));
    }
}

/** How long a restore waits for Bunko to list files the cloud holds (cloud-restore.mjs). */
const UNSEEN_WAIT_MS = 60_000;
/** Unlisted this long is no lag (the rig's Bunko polls every 2 s), so it is noted. */
const UNSEEN_NOTE_MS = 10_000;
/** Rounds of removing left-over copies before the restore gives up (each waits QUIET_MS). */
const COPY_ROUNDS = 4;

/**
 * Put the cloud back as the spec found it (`found`, ownTreeFound's): the other account's
 * overlay and the spec's series removed (the admin), the owner's tree written back
 * through Bunko as the owner (the cloud's own files are its), and then, once Bunko's own
 * writes have landed, one copy of each file and no more. Answers what failed.
 */
async function restoreCloud(rig, { found, own, owner, other, admin }) {
    const server = bunko(rig);
    const del = (path, auth) => settled(async () => (await server.request('DELETE', path, { auth })).status);
    // The cloud's file at `segments` (with `folder`, the folder and all below it).
    const deleterFor = () =>
        cloudDeleter({
            del: ({ segments, folder, auth }) => del(davPath(...segments) + (folder ? '/' : ''), auth),
            holds: async ({ segments, folder }) => {
                const rel = segments.join('/');
                return (await cloudFiles(rig)).some((f) => (folder ? f.path.startsWith(`${rel}/`) : f.path === rel));
            },
            waitMs: UNSEEN_WAIT_MS,
        });
    let deleter = deleterFor();
    const delCloud = async (segments, auth, { folder = false } = {}) => {
        const got = await deleter({ segments, folder, auth });
        const note = unseenNote(segments.join('/') + (folder ? '/' : ''), got, UNSEEN_NOTE_MS);
        if (note) test.info().annotations.push({ type: 'restore-unseen', description: note });
        return got.status;
    };
    const problems = [];
    const step = async (what, call, ok) => {
        try {
            const status = await call();
            if (!ok.includes(status)) problems.push(`${what}: ${status}`);
        } catch (e) {
            problems.push(`${what}: ${e.message}`);
        }
    };
    // The owner's file at `p` put back as found (its history folders made on the way).
    const putBack = async (p) => {
        const segments = p.split('/');
        if (segments[0] === 'history') {
            for (let i = 1; i < segments.length; i++) {
                const dir = segments.slice(0, i);
                await step(`MKCOL ${dir.join('/')}`, () => settled(() => server.mkcol(davPath(...dir), owner)), [
                    201, 405,
                ]);
            }
        }
        await step(p, () => settled(() => server.put(davPath(...segments), found.bytes[p], owner)), [200, 201, 204]);
    };
    await step("the other account's overlay", () => del(davPath(own, 'Vol 1.edit.mokuro'), other), [204, 404]);
    await step(`the series ${own}`, () => delCloud([own], admin, { folder: true }), [204, 404]);
    let now = {};
    try {
        now = await ownCopies(rig);
    } catch (e) {
        problems.push(`the cloud's own tree: ${e.message}`);
    }
    const devices = (paths) => new Set(paths.filter((p) => p.startsWith('history/')).map((p) => p.split('/')[1]));
    const hadDevices = devices(Object.keys(found.want));
    // A device's folder the spec's readers made goes whole; a file added beside the
    // ones that were there goes alone.
    for (const device of devices(Object.keys(now))) {
        if (hadDevices.has(device)) continue;
        await step(`history/${device}/`, () => delCloud(['history', device], owner, { folder: true }), [204, 404]);
    }
    for (const p of Object.keys(now)) {
        const segments = p.split('/');
        if (p in found.want || (segments[0] === 'history' && !hadDevices.has(segments[1]))) continue;
        await step(p, () => delCloud(segments, owner), [204, 404]);
    }
    if (hadDevices.size === 0 && devices(Object.keys(now)).size > 0) {
        await step('history/', () => delCloud(['history'], owner, { folder: true }), [204, 404]);
    }
    for (const [p, sha] of Object.entries(found.want)) {
        const copies = now[p] ?? [];
        if (copies.length === 1 && copies[0].sha256 === sha) continue;
        await putBack(p);
    }

    // Left-over copies (cloud-restore.mjs: a Bunko product issue, not this spec's). Bunko
    // writes the owner's file twice on MEGA, the second node 3-8 s after the first, and
    // its Replace supersedes only the copy it indexes, leaving the direct reader's beside
    // it; a DELETE through Bunko likewise takes the copy it indexes. So: wait until the
    // tree holds still, then take away a copy at a time (a doubled write of the right
    // bytes: all but one; a foreign copy: every one, and the found bytes go back up
    // once), and look again.
    for (let round = 1; round <= COPY_ROUNDS; round++) {
        try {
            now = await ownCopiesQuiet(rig);
        } catch (e) {
            problems.push(`the cloud's own tree held still: ${e.message}`);
            break;
        }
        const wrong = copyProblems(found.want, now);
        if (wrong.length === 0) break;
        const copiesNow = Object.fromEntries(wrong.map((w) => [w.path, now[w.path]]));
        const description = JSON.stringify({ round, wrong, copiesNow });
        test.info().annotations.push({ type: 'restore-copies', description });
        if (round === COPY_ROUNDS) {
            problems.push(`left after ${COPY_ROUNDS} rounds: ${wrong.map((w) => `${w.why} ${w.path}`).join(', ')}`);
            break;
        }
        // Each round has its own wait for Bunko to list the copy it has not indexed yet.
        deleter = deleterFor();
        for (const w of wrong) {
            const copies = now[w.path] ?? [];
            const removals = w.extra === 'copies' ? copies.length - 1 : w.extra === 'all' ? copies.length : 0;
            for (let i = 0; i < removals; i++) {
                await step(`copy ${i + 1} of ${w.path}`, () => delCloud(w.path.split('/'), owner), [204, 404]);
            }
            if (w.path in found.want && w.extra !== 'copies') await putBack(w.path);
        }
    }
    return problems;
}

const OK = { ok: true, errors: [] };

/** The reader on screen with its volume open: a page view is recorded when it ends. */
const pageShown = (reader) =>
    reader.page.waitForFunction(() => document.querySelector('.pageImage') !== null, null, { timeout: 30_000 });

/** The first block's text of a `.mokuro` document's first page. */
const firstLines = (doc) => doc?.pages?.[0]?.blocks?.[0]?.lines ?? null;

/** A correction layer of one page (the shape a reader's `edit` layer has). */
const layer = (text) =>
    JSON.stringify({ version: '0.2.5', pages: [{ img_path: '001.png', blocks: [{ lines: [text] }] }] });

test("the cloud-root owner's history, positions and correction are the cloud's", async ({
    reader,
    rig,
    anotherDevice,
}) => {
    test.setTimeout(480_000);
    const server = bunko(rig);
    const tag = Date.now().toString(36);
    const owner = await server.createUser(`s3o-owner-${tag}`, PASSWORD, 'registered');
    const other = await server.createUser(`s3o-other-${tag}`, PASSWORD, 'registered');
    const o = basic(owner.username, owner.password);
    const x = basic(other.username, other.password);
    const admin = rig.account('admin');
    const ad = basic(admin.username, admin.password);
    const series = rig.library.series;
    const own = `S3 Owner ${tag}`;
    const found = await ownTreeFound(rig);
    const before = await server.storage();
    // Read per request: the next one is the owner's.
    await server.setReaders(before.serve_readers, owner.username);
    let owned = null;
    try {
        const seeded = await seedSentinel(rig, expect, tag, own);
        await connect(reader, rig);
        await bunkoSeenAlive(reader);
        expect((await reader.historyStatus()).enabled, 'history syncs beside this Bunko').toBe(true);
        const { id } = await reader.open((await volumeNamed(reader, series, 'Vol 1')).id);
        await pageShown(reader);
        expect(await reader.setPage(id, 2)).toMatchObject({ page: 2 });
        await reader.showSeries(series);
        let month = null;
        await expect
            .poll(
                async () => {
                    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                    await reader.idle();
                    // This reader's: an earlier spec's reader may have left its own.
                    month = (await cloudFiles(rig)).find((f) => monthFile(f.path) && !(f.path in found.want)) ?? null;
                    const positions = await cloudJson(rig, 'positions.json');
                    return { month: month !== null, page: positions?.positions?.[id]?.[1] ?? null };
                },
                { timeout: 120_000, intervals: [1000, 2000, 5000] },
            )
            .toEqual({ month: true, page: 2 });
        const monthPath = davPath(...month.path.split('/'));

        // The owner reads, through WebDAV, the very files the direct reader wrote (once
        // Bunko's view of the cloud has caught up with them).
        const want = sha256(await cloudBytes(rig, month.path));
        await expect
            .poll(
                async () => {
                    // A status until then (WebDAV: 503, Retry-After, while Bunko lists
                    // the cloud's changed file again).
                    const read = await server.bytes(monthPath, o);
                    const p = await server.get(davPath('positions.json'), o);
                    return {
                        month: read.status === 200 ? sha256(read.bytes) : read.status,
                        page: p.status === 200 ? (JSON.parse(p.text)?.positions?.[id]?.[1] ?? null) : p.status,
                    };
                },
                { message: `the owner's ${month.path} and positions.json`, timeout: 120_000, intervals: [1000, 2000, 5000] },
            )
            .toEqual({ month: want, page: 2 });
        // Nobody else does.
        expect((await server.get(monthPath, x)).status).toBe(404);
        expect(await positionsOf(rig, x)).toBeNull();

        // The owner's own reader, signed in to Bunko: it takes in the direct reader's
        // history and position, and its page turn, its history and a correction of the
        // spec's own volume land in the cloud beside them (its writes go through Bunko's
        // If-Match passthrough).
        const directDevice = monthFile(month.path).device;
        owned = await anotherDevice();
        const signedIn = await owned.connectWebDAV({
            url: rig.bunkoUrl,
            username: owner.username,
            password: owner.password,
        });
        expect(signedIn.sync).toEqual(OK);
        expect((await owned.historyStatus()).enabled, "the owner's history syncs through Bunko").toBe(true);
        const ownId = (await volumeNamed(owned, own, 'Vol 1')).id;
        expect(ownId).toBe(seeded.volumeId);
        await owned.open(ownId);
        await pageShown(owned);
        expect(await owned.setPage(ownId, 3)).toMatchObject({ page: 3 });
        const text = `持ち主の訂正 ${tag}`;
        await owned.editFirstBlock(ownId, 1, text);
        await owned.showSeries(own);
        const edit = `${own}/Vol 1.edit.mokuro`;
        let ownerMonth = null;
        await expect
            .poll(
                async () => {
                    expect(await owned.sync()).toEqual(OK);
                    await owned.idle();
                    ownerMonth =
                        (await cloudFiles(rig)).find(
                            (f) =>
                                monthFile(f.path) &&
                                !(f.path in found.want) &&
                                monthFile(f.path).device !== directDevice,
                        ) ?? null;
                    const positions = (await cloudJson(rig, 'positions.json'))?.positions;
                    return {
                        direct: positions?.[id]?.[1] ?? null,
                        owner: positions?.[ownId]?.[1] ?? null,
                        ownerMonth: ownerMonth !== null,
                        edit: firstLines(await cloudJson(rig, edit)),
                        directDevice: (await owned.historyStatus()).devices.includes(directDevice),
                    };
                },
                { message: "the owner's reader's writes in the cloud", timeout: 120_000, intervals: [1000, 2000, 5000] },
            )
            .toEqual({ direct: 2, owner: 3, ownerMonth: true, edit: [text], directDevice: true });
        const ownerDevice = monthFile(ownerMonth.path).device;
        // The owner's reader is done: what it wrote stays as it is from here on.
        await owned.idle();
        await owned.logout();
        const ownerEdit = await cloudBytes(rig, edit);
        const ownerMonthBytes = await cloudBytes(rig, ownerMonth.path);

        // Another account: none of it, and its own correction stays off the cloud.
        expect((await server.get(davPath(own, 'Vol 1.edit.mokuro'), x)).status).toBe(404);
        expect(await positionsOf(rig, x)).toBeNull();
        expect([201, 204]).toContain(await server.put(davPath(own, 'Vol 1.edit.mokuro'), layer(`other ${tag}`), x));
        expect((await server.get(davPath(own, 'Vol 1.edit.mokuro'), x)).text).toBe(layer(`other ${tag}`));
        expect(sha256(await cloudBytes(rig, edit)), 'the cloud keeps the owner correction').toBe(sha256(ownerEdit));
        expect((await server.get(davPath(own, 'Vol 1.edit.mokuro'), o)).text).toBe(ownerEdit.toString('utf8'));
        expect(
            (await cloudFiles(rig)).filter((f) => f.path === edit).length,
            `${edit} in the cloud, once`,
        ).toBe(1);

        // The direct reader merges what the owner wrote: a page turn of its own goes up
        // beside the owner's position, it takes in the owner's device, and the owner's
        // history and correction are in the cloud as the owner left them.
        const writes = recordReaderWrites(reader.page, rig);
        await reader.open(id);
        await pageShown(reader);
        expect(await reader.setPage(id, 4)).toMatchObject({ page: 4 });
        await reader.showSeries(series);
        await expect
            .poll(
                async () => {
                    expect(await reader.sync()).toEqual(OK);
                    await reader.idle();
                    const positions = (await cloudJson(rig, 'positions.json'))?.positions;
                    return {
                        direct: positions?.[id]?.[1] ?? null,
                        owner: positions?.[ownId]?.[1] ?? null,
                        ownerDevice: (await reader.historyStatus()).devices.includes(ownerDevice),
                    };
                },
                { message: "the direct reader's merge of the owner's files", timeout: 120_000, intervals: [1000, 2000, 5000] },
            )
            .toEqual({ direct: 4, owner: 3, ownerDevice: true });
        // One more sync, to the end: nothing of the owner's goes on a later one either.
        expect(await reader.sync()).toEqual(OK);
        await reader.idle();
        expect((await cloudJson(rig, 'positions.json'))?.positions?.[ownId]?.[1], "the owner's position").toBe(3);
        expect(sha256(await cloudBytes(rig, ownerMonth.path) ?? Buffer.alloc(0)), `the owner's ${ownerMonth.path}`).toBe(
            sha256(ownerMonthBytes),
        );
        expect(sha256(await cloudBytes(rig, edit) ?? Buffer.alloc(0)), `the owner's ${edit}`).toBe(sha256(ownerEdit));
        expect(await writes.names(), 'what the direct reader wrote').not.toContain('Vol 1.edit.mokuro');
    } finally {
        try {
            // Nothing writes meanwhile: the readers sign out first.
            await reader.logout().catch(() => {});
            await owned?.logout().catch(() => {});
            const problems = await restoreCloud(rig, { found, own, owner: o, other: x, admin: ad });
            expect.soft(problems, 'putting the cloud back').toEqual([]);
            // Bunko's writes reach the cloud on its own time: each look waits until the
            // cloud's own tree holds still, and counts every copy.
            let left = null;
            for (const until = Date.now() + 60_000; Date.now() < until; ) {
                left = await leftBehind(rig, found, own);
                if (left.length === 0) break;
                await new Promise((r) => setTimeout(r, 1000));
            }
            expect.soft(left, 'the cloud as the spec found it').toEqual([]);
        } finally {
            await server.setReaders(before.serve_readers, before.progress_user ?? null);
        }
    }
});
