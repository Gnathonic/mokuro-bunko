// direct (§7.1): the reader alone on the cloud reads and writes its library. A volume
// opened is downloaded from the cloud and its pages shown; its progress goes up to the
// cloud's volume-data.json once the reading session ends, and its cover beside its
// archive; another device signed in to the same cloud reads the progress back.
import { test, expect } from '../fixtures.mjs';
import { cloudFile, signIn } from '../cloud-direct.mjs';

const VOLUME_DATA = 'volume-data.json';

async function connect(reader, rig) {
    const { sync } = await signIn(reader, rig);
    expect(sync).toEqual({ ok: true, errors: [] });
}

async function volumeNamed(reader, rig, title) {
    const { volumes } = await reader.library();
    const v = volumes.find((x) => x.series === rig.library.series && x.title === title);
    expect(v, `${rig.library.series}/${title} in the library`).toBeTruthy();
    return v;
}

test('a volume opened is downloaded from the cloud and its pages shown', async ({ reader, rig }) => {
    await connect(reader, rig);
    const listed = await volumeNamed(reader, rig, 'Vol 1');
    expect(listed.hasOcr).toBe(false);

    const { id } = await reader.open(listed.id);
    expect(await volumeNamed(reader, rig, 'Vol 1')).toMatchObject({
        id,
        hasOcr: true,
        cloudPath: `${rig.library.series}/Vol 1.cbz`,
    });
    // A page of it is on screen: its image (MangaPage's background), out of the
    // downloaded archive, and it decodes.
    const bg = await reader.page.waitForFunction(
        () => {
            if (!location.hash.includes('/reader/')) return null;
            const el = document.querySelector('.pageImage');
            const m = el && getComputedStyle(el).backgroundImage.match(/^url\("(blob:[^"]+)"\)$/);
            return m ? m[1] : null;
        },
        null,
        { timeout: 30_000 },
    );
    const size = await reader.page.evaluate(
        async (src) => {
            const img = new Image();
            img.src = src;
            await img.decode();
            return [img.naturalWidth, img.naturalHeight];
        },
        await bg.jsonValue(),
    );
    expect(size[0]).toBeGreaterThan(0);
    expect(size[1]).toBeGreaterThan(0);
});

test('progress and the cover go up to the cloud; another device reads the progress back', async ({
    reader,
    rig,
    anotherDevice,
}) => {
    await connect(reader, rig);
    const { id } = await reader.open((await volumeNamed(reader, rig, 'Vol 1')).id);
    const before = await cloudFile(rig, VOLUME_DATA);
    // A page other than the one the cloud already has (an earlier spec's).
    const page = ((await reader.progress(id)).page % 4) + 1;
    expect(await reader.setPage(id, page)).toMatchObject({ page });
    // While the reader is open the turn travels in positions.json and volume-data.json
    // waits for the end of the session (the reader's volume-data-deferral.ts): out of
    // the reader, the next sync writes it.
    await reader.showSeries(rig.library.series);
    await reader.page.waitForFunction(() => !location.hash.includes('/reader/'), null, { timeout: 30_000 });
    await reader.idle();
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });

    const after = await cloudFile(rig, VOLUME_DATA);
    expect(after, `${VOLUME_DATA} in the cloud`).toBeTruthy();
    expect(after.sha256).not.toBe(before?.sha256);
    // The cover the device made of the volume, beside its archive (sidecar-backfill).
    await expect
        .poll(async () => (await cloudFile(rig, `${rig.library.series}/Vol 1.webp`))?.size ?? 0, {
            timeout: 30_000,
        })
        .toBeGreaterThan(0);

    const other = await anotherDevice();
    expect(await other.library()).toEqual({ volumes: [] });
    await connect(other, rig);
    expect(await other.progress(id)).toMatchObject({ page });
});
