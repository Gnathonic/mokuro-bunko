// S8 (B8.6, plan X15, B7.1 + R4.1) on a cloud a Bunko holds: the reader's series.json
// written into the series folder is an update request Bunko applies, then recompiles
// the file and publishes it. The reader sets the spine offset to 3, so Bunko's
// series.json says `spine_offset: 3`; it resets it to 0 (sent as an explicit 0, since
// Bunko reads an absent key as "keep the stored value"), so Bunko's file carries no
// override; and the reader's two syncs after that write series.json not once.
//
// "Bunko's series.json" is what Bunko itself serves for the path over WebDAV once it is
// Bunko's own compile (every volume entry carries `matched_page_count`, which only its
// compiler computes: a reader's series.json never has it, and before Bunko ingests a
// reader's write it serves that file as it found it in the cloud), and the cloud holds
// those same bytes as the one file of that name: the reader's own write of the file
// says nothing of what Bunko made of it.
import { createHash } from 'node:crypto';
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, davPath } from '../bunko.mjs';
import { cloudFiles } from '../cloud-direct.mjs';
import { bunkoSeenAlive, connect, recordReaderWrites, volumeNamed } from './shared.mjs';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

test('a spine offset set, then cleared, leaves series.json without one and is not re-sent', async ({ reader, rig }) => {
    test.setTimeout(300_000);
    const series = rig.library.series;
    const rel = `${series}/series.json`;
    const server = bunko(rig);
    const admin = rig.account('admin');
    /** Bunko's compiled series.json, once it is the cloud's one copy; else why not. */
    const published = async () => {
        const [served, cloud] = await Promise.all([
            server.bytes(davPath(series, 'series.json'), basic(admin.username, admin.password)),
            cloudFiles(rig),
        ]);
        const copies = cloud.filter((f) => f.path === rel);
        if (!served.bytes) return { state: `Bunko serves ${served.status}` };
        const file = JSON.parse(served.bytes.toString('utf8'));
        if (!file.volumes.length || !file.volumes.every((v) => Object.hasOwn(v, 'matched_page_count'))) {
            return { state: 'not Bunko compile' };
        }
        if (copies.length !== 1) return { state: `${copies.length} copies in the cloud` };
        if (copies[0].sha256 !== sha256(served.bytes)) return { state: 'unpublished' };
        return { file };
    };
    const spineOffset = async () => {
        const { file, state } = await published();
        if (!file) return state;
        return Object.hasOwn(file, 'spine_offset') ? file.spine_offset : 'absent';
    };

    await connect(reader, rig);
    await bunkoSeenAlive(reader);
    // Bunko compiled the series' file before anyone asked for an offset.
    await expect.poll(spineOffset, { timeout: 120_000 }).toBe('absent');
    // A volume of the series on this device, as on the shelf the offset is set from:
    // the reader writes series.json only for a series it holds something of.
    await reader.open((await volumeNamed(reader, series, 'Vol 1')).id);

    let writes = recordReaderWrites(reader.page, rig);
    await reader.setSpineOffset(series, 3);
    await reader.settleSeriesWrites();
    expect(await writes.names(), 'the offset went up as a series.json write').toContain('series.json');
    await expect.poll(spineOffset, { timeout: 120_000 }).toBe(3);

    writes = recordReaderWrites(reader.page, rig);
    await reader.setSpineOffset(series, 0);
    await reader.settleSeriesWrites();
    expect(await writes.names(), 'the reset went up as a series.json write').toContain('series.json');
    await expect.poll(spineOffset, { timeout: 120_000 }).toBe('absent');

    // Two more syncs, each to its end with every series.json write it queued.
    writes = recordReaderWrites(reader.page, rig);
    for (let i = 0; i < 2; i++) {
        expect(await reader.sync()).toEqual({ ok: true, errors: [] });
        await reader.idle();
        await reader.settleSeriesWrites();
    }
    expect(await writes.names(), 'what the reader wrote on the two syncs after the reset').not.toContain('series.json');
    expect(await spineOffset()).toBe('absent');
});
