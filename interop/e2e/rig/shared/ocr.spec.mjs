// shared (§7.1, M7): Bunko OCRs a volume the reader uploaded straight to the cloud they
// share. The reader imports an image-only archive and backs it up; Bunko, holding the
// library's lease, finds it in the cloud and queues its OCR, which the rig's
// processor-fake claims and holds until the hold file exists (RIG_FAKEPROC_HOLD). While
// the marker is fresh the reader watches the volume on the queue file Bunko keeps in
// the cloud (`.mokuro-queue.json`): its card shows the job. Once the job is done, the
// queue file lists the volume in `done[]`, and the reader pulls the sidecar Bunko wrote
// beside the archive: the volume's OCR is on the device.
import { writeFileSync } from 'node:fs';
import { test, expect } from '../fixtures.mjs';
import { cloudFile } from '../cloud-direct.mjs';
import { bunkoSeenAlive, cloudJson, connect, tabAwayAndBack } from './shared.mjs';

const QUEUE_FILE = '.mokuro-queue.json';

test('Bunko OCRs a volume the reader uploaded: its job on the card, its sidecar pulled on done[]', async ({
    reader,
    rig,
}) => {
    test.setTimeout(420_000);
    const series = rig.library.series;
    const rel = `${series}/Vol 3`;
    await connect(reader, rig);
    const holder = await bunkoSeenAlive(reader);

    const { id } = await reader.importVolume(rig.upload(`${rel}.cbz`));
    expect((await reader.library()).volumes.find((v) => v.id === id)).toMatchObject({
        series,
        title: 'Vol 3',
        hasOcr: false,
    });
    expect(await reader.backupVolume(id)).toEqual({ cloudPath: `${rel}.cbz` });
    expect(await cloudFile(rig, `${rel}.cbz`)).toBeTruthy();
    expect(await cloudFile(rig, `${rel}.mokuro`)).toBeNull();
    await reader.showSeries(series);

    // Bunko found the upload and queued it: the card shows Bunko's job, read from the
    // queue file of the lease holder.
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                return reader.pendingOcr(id);
            },
            { timeout: 180_000, intervals: [1000, 2000, 5000] },
        )
        .toMatchObject({ title: 'Server OCR' });
    const queue = await cloudJson(rig, QUEUE_FILE);
    expect(queue?.instance_id).toBe(holder.instanceId);
    expect(queue.volumes.some((v) => v.series === series && v.volume === 'Vol 3')).toBe(true);

    // The processor finishes: the queue file lists the volume as done, the card
    // clears, and the sidecar Bunko wrote is the volume's OCR on this device.
    writeFileSync(rig.fakeprocHold, '');
    await expect
        .poll(
            async () => {
                const q = await cloudJson(rig, QUEUE_FILE);
                return (q?.done ?? []).some((d) => d.series === series && d.volume === 'Vol 3');
            },
            { timeout: 180_000, intervals: [1000, 2000] },
        )
        .toBe(true);
    expect(await cloudFile(rig, `${rel}.mokuro`)).toBeTruthy();
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                const v = (await reader.library()).volumes.find((x) => x.id === id);
                return { pending: await reader.pendingOcr(id), hasOcr: v?.hasOcr ?? false };
            },
            { timeout: 180_000, intervals: [1000, 2000, 5000] },
        )
        .toEqual({ pending: null, hasOcr: true });
});
