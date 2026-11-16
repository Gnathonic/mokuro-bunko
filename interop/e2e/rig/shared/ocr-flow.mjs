// The shared OCR round trip (queue file v2, docs/design/queue-file-v2.md §12.4), for
// `shared/ocr.spec.mjs` (the reader straight on a cloud) and its local-folder variant
// (`shared/ocr-local-folder.spec.mjs`). The reader uploads an image-only `Vol 3`;
// Bunko, holding the library's lease, queues its OCR and processor-fake holds it.
//
// - The store holds no root `.mokuro-queue.json`; the marker carries `series_queue`
//   (`queue: null`), naming the per-series file.
// - Before the series page is open, the reader reads no queue file (the library grid,
//   the upload; a watch of the volume it uploaded reads its own series only).
// - Bunko writes `<Series>/.mokuro-series-queue.json` under its own `instance_id`:
//   the volume is in `vols`. Opening the series page reads that file and no other
//   series' file, and the card shows the job.
// - The processor finishes: the volume leaves `vols` with a `done` entry, Bunko's
//   sidecar is beside the archive, and the reader pulls it (the card clears, the
//   volume has OCR on this device).
import { writeFileSync } from 'node:fs';
import { expect } from '../fixtures.mjs';
import { cloudFile } from '../cloud-direct.mjs';
import { MARKER_FILE, ROOT_QUEUE_FILE, SERIES_QUEUE_FILE, recordQueueReads, seriesFolders } from '../queue-reads.mjs';
import { bunkoSeenAlive, cloudJson } from './shared.mjs';

/** The library on a cloud (cloudsim's tree, or rclone's served directory). */
export function cloudStore(rig) {
    return {
        json: (rel) => cloudJson(rig, rel),
        has: async (rel) => (await cloudFile(rig, rel)) !== null,
    };
}

/**
 * The round trip. `signIn()` connects the reader to the library; `store` reads the
 * library's files as the store holds them (`json(rel)`, `has(rel)`); `wire` checks the
 * reader's reads off its requests (false for a local folder, which reads with none).
 */
export async function ocrRoundTrip({ reader, rig, signIn, store, wire }) {
    const series = rig.library.series;
    const rel = `${series}/Vol 3`;
    const seriesFile = `${series}/${SERIES_QUEUE_FILE}`;
    const reads = wire ? recordQueueReads(reader.page, rig) : null;

    await signIn();
    const holder = await bunkoSeenAlive(reader);
    const marker = await store.json(MARKER_FILE);
    expect(marker?.instance_id).toBe(holder.instanceId);
    expect(marker.queue).toBeNull();
    expect(marker.series_queue).toMatchObject({ v: 2, file: SERIES_QUEUE_FILE });
    expect(await store.has(ROOT_QUEUE_FILE)).toBe(false);
    // The library grid: the marker read, and no queue file at all.
    if (reads) expect((await reads.reads()).filter((r) => r.file !== 'marker')).toEqual([]);

    const { id } = await reader.importVolume(rig.upload(`${rel}.cbz`));
    expect((await reader.library()).volumes.find((v) => v.id === id)).toMatchObject({
        series,
        title: 'Vol 3',
        hasOcr: false,
    });
    expect(await reader.backupVolume(id)).toEqual({ cloudPath: `${rel}.cbz` });
    expect(await store.has(`${rel}.cbz`)).toBe(true);
    expect(await store.has(`${rel}.mokuro`)).toBe(false);

    // Bunko found the upload and queued it: its series file lists the volume, written
    // under the holder's instance id, with no `live` block (that is the HTTP copy's).
    await expect
        .poll(async () => Object.keys((await store.json(seriesFile))?.vols ?? {}), {
            timeout: 180_000,
            intervals: [1000, 2000, 5000],
        })
        .toContain('Vol 3');
    const queued = await store.json(seriesFile);
    expect(queued).toMatchObject({ v: 2, instance_id: holder.instanceId, series });
    expect(queued).not.toHaveProperty('live');
    expect(queued.rows.length).toBeGreaterThan(0);

    // The series page: its one file read, the job on the card.
    const opened = reads?.mark() ?? 0;
    await reader.showSeries(series);
    await expect
        .poll(() => reader.pendingOcr(id), { timeout: 180_000, intervals: [1000, 2000, 5000] })
        .toMatchObject({ title: 'Server OCR' });

    // The processor finishes: the volume leaves the file with a `done` entry, and
    // Bunko's sidecar is beside the archive by then (the settle rule).
    writeFileSync(rig.fakeprocHold, '');
    await expect
        .poll(
            async () => {
                const q = await store.json(seriesFile);
                return { queued: 'Vol 3' in (q?.vols ?? {}), done: typeof q?.done?.['Vol 3'] };
            },
            { timeout: 180_000, intervals: [1000, 2000] },
        )
        .toEqual({ queued: false, done: 'number' });
    expect(await store.has(`${rel}.mokuro`)).toBe(true);

    // The reader sees it gone, lists that one series and pulls the sidecar.
    await expect
        .poll(
            async () => {
                const v = (await reader.library()).volumes.find((x) => x.id === id);
                return { pending: await reader.pendingOcr(id), hasOcr: v?.hasOcr ?? false };
            },
            { timeout: 240_000, intervals: [1000, 2000, 5000] },
        )
        .toEqual({ pending: null, hasOcr: true });

    // Never the root queue file, and still none on the store.
    expect(await store.has(ROOT_QUEUE_FILE)).toBe(false);
    if (reads) {
        const all = await reads.reads();
        expect(all.filter((r) => r.file === 'root')).toEqual([]);
        // Before the page was open only the watch of this device's upload read: its
        // series. With the page open, that series' file and no other.
        expect(seriesFolders(all)).toEqual([series]);
        expect(seriesFolders(await reads.reads(opened))).toEqual([series]);
        expect(all.some((r) => r.file === 'marker')).toBe(true);
    }
}
