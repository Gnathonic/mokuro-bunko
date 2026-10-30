// S3 (B8.6, plan §1 "Per-user tree"): reading history and positions across accounts. A
// reader turns pages; its history (`history/<device>/<YYYY-MM>.events`) and its
// `positions.json` are the account's own. Two accounts, one after the other:
// - alice reads: her history and positions are in her tree (Bunko's
//   `users/<u>/`), never in the library (nor, through a bridge, in the cloud);
// - bob reads: his reader imports none of alice's devices nor her position, his own
//   files go to his tree, and his write of alice's very history path keeps both;
// - none of it reaches the catalog, a series.json or the OCR queue: after a series
//   seeded later shows up in Bunko's catalog.json (so a metadata pass ran since),
//   none of those name the history tree or a device.
// shared/history-accounts.spec.mjs has the same for a reader on the cloud itself.
import { createHash, randomUUID } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { basic, bunko, davPath } from './bunko.mjs';
import { cloudFiles } from './cloud-direct.mjs';

const PASSWORD = 'rig-history-1';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** `history/<device>/<YYYY-MM>.events`, reader-root relative: `{device, month}`, or null. */
export function monthFile(rel) {
    const m = rel.match(/^history\/([^/]+)\/(\d{4}-\d{2})\.events$/);
    return m ? { device: m[1], month: m[2] } : null;
}

/** The month files `auth` lists in its history tree (reader-root relative). */
export async function listedMonths(rig, auth) {
    const r = await bunko(rig).propfind(davPath('history') + '/', auth, 'infinity');
    if (r.status === 404) return [];
    if (r.status !== 207) throw new Error(`PROPFIND history: ${r.status}`);
    return r.hrefs.map((h) => h.replace(/^\/mokuro-reader\//, '')).filter((rel) => monthFile(rel));
}

/** `positions.json` as `auth` reads it: the parsed file, or null for a 404. */
export async function positionsOf(rig, auth) {
    const r = await bunko(rig).get(davPath('positions.json'), auth);
    if (r.status === 404) return null;
    if (r.status !== 200) throw new Error(`GET positions.json: ${r.status}`);
    return JSON.parse(r.text);
}

/**
 * A series seeded by the admin after the history was written (an archive and its own
 * OCR, so it owes no OCR job): `{series, wait}`, `wait()` resolving once Bunko's
 * catalog.json lists it, so a metadata pass has run since the history was there.
 */
export async function seedSentinel(rig, expect, tag) {
    const server = bunko(rig);
    const admin = rig.account('admin');
    const auth = basic(admin.username, admin.password);
    const series = `S3 Sentinel ${tag}`;
    const source = await server.bytes(davPath(rig.library.series, 'Vol 1.cbz'), auth);
    expect(source.status).toBe(200);
    const pages = JSON.parse((await server.get(davPath(rig.library.series, 'Vol 1.mokuro'), auth)).text).pages;
    expect(await server.mkcol(davPath(series), auth)).toBe(201);
    const archive = Buffer.from(source.bytes);
    // Other bytes than the seeded archive (its zip comment, the last bytes).
    Buffer.from('s3').copy(archive, archive.length - 2);
    expect(await server.put(davPath(series, 'Vol 1.cbz'), archive, auth)).toBe(201);
    const doc = {
        version: '0.2.2',
        title: series,
        title_uuid: randomUUID(),
        volume: 'Vol 1',
        volume_uuid: randomUUID(),
        pages,
    };
    expect([201, 204]).toContain(await server.put(davPath(series, 'Vol 1.mokuro'), JSON.stringify(doc), auth));
    return {
        series,
        async wait() {
            let catalog = null;
            await expect
                .poll(
                    async () => {
                        const r = await server.get(davPath('catalog.json'), auth);
                        catalog = r.status === 200 ? JSON.parse(r.text) : null;
                        return (catalog?.series ?? []).map((s) => s.series_title);
                    },
                    { timeout: 120_000, intervals: [500, 1000, 2000] },
                )
                .toContain(series);
            return catalog;
        },
    };
}

/**
 * What the metadata and the queue say, after a pass since the history was written:
 * the catalog's series titles, and the queue file's text (empty when there is none).
 */
export async function metadataAfter(rig, sentinel) {
    const admin = rig.account('admin');
    const auth = basic(admin.username, admin.password);
    const catalog = await sentinel.wait();
    const queue = await bunko(rig).get('/mokuro-reader/.mokuro-queue.json', auth);
    const historySeries = await bunko(rig).get(davPath('history', 'series.json'), auth);
    return {
        titles: (catalog?.series ?? []).map((s) => s.series_title),
        queue: queue.status === 200 ? queue.text : '',
        historySeriesStatus: historySeries.status,
    };
}

/**
 * Define S3's specs (serial, one fresh device each) for a library read through Bunko.
 * `inCloud`: Bunko keeps the library in the rig's cloud, which is checked too.
 */
export function historyAcrossAccounts(test, expect, { inCloud }) {
    test.describe.configure({ mode: 'serial' });
    test.use({ needs: ['history', 'positions'] });

    /** Shared by the specs below, in order. */
    const s = { ready: false };
    const auth = (a) => basic(a.username, a.password);
    /** A file of an account's own tree on Bunko's disk (`users/<u>/<rel>`). */
    const ownFile = (rig, account, rel) => path.join(rig.workdir, 'bunko', 'users', account.username, rel);
    /** The library's own copies: what Bunko's disk or the cloud holds under the reader's root. */
    async function libraryPaths(rig) {
        if (inCloud) return (await cloudFiles(rig)).map((f) => f.path);
        const root = path.join(rig.workdir, 'bunko', 'library');
        return ['history', 'positions.json'].filter((n) => existsSync(path.join(root, n)));
    }

    async function connect(reader, rig, account) {
        const { sync } = await reader.connectWebDAV({
            url: rig.bunkoUrl,
            username: account.username,
            password: account.password,
        });
        expect(sync).toEqual({ ok: true, errors: [] });
        expect((await reader.historyStatus()).enabled, 'history syncs with this server').toBe(true);
    }

    /** Signed in as `account`, the seeded Vol 1 open at `page` and synced: its id. */
    async function readTo(reader, rig, account, page) {
        await connect(reader, rig, account);
        const { volumes } = await reader.library();
        const v = volumes.find((x) => x.series === rig.library.series && x.title === 'Vol 1');
        expect(v, `${rig.library.series}/Vol 1 in the library`).toBeTruthy();
        const { id } = await reader.open(v.id);
        // The reader on screen, then a page turn, then back to the series page: a page
        // view is recorded in the history when it ends.
        await reader.page.waitForFunction(() => document.querySelector('.pageImage') !== null, null, {
            timeout: 30_000,
        });
        expect(await reader.setPage(id, page)).toMatchObject({ page });
        await reader.showSeries(rig.library.series);
        // Each sync to its end: the history month and positions.json go up on it.
        await expect
            .poll(
                async () => {
                    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                    await reader.idle();
                    const months = await listedMonths(rig, auth(account));
                    const positions = await positionsOf(rig, auth(account));
                    return { months: months.length > 0, page: positions?.positions?.[id]?.[1] ?? null };
                },
                { timeout: 90_000, intervals: [1000, 2000, 5000] },
            )
            .toEqual({ months: true, page });
        return id;
    }

    test.beforeAll(async ({ rig }) => {
        const server = bunko(rig);
        s.tag = Date.now().toString(36);
        // Registered: the least role that keeps a history and positions.
        s.alice = await server.createUser(`s3-alice-${s.tag}`, PASSWORD, 'registered');
        s.bob = await server.createUser(`s3-bob-${s.tag}`, PASSWORD, 'registered');
        s.ready = true;
    });

    test("alice's history and positions go to her own tree, not the library", async ({ reader, rig }) => {
        test.skip(!s.ready, 'needs the accounts');
        test.setTimeout(180_000);
        s.volumeId = await readTo(reader, rig, s.alice, 2);
        const months = await listedMonths(rig, auth(s.alice));
        s.month = months[0];
        s.aliceDevice = monthFile(s.month).device;
        const own = (await reader.historyStatus()).devices;
        expect(own, "alice's devices").toContain(s.aliceDevice);
        const positions = await positionsOf(rig, auth(s.alice));
        expect(positions.positions[s.volumeId]).toEqual([expect.any(Number), 2, expect.any(Number), s.aliceDevice]);
        s.aliceMonth = (await bunko(rig).bytes(davPath(...s.month.split('/')), auth(s.alice))).bytes;
        s.alicePositions = sha256(readFileSync(ownFile(rig, s.alice, 'positions.json')));

        // On Bunko's disk in her tree, byte for byte; nowhere in the library.
        expect(sha256(readFileSync(ownFile(rig, s.alice, s.month)))).toBe(sha256(s.aliceMonth));
        const library = await libraryPaths(rig);
        expect(library.filter((p) => p === 'positions.json' || p === 'history' || p.startsWith('history/'))).toEqual(
            [],
        );
    });

    test("bob's reader takes none of alice's; his write of her path keeps both", async ({ reader, rig }) => {
        test.skip(!s.aliceDevice, "needs alice's history");
        test.setTimeout(180_000);
        const server = bunko(rig);
        const b = auth(s.bob);
        // Nothing of alice's at her own paths, as bob asks.
        expect((await server.get(davPath(...s.month.split('/')), b)).status).toBe(404);
        expect(await positionsOf(rig, b)).toBeNull();

        await readTo(reader, rig, s.bob, 3);
        const devices = (await reader.historyStatus()).devices;
        expect(devices, "bob's reader's devices").not.toContain(s.aliceDevice);
        expect(await reader.progress(s.volumeId)).toMatchObject({ page: 3 });
        const months = await listedMonths(rig, b);
        expect(months.map((m) => monthFile(m).device)).not.toContain(s.aliceDevice);
        const bobs = await positionsOf(rig, b);
        expect(bobs.positions[s.volumeId][1]).toBe(3);
        expect(bobs.positions[s.volumeId][3]).not.toBe(s.aliceDevice);
        // alice's file is as she left it.
        expect(sha256(readFileSync(ownFile(rig, s.alice, 'positions.json')))).toBe(s.alicePositions);

        // bob writes alice's very history path: two files, one each.
        const [, device] = s.month.split('/');
        expect([201, 405]).toContain(await server.mkcol(davPath('history'), b));
        expect([201, 405]).toContain(await server.mkcol(davPath('history', device), b));
        const mine = Buffer.from(`bob's own bytes ${s.tag}`);
        expect([201, 204]).toContain(await server.put(davPath(...s.month.split('/')), mine, b));
        expect(sha256((await server.bytes(davPath(...s.month.split('/')), b)).bytes)).toBe(sha256(mine));
        const alices = await server.bytes(davPath(...s.month.split('/')), auth(s.alice));
        expect(sha256(alices.bytes), "alice's file after bob's write").toBe(sha256(s.aliceMonth));
        expect(sha256(readFileSync(ownFile(rig, s.bob, s.month)))).toBe(sha256(mine));
        s.bobDone = true;
    });

    test('none of it reaches the catalog, a series.json or the queue', async ({ rig }) => {
        test.skip(!s.bobDone, 'needs both histories');
        test.setTimeout(180_000);
        const sentinel = await seedSentinel(rig, expect, s.tag);
        const after = await metadataAfter(rig, sentinel);
        expect(after.titles).not.toContain('history');
        expect(after.historySeriesStatus, 'history/series.json').toBe(404);
        for (const needle of ['history', s.aliceDevice]) {
            expect(after.queue, 'the OCR queue file').not.toContain(needle);
        }
        expect(JSON.stringify(after.titles), "the catalog's series").not.toContain(s.aliceDevice);
        const library = await libraryPaths(rig);
        expect(library.filter((p) => p === 'positions.json' || p === 'history' || p.startsWith('history/'))).toEqual(
            [],
        );
    });
}
