// S3 (B8.6, plan §1 "Per-user tree", X3) on a cloud a Bunko holds, the reader on the
// cloud itself: Bunko reports a version past 0.8.0 and keeps history per user (its
// lease marker says so), so the reader syncs its history (`history/<device>/<YYYY-MM>
// .events`) and its `positions.json` into the cloud folder. Nobody owns this cloud's
// root, so both are that cloud account's alone: no Bunko account is shown them, an
// account's write of the very same path is its own file (on Bunko's disk, the cloud's
// untouched), and none of it reaches the catalog, a series.json or the OCR queue.
import { createHash } from 'node:crypto';
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, davPath } from '../bunko.mjs';
import { cloudFiles } from '../cloud-direct.mjs';
import { metadataAfter, monthFile, positionsOf, seedSentinel } from '../history-accounts.mjs';
import { bunkoSeenAlive, cloudBytes, cloudJson, connect, volumeNamed } from './shared.mjs';

test.use({ needs: ['history', 'positions'] });

const PASSWORD = 'rig-history-2';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** A path of the reader's own tree: the history, or positions.json. */
const perUser = (rel) => rel === 'positions.json' || rel === 'history' || rel.startsWith('history/');

test("the reader's history and positions in the cloud are no Bunko account's", async ({ reader, rig }) => {
    test.setTimeout(300_000);
    const server = bunko(rig);
    const tag = Date.now().toString(36);
    const alice = await server.createUser(`s3c-alice-${tag}`, PASSWORD, 'registered');
    const bob = await server.createUser(`s3c-bob-${tag}`, PASSWORD, 'uploader');
    const series = rig.library.series;

    await connect(reader, rig);
    await bunkoSeenAlive(reader);
    expect((await reader.historyStatus()).enabled, 'history syncs beside this Bunko').toBe(true);
    const { id } = await reader.open((await volumeNamed(reader, series, 'Vol 1')).id);
    // A page turn, then back to the series page: a page view is recorded when it ends.
    await reader.page.waitForFunction(() => document.querySelector('.pageImage') !== null, null, {
        timeout: 30_000,
    });
    expect(await reader.setPage(id, 2)).toMatchObject({ page: 2 });
    await reader.showSeries(series);
    let month = null;
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                await reader.idle();
                month = (await cloudFiles(rig)).find((f) => monthFile(f.path)) ?? null;
                const positions = await cloudJson(rig, 'positions.json');
                return { month: month !== null, page: positions?.positions?.[id]?.[1] ?? null };
            },
            { timeout: 120_000, intervals: [1000, 2000, 5000] },
        )
        .toEqual({ month: true, page: 2 });
    const { device } = monthFile(month.path);
    expect((await reader.historyStatus()).devices).toContain(device);
    const monthBytes = await cloudBytes(rig, month.path);

    // As Bunko's accounts see the library: none of it.
    for (const who of [alice, bob]) {
        const a = basic(who.username, who.password);
        const tree = await server.propfind('/mokuro-reader/', a, 'infinity');
        expect(tree.status).toBe(207);
        const listed = tree.hrefs.map((h) => h.replace(/^\/mokuro-reader\/?/, '').replace(/\/$/, ''));
        expect(listed.filter(perUser), `what ${who.username} lists of the reader's own files`).toEqual([]);
        expect((await server.get(davPath(...month.path.split('/')), a)).status).toBe(404);
        expect(await positionsOf(rig, a), `${who.username}'s positions.json`).toBeNull();
    }

    // alice writes that very path: hers, on Bunko's disk; the cloud's file stays the reader's.
    const a = basic(alice.username, alice.password);
    expect([201, 405]).toContain(await server.mkcol(davPath('history'), a));
    expect([201, 405]).toContain(await server.mkcol(davPath('history', device), a));
    const hers = Buffer.from(`alice's own bytes ${tag}`);
    expect([201, 204]).toContain(await server.put(davPath(...month.path.split('/')), hers, a));
    expect(sha256((await server.bytes(davPath(...month.path.split('/')), a)).bytes)).toBe(sha256(hers));
    const copies = (await cloudFiles(rig)).filter((f) => f.path === month.path);
    expect(
        copies.map((f) => f.sha256),
        'the cloud holds the reader file, once',
    ).toEqual([sha256(monthBytes)]);

    // Bunko's metadata after a pass that saw it all: nothing of it.
    const sentinel = await seedSentinel(rig, expect, tag);
    const after = await metadataAfter(rig, sentinel);
    expect(after.titles).not.toContain('history');
    expect(after.queue, 'the OCR queue file').not.toContain('history');
    expect(after.queue, 'the OCR queue file').not.toContain(device);
    await expect
        .poll(async () => ((await cloudJson(rig, 'catalog.json'))?.series ?? []).map((s) => s.series_title), {
            timeout: 120_000,
        })
        .toContain(sentinel.series);
    const catalog = (await cloudJson(rig, 'catalog.json')).series.map((s) => s.series_title);
    expect(catalog, "the cloud's catalog.json").not.toContain('history');
    const inCloud = (await cloudFiles(rig)).map((f) => f.path);
    expect(
        inCloud.filter((p) => perUser(p) && p.endsWith('series.json')),
        'a series.json in the history',
    ).toEqual([]);
});
