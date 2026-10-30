// S5 (B8.6, R3.2) on a cloud a Bunko holds: one series under two spellings. The cloud
// first holds `kingdom/Vol 2` (another device's upload), which the reader downloads
// and reads a page of; then an OLDER `Kingdom/` appears with `Vol 1`. Bunko merges the
// younger spelling into the older one by itself (`Kingdom/Vol 2.cbz`, the loser
// trashed once empty). After the reader's next sync its row for Vol 2 has taken the
// cloud's spelling (`Kingdom`), under the same volume id and with the same progress,
// and nothing ever goes back into `kingdom/`: no folder of that spelling is made and
// nothing is uploaded into the one Bunko trashed, so Bunko has no second merge to make.
//
// The seeds are made the way another client would make them: `/_sim/node`, which can
// date a folder in the past, or rclone's WebDAV (MKCOL, PUT; never its directory on
// disk, which its directory cache would not see for minutes), where the older spelling
// wins on its path, the order Bunko breaks a tie of unknown creation times by. Each volume
// carries its OCR sidecar: an image-only one would be queued for OCR, and a job the
// suite's held processor-fake claims holds the merge off. OneDrive holds no two
// spellings of one name (Graph answers 409), so the scenario has nothing to merge there.
import { existsSync } from 'node:fs';
import path from 'node:path';
import { test, expect } from '../fixtures.mjs';
import { basic, bunko } from '../bunko.mjs';
import { LIBRARY_FOLDER, cloudFiles } from '../cloud-direct.mjs';
import { walksAfter } from '../rename-ocr.mjs';
import { bunkoSeenAlive, connect } from './shared.mjs';

test.use({ needs: ['case-merge-rename'] });

const OLDER = 'Kingdom';
const YOUNGER = 'kingdom';
/** The volume id `kingdom/Vol 2.mokuro` carries (the rig's CASE_VARIANTS, browser.rs). */
const VOL2_ID = '00000000-0000-4000-8000-000000000052';
const OK = { ok: true, errors: [] };

/**
 * `folder` under the library folder with `<volume>.cbz` and `<volume>.mokuro`, made as
 * another client makes them; `created` (RFC 3339) dates the folder and its files.
 */
async function seed(rig, folder, volume, created) {
    const files = [`${volume}.cbz`, `${volume}.mokuro`].map((name) => ({
        name,
        base64: rig.upload(`${folder}/${name}`).base64,
    }));
    if (rig.provider === 'rclone') {
        const { url, username, password } = rig.webdav;
        const dav = async (method, rel, body) => {
            const at = `${url.replace(/\/$/, '')}/${[LIBRARY_FOLDER, ...rel].map(encodeURIComponent).join('/')}`;
            const r = await fetch(at, { method, headers: { authorization: basic(username, password) }, body });
            if (!r.ok) throw new Error(`${method} ${rel.join('/')} on rclone: ${r.status}`);
        };
        await dav('MKCOL', [folder]);
        for (const f of files) await dav('PUT', [folder, f.name], Buffer.from(f.base64, 'base64'));
        return;
    }
    const times = created ? { created, modified: created } : {};
    const node = (body) => rig.sim('POST', 'node', { provider: rig.provider, account: rig.simAccount, ...body });
    const { id } = await node({ parent_path: LIBRARY_FOLDER, name: folder, folder: true, ...times });
    for (const f of files) await node({ parent_id: id, name: f.name, content_base64: f.base64, ...times });
}

/** Every node cloudsim has had, trashed ones included. */
const tree = (rig) =>
    rig.sim('GET', `tree?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&include_trashed=1`);

/** The case merges Bunko has recorded into the series folder `winner`. */
async function mergesInto(rig, winner) {
    const { recent_merges: merges = [] } = await bunko(rig).storage();
    return merges.filter((m) => m.winner === winner);
}

/** Whether `kingdom/` is gone from the cloud: trashed (cloudsim) or deleted (rclone). */
async function youngerGone(rig) {
    if (rig.provider === 'rclone') return !existsSync(path.join(rig.workdir, 'rclone-root', LIBRARY_FOLDER, YOUNGER));
    const rows = await tree(rig);
    const library = rows.find((r) => r.folder && r.name === LIBRARY_FOLDER && !r.trashed);
    return rows.filter((r) => r.parent_id === library?.id && r.name === YOUNGER).every((r) => r.trashed);
}

/**
 * Every write the page (and its workers) sends to the cloud, as `{method, path,
 * destination}`: what an rclone WebDAV write names (cloudsim's are read off its tree).
 */
function recordCloudWrites(page, rig) {
    const origins = new Set(
        [rig.cloudsimUrl, rig.preauthUrl, rig.rcloneUrl].filter(Boolean).map((u) => new URL(u).origin),
    );
    const writes = [];
    page.on('request', (request) => {
        const url = new URL(request.url());
        if (!origins.has(url.origin) || ['GET', 'HEAD', 'OPTIONS', 'PROPFIND'].includes(request.method())) return;
        const dest = request.headers().destination;
        writes.push({
            method: request.method(),
            path: decodeURIComponent(url.pathname),
            destination: dest ? decodeURIComponent(new URL(dest).pathname) : null,
        });
    });
    return writes;
}

test('a case-variant merge: the reader takes the older spelling, keeps the volume and its progress, writes nothing into the younger', async ({
    reader,
    rig,
}) => {
    test.skip(rig.provider === 'microsoft', 'OneDrive holds no two spellings of one name (Graph answers 409)');
    test.setTimeout(480_000);
    await connect(reader, rig);
    await bunkoSeenAlive(reader);
    expect(await mergesInto(rig, OLDER)).toEqual([]);

    // Another device's upload of `kingdom/Vol 2`: the reader downloads it and reads on.
    await seed(rig, YOUNGER, 'Vol 2');
    let listed = null;
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual(OK);
                listed = (await reader.library()).volumes.find((v) => v.series === YOUNGER && v.title === 'Vol 2');
                return listed?.cloudPath ?? null;
            },
            { timeout: 120_000, intervals: [2000] },
        )
        .toBe(`${YOUNGER}/Vol 2.cbz`);
    const { id } = await reader.open(listed.id);
    expect(id).toBe(VOL2_ID);
    expect(await reader.setPage(id, 2)).toEqual({ page: 2, completed: false });
    expect(await reader.sync()).toEqual(OK);
    await reader.idle();
    const progress = await reader.progress(id);
    expect((await reader.library()).volumes.filter((v) => v.id === id)).toMatchObject([
        { series: YOUNGER, title: 'Vol 2', cloudPath: `${YOUNGER}/Vol 2.cbz` },
    ]);

    // The older spelling appears; Bunko merges the younger into it and trashes it.
    await seed(rig, OLDER, 'Vol 1', '2020-01-01T00:00:00Z');
    await expect
        .poll(
            async () => {
                const done = (await mergesInto(rig, OLDER)).filter((m) => m.state === 'done');
                const paths = (await cloudFiles(rig)).map((f) => f.path);
                const under = (folder) => paths.filter((p) => p.startsWith(`${folder}/`)).sort();
                return {
                    merges: done.map((m) => m.losers),
                    older: under(OLDER),
                    younger: under(YOUNGER),
                    gone: await youngerGone(rig),
                };
            },
            { timeout: 240_000, intervals: [1000, 2000] },
        )
        .toEqual({
            merges: [[YOUNGER]],
            older: expect.arrayContaining([`${OLDER}/Vol 1.cbz`, `${OLDER}/Vol 2.cbz`, `${OLDER}/Vol 2.mokuro`]),
            younger: [],
            gone: true,
        });

    // The reader's sync after the merge, and one more with everything it queued.
    const before = rig.provider === 'rclone' ? null : new Map((await tree(rig)).map((r) => [r.id, r]));
    const writes = recordCloudWrites(reader.page, rig);
    expect(await reader.sync()).toEqual(OK);
    await reader.idle();
    // The adoption runs on the sync's listing, beside the sync: its outcome is the row.
    await expect
        .poll(async () => (await reader.library()).volumes.filter((v) => v.id === id), { timeout: 60_000 })
        .toMatchObject([{ series: OLDER, title: 'Vol 2', cloudPath: `${OLDER}/Vol 2.cbz` }]);
    expect(await reader.progress(id)).toEqual(progress);
    expect(progress.page).toBe(2);
    expect(await reader.sync()).toEqual(OK);
    await reader.idle();
    await reader.settleSeriesWrites();
    // Two Bunko walks: room for the merge pass a recreated `kingdom/` would get.
    await walksAfter(expect, rig, 2);

    expect((await reader.library()).volumes.filter((v) => v.series === YOUNGER)).toEqual([]);
    if (rig.provider === 'rclone') {
        const into = (p) => p === `/${LIBRARY_FOLDER}/${YOUNGER}` || p?.startsWith(`/${LIBRARY_FOLDER}/${YOUNGER}/`);
        expect(writes.filter((w) => into(w.path) || into(w.destination))).toEqual([]);
        expect(await youngerGone(rig)).toBe(true);
    } else {
        // Every node made since the merge, by anyone: none is a `kingdom` folder; and
        // nothing was made in, or moved into, the trashed one.
        const rows = await tree(rig);
        const younger = new Set(rows.filter((r) => r.folder && r.name === YOUNGER).map((r) => r.id));
        const inYounger = (list) =>
            list
                .filter((r) => younger.has(r.parent_id))
                .map((r) => r.id)
                .sort();
        expect(rows.filter((r) => !before.has(r.id) && r.name === YOUNGER)).toEqual([]);
        expect(inYounger(rows)).toEqual(inYounger([...before.values()]));
    }
    expect((await mergesInto(rig, OLDER)).length, 'case merges into Kingdom').toBe(1);
});
