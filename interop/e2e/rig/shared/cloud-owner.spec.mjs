// S3 with a cloud-root owner (`storage.library.progress_user`, the Readers setting's
// "Reading progress linked to the cloud") on a cloud a Bunko holds, the reader on the
// cloud itself. The owner's history, positions.json and `<Vol>.edit.mokuro` ARE the
// cloud's own files (`Core::owns_cloud_root`): the owner reads through WebDAV what the
// direct reader wrote, and its writes land in the cloud for that reader to merge. Every
// other account still sees none of it, and its own write of the same path stays its own.
import { createHash } from 'node:crypto';
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, davPath } from '../bunko.mjs';
import { cloudFiles } from '../cloud-direct.mjs';
import { monthFile, positionsOf } from '../history-accounts.mjs';
import { bunkoSeenAlive, cloudBytes, cloudJson, connect, volumeNamed } from './shared.mjs';

test.use({ needs: ['history', 'positions'] });

const PASSWORD = 'rig-owner-3';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** A correction layer of one page (the shape a reader's `edit` layer has). */
const layer = (text) =>
    JSON.stringify({ version: '0.2.5', pages: [{ img_path: '001.png', blocks: [{ lines: [text] }] }] });

test("the cloud-root owner's history, positions and correction are the cloud's", async ({ reader, rig }) => {
    test.setTimeout(300_000);
    const server = bunko(rig);
    const tag = Date.now().toString(36);
    const owner = await server.createUser(`s3o-owner-${tag}`, PASSWORD, 'registered');
    const other = await server.createUser(`s3o-other-${tag}`, PASSWORD, 'registered');
    const o = basic(owner.username, owner.password);
    const x = basic(other.username, other.password);
    const series = rig.library.series;
    const before = await server.storage();
    // Read per request: the next one is the owner's.
    await server.setReaders(before.serve_readers, owner.username);
    try {
        await connect(reader, rig);
        await bunkoSeenAlive(reader);
        expect((await reader.historyStatus()).enabled, 'history syncs beside this Bunko').toBe(true);
        const { id } = await reader.open((await volumeNamed(reader, series, 'Vol 1')).id);
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

        // The owner's writes land in the cloud: positions.json, and a correction.
        const mine = await positionsOf(rig, o);
        const edited = { ...mine, owner_tag: tag };
        expect([200, 201, 204]).toContain(await server.put(davPath('positions.json'), edited, o));
        expect((await cloudJson(rig, 'positions.json'))?.owner_tag, "the cloud's positions.json").toBe(tag);
        const edit = `${series}/Vol 1.edit.mokuro`;
        expect([201, 204]).toContain(await server.put(davPath(series, 'Vol 1.edit.mokuro'), layer(`owner ${tag}`), o));
        const inCloud = await cloudBytes(rig, edit);
        expect(inCloud?.toString('utf8'), `the cloud's ${edit}`).toBe(layer(`owner ${tag}`));

        // Another account: none of it, and its own correction stays off the cloud.
        expect((await server.get(davPath(series, 'Vol 1.edit.mokuro'), x)).status).toBe(404);
        expect(await positionsOf(rig, x)).toBeNull();
        expect([201, 204]).toContain(await server.put(davPath(series, 'Vol 1.edit.mokuro'), layer(`other ${tag}`), x));
        expect((await server.get(davPath(series, 'Vol 1.edit.mokuro'), x)).text).toBe(layer(`other ${tag}`));
        expect((await cloudBytes(rig, edit))?.toString('utf8'), 'the cloud keeps the owner correction').toBe(
            layer(`owner ${tag}`),
        );
        expect((await server.get(davPath(series, 'Vol 1.edit.mokuro'), o)).text).toBe(layer(`owner ${tag}`));
        expect(
            (await cloudFiles(rig)).filter((f) => f.path === edit).length,
            `${edit} in the cloud, once`,
        ).toBe(1);
    } finally {
        await server.setReaders(before.serve_readers, before.progress_user ?? null);
    }
});
