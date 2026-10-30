// shared (§7.1, M7): who writes `series.json` and `catalog.json` on a cloud a Bunko
// holds. Bunko compiles both while its lease marker is fresh, and the reader writes
// neither, even for a series it just uploaded. The reader judges the marker by what it
// saw: one that has not moved for 2 x ttl is stale, so a Bunko that stopped (and wrote
// nothing more) hands the files back to the reader 2 x ttl after its last heartbeat; one
// that released the library (the Storage panel's Disconnect) hands them back at once.
//
// Each upload is a new series, so a reader that writes the files has something new to
// write into both. The last spec disconnects the suite's Bunko from the cloud for good:
// this file must stay the suite's last (Playwright runs the files in name order).
import { test, expect } from '../fixtures.mjs';
import { bunko } from '../bunko.mjs';
import { cloudFile } from '../cloud-direct.mjs';
import { bunkoSeenAlive, cloudJson, connect, recordReaderWrites, tabAwayAndBack } from './shared.mjs';

test.describe.configure({ mode: 'serial' });

const METADATA = ['catalog.json', 'series.json'];

/** Import `<series>/Vol 1.cbz` and back it up to the end of its run: what the reader wrote. */
async function uploadNewSeries(reader, rig, series) {
    const writes = recordReaderWrites(reader.page, rig);
    const { id } = await reader.importVolume(rig.upload(`${series}/Vol 1.cbz`));
    expect(await reader.backupVolume(id)).toEqual({ cloudPath: `${series}/Vol 1.cbz` });
    expect(await cloudFile(rig, `${series}/Vol 1.cbz`)).toBeTruthy();
    const wrote = await writes.names();
    // The record sees the reader's writes at all: the archive is one.
    expect(wrote).toContain('Vol 1.cbz');
    return wrote;
}

/** The series `catalog.json` lists, as the cloud stores it. */
async function catalogSeries(rig) {
    return ((await cloudJson(rig, 'catalog.json'))?.series ?? []).map((s) => s.series_title);
}

/** Poll the reader's verdict, keeping it looking, until it no longer finds Bunko compiling. */
async function untilReaderCompiles(reader, timeout) {
    const seen = [];
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                const compiles = (await reader.controlState()).serverCompilesMetadata;
                seen.push({ at: Date.now(), compiles });
                return compiles;
            },
            { timeout, intervals: [1000] },
        )
        .toBe(false);
    return seen;
}

test('while the marker is fresh the reader writes neither series.json nor catalog.json', async ({ reader, rig }) => {
    test.setTimeout(300_000);
    const series = 'Rig Fresh';
    await connect(reader, rig);
    await bunkoSeenAlive(reader);
    const wrote = await uploadNewSeries(reader, rig, series);
    expect(wrote.filter((n) => METADATA.includes(n))).toEqual([]);
    expect((await reader.controlState()).serverCompilesMetadata).toBe(true);
    // Bunko compiled both for the new series.
    await expect.poll(() => catalogSeries(rig), { timeout: 120_000 }).toContain(series);
    await expect.poll(async () => cloudFile(rig, `${series}/series.json`), { timeout: 120_000 }).toBeTruthy();
});

test('a Bunko that stopped hands them back 2 x ttl after its last heartbeat', async ({ reader, rig }) => {
    const ttl = rig.lease.ttl_s;
    test.setTimeout((4 * ttl + 240) * 1000);
    const series = 'Rig Resumed';
    await connect(reader, rig);
    await bunkoSeenAlive(reader);

    // SIGINT: Bunko writes no `released`; its marker just stops moving.
    const { stopped_at_ms: stoppedAt } = await rig.stopBunko();
    const seen = await untilReaderCompiles(reader, (2 * ttl + 60) * 1000);
    const flippedAfter = (seen.at(-1).at - stoppedAt) / 1000;
    test.info().annotations.push({ type: 'stale-after-s', description: String(flippedAfter) });
    // Its last heartbeat was at most ttl / 3 before the stop; the reader looked every
    // second, and calls the marker stale 2 x ttl after the last move it saw.
    expect(flippedAfter).toBeGreaterThan(2 * ttl - ttl / 3 - 3);
    expect(flippedAfter).toBeLessThan(2 * ttl + 10);
    expect(seen.filter((s) => s.compiles).length).toBeGreaterThan(0);

    const wrote = await uploadNewSeries(reader, rig, series);
    expect(wrote).toEqual(expect.arrayContaining(METADATA));
    // Bunko is down: what the cloud holds is the reader's.
    expect(await catalogSeries(rig)).toContain(series);
    expect(await cloudFile(rig, `${series}/series.json`)).toBeTruthy();

    await rig.startBunko();
    await bunkoSeenAlive(reader);
});

test('a Bunko that released the library hands them back at once', async ({ reader, rig }) => {
    const ttl = rig.lease.ttl_s;
    test.setTimeout(300_000);
    const series = 'Rig Released';
    await connect(reader, rig);
    await bunkoSeenAlive(reader);

    const answer = await bunko(rig).disconnectStorage();
    expect(answer.report).toMatchObject({ released: true });
    const releasedAt = Date.now();
    const seen = await untilReaderCompiles(reader, ttl * 1000);
    test.info().annotations.push({
        type: 'released-after-s',
        description: String((seen.at(-1).at - releasedAt) / 1000),
    });
    // Well inside the 2 x ttl a stopped Bunko would get: the reader's next look.
    expect((seen.at(-1).at - releasedAt) / 1000).toBeLessThan(ttl / 2);
    expect((await reader.controlState()).marker?.holder ?? null).toBeNull();

    const wrote = await uploadNewSeries(reader, rig, series);
    expect(wrote).toEqual(expect.arrayContaining(METADATA));
    expect(await catalogSeries(rig)).toContain(series);
    expect(await cloudFile(rig, `${series}/series.json`)).toBeTruthy();
});
