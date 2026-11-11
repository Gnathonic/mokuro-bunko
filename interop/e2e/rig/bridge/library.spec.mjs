// bridge (§7.1): the reader reads and writes a cloud library through Bunko, which holds
// the library's lease and alone talks to the cloud. A volume opened is downloaded
// through Bunko, its archive the cloud's byte for byte, and the reader never reaches
// the cloud itself; the shared corrections an editor publishes go up through Bunko (a
// bridge PUT of `<Vol>.curated.mokuro`) and land in the cloud, while the account's own
// edit stays with Bunko (nobody owns this cloud's root, so a personal overlay is never
// written into it).
import { test, expect } from '../fixtures.mjs';
import { cloudFile } from '../cloud-direct.mjs';
import {
    cloudOrigins,
    firstLines,
    landedInCloud,
    newAccount,
    openVolume,
    servedAndStored,
    sha256,
    volumeNamed,
    connect,
} from './bridge.mjs';

test('a volume is read through Bunko from the cloud library', async ({ reader, rig }) => {
    const origins = cloudOrigins(rig);
    const reached = [];
    reader.page.on('request', (r) => {
        if (origins.includes(new URL(r.url()).origin)) reached.push(`${r.method()} ${r.url()}`);
    });
    const account = await newAccount(rig, 'bridge-read', 'registered');
    await connect(reader, rig, account);
    const listed = await volumeNamed(reader, rig, 'Vol 1');
    expect(listed.hasOcr).toBe(false);

    const { id } = await reader.open(listed.id);
    expect(await volumeNamed(reader, rig, 'Vol 1')).toMatchObject({
        id,
        hasOcr: true,
        cloudPath: `${rig.library.series}/Vol 1.cbz`,
    });
    // A page of it on screen: its image (MangaPage's background), out of the archive
    // Bunko served, and it decodes.
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

    // What Bunko serves is the cloud's archive, byte for byte.
    const { served, cloud } = await servedAndStored(rig, account, `${rig.library.series}/Vol 1.cbz`);
    expect(served.status).toBe(200);
    expect(cloud, 'the archive in the cloud').toBeTruthy();
    expect(sha256(served.bytes)).toBe(cloud.sha256);
    // Only Bunko talked to the cloud.
    expect(reached).toEqual([]);
});

test.describe('the shared corrections', () => {
    test.use({ needs: ['edit-layers-personal', 'edit-layers-curated'] });

    test('published through Bunko, they land in the cloud; the edit stays with Bunko', async ({ reader, rig }) => {
        const text = `橋の訂正 ${Date.now().toString(36)}`;
        const series = rig.library.series;
        const account = await newAccount(rig, 'bridge-put');
        const id = await openVolume(reader, rig, account);
        expect(await reader.editFirstBlock(id, 1, text)).toBeTruthy();
        // The account's own edit goes up to Bunko on a sync, never into the cloud.
        await expect
            .poll(
                async () => {
                    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                    return (await servedAndStored(rig, account, `${series}/Vol 1.edit.mokuro`)).served.status;
                },
                { timeout: 60_000, intervals: [500, 1000, 2000] },
            )
            .toBe(200);
        expect(await cloudFile(rig, `${series}/Vol 1.edit.mokuro`)).toBeNull();

        const published = await reader.publishCurated(id);
        expect(published.outcome).toBe('published');
        const doc = await landedInCloud(rig, account, `${series}/Vol 1.curated.mokuro`, (d) =>
            (firstLines(d) ?? []).includes(text),
        );
        expect(firstLines(doc)).toEqual([text]);
        // Still no personal overlay in the cloud.
        expect(await cloudFile(rig, `${series}/Vol 1.edit.mokuro`)).toBeNull();
    });
});
