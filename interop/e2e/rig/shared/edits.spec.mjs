// shared (§7.1, M7, D4): on a cloud a Bunko holds, the main `<Vol>.mokuro` is model
// output. A reader's edit goes up as the edit layer `<Vol>.edit.mokuro` beside it, with
// an `edited` block and no `ocr_engine`, and the reader never writes the main: not with
// the edit, and not on the syncs after it (the sidecar backfill's sweep included).
import { test, expect } from '../fixtures.mjs';
import { cloudFile } from '../cloud-direct.mjs';
import { bunkoSeenAlive, cloudJson, connect, recordReaderWrites, volumeNamed } from './shared.mjs';

test.use({ needs: ['edit-layers-personal'] });

test('an edit goes up as <Vol>.edit.mokuro, never as the main sidecar', async ({ reader, rig }) => {
    test.setTimeout(240_000);
    const series = rig.library.series;
    const main = `${series}/Vol 1.mokuro`;
    const layer = `${series}/Vol 1.edit.mokuro`;
    const text = `共有の編集 ${Date.now().toString(36)}`;
    await connect(reader, rig);
    await bunkoSeenAlive(reader);
    const mainBefore = await cloudFile(rig, main);
    expect(mainBefore, main).toBeTruthy();

    const { id } = await reader.open((await volumeNamed(reader, series, 'Vol 1')).id);
    const writes = recordReaderWrites(reader.page, rig);
    expect(await reader.editFirstBlock(id, 1, text)).toBeTruthy();
    // The push is the layer sync's: a sync (or its own debounce) sends it.
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                return (await cloudJson(rig, layer))?.pages?.[0]?.blocks?.[0]?.lines ?? null;
            },
            { timeout: 90_000, intervals: [1000, 2000, 5000] },
        )
        .toEqual([text]);
    const pushed = await cloudJson(rig, layer);
    expect(pushed.edited).toMatchObject({ by: 'mokuro-reader' });
    expect(pushed).not.toHaveProperty('ocr_engine');
    expect((await reader.library()).volumes.find((v) => v.id === id).layers).toContain('edit');

    // Two more syncs: the backfill sweep finds an edited volume and still leaves the
    // main alone.
    for (let i = 0; i < 2; i++) expect(await reader.sync()).toEqual({ ok: true, errors: [] });
    await reader.idle();
    const wrote = await writes.names();
    expect(wrote).toContain('Vol 1.edit.mokuro');
    expect(wrote).not.toContain('Vol 1.mokuro');
    expect((await cloudFile(rig, main))?.sha256).toBe(mainBefore.sha256);
});
