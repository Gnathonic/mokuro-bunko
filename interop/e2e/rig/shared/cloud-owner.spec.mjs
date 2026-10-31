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
import { monthFile, positionsOf, seedSentinel } from '../history-accounts.mjs';
import { bunkoSeenAlive, cloudBytes, cloudJson, connect, recordReaderWrites, volumeNamed } from './shared.mjs';

test.use({ needs: ['history', 'positions', 'edit-layers-personal'] });

const PASSWORD = 'rig-owner-3';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** The reader-root files that are an account's own (bunko-core reader_tree.rs). */
const PER_USER_FILES = ['volume-data.json', 'profiles.json', 'goals.json', 'positions.json'];
const ownTree = (rel) => PER_USER_FILES.includes(rel) || rel.startsWith('history/');

/** The cloud's own tree as it stands: `{path → bytes}` (of several at a path, the last written). */
async function ownTreeNow(rig) {
    const paths = [...new Set((await cloudFiles(rig)).map((f) => f.path).filter(ownTree))];
    return Object.fromEntries(await Promise.all(paths.map(async (p) => [p, await cloudBytes(rig, p)])));
}

/**
 * What differs from the cloud as the spec found it (`found`, ownTreeNow's), and every
 * file left under the spec's own series: `[]` once the cloud is as it was.
 */
async function leftBehind(rig, found, series) {
    const now = await ownTreeNow(rig);
    const out = [];
    for (const [p, bytes] of Object.entries(found)) {
        if (!(p in now)) out.push(`gone ${p}`);
        else if (sha256(now[p]) !== sha256(bytes)) out.push(`changed ${p}`);
    }
    for (const p of Object.keys(now)) if (!(p in found)) out.push(`added ${p}`);
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

/**
 * Put the cloud back as the spec found it (`found`, ownTreeNow's): the other account's
 * overlay and the spec's series removed (the admin), the owner's tree written back
 * through Bunko as the owner (the cloud's own files are its). Answers what failed.
 */
async function restoreCloud(rig, { found, own, owner, other, admin }) {
    const server = bunko(rig);
    const del = (path, auth) => settled(async () => (await server.request('DELETE', path, { auth })).status);
    const problems = [];
    const step = async (what, call, ok) => {
        try {
            const status = await call();
            if (!ok.includes(status)) problems.push(`${what}: ${status}`);
        } catch (e) {
            problems.push(`${what}: ${e.message}`);
        }
    };
    await step("the other account's overlay", () => del(davPath(own, 'Vol 1.edit.mokuro'), other), [204, 404]);
    await step(`the series ${own}`, () => del(`${davPath(own)}/`, admin), [204, 404]);
    let now = {};
    try {
        now = await ownTreeNow(rig);
    } catch (e) {
        problems.push(`the cloud's own tree: ${e.message}`);
    }
    const devices = (paths) => new Set(paths.filter((p) => p.startsWith('history/')).map((p) => p.split('/')[1]));
    const hadDevices = devices(Object.keys(found));
    // A device's folder the spec's readers made goes whole; a file added beside the
    // ones that were there goes alone.
    for (const device of devices(Object.keys(now))) {
        if (hadDevices.has(device)) continue;
        await step(`history/${device}/`, () => del(`${davPath('history', device)}/`, owner), [204, 404]);
    }
    for (const p of Object.keys(now)) {
        const segments = p.split('/');
        if (p in found || (segments[0] === 'history' && !hadDevices.has(segments[1]))) continue;
        await step(p, () => del(davPath(...segments), owner), [204, 404]);
    }
    if (hadDevices.size === 0 && devices(Object.keys(now)).size > 0) {
        await step('history/', () => del(`${davPath('history')}/`, owner), [204, 404]);
    }
    for (const [p, bytes] of Object.entries(found)) {
        if (p in now && sha256(now[p]) === sha256(bytes)) continue;
        const segments = p.split('/');
        if (segments[0] === 'history') {
            for (let i = 1; i < segments.length; i++) {
                const dir = segments.slice(0, i);
                await step(`MKCOL ${dir.join('/')}`, () => settled(() => server.mkcol(davPath(...dir), owner)), [
                    201, 405,
                ]);
            }
        }
        await step(p, () => settled(() => server.put(davPath(...segments), bytes, owner)), [200, 201, 204]);
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
    const found = await ownTreeNow(rig);
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
                    month = (await cloudFiles(rig)).find((f) => monthFile(f.path) && !(f.path in found)) ?? null;
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
                            (f) => monthFile(f.path) && !(f.path in found) && monthFile(f.path).device !== directDevice,
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
            // Bunko's writes reach the cloud on its own time.
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
