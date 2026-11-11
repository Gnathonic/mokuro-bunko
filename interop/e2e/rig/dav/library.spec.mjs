// dav (§7.1): the library through Bunko's WebDAV. A volume opened is downloaded and
// read; its progress goes up to the account's own volume-data.json and comes back on
// another device; an edit goes up as the account's own `<Vol>.edit.mokuro` sidecar,
// never into the shared main sidecar. One account for the file (serial: the second
// device reads what the first wrote).
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, davPath } from '../bunko.mjs';

test.use({ preflightsReachServers: true });
test.describe.configure({ mode: 'serial' });

const PASSWORD = 'rig-library-1';
const EDITED = 'リグの編集です';
/** Shared by the specs below, in order. */
const state = { account: null, volumeId: null };

async function connect(reader, rig, account) {
    const { sync } = await reader.connectWebDAV({
        url: rig.bunkoUrl,
        username: account.username,
        password: account.password,
    });
    expect(sync).toEqual({ ok: true, errors: [] });
}

async function volumeNamed(reader, rig, title) {
    const { volumes } = await reader.library();
    const v = volumes.find((x) => x.series === rig.library.series && x.title === title);
    expect(v, `${rig.library.series}/${title} in the library`).toBeTruthy();
    return v;
}

test('a volume opened is downloaded and its pages shown', async ({ reader, rig }) => {
    state.account = await bunko(rig).createUser(`dav-lib-${Date.now().toString(36)}`, PASSWORD);
    await connect(reader, rig, state.account);
    const listed = await volumeNamed(reader, rig, 'Vol 1');
    expect(listed.hasOcr).toBe(false);

    const { id } = await reader.open(listed.id);
    state.volumeId = id;
    const installed = await volumeNamed(reader, rig, 'Vol 1');
    expect(installed).toMatchObject({ id, hasOcr: true, cloudPath: `${rig.library.series}/Vol 1.cbz` });
    // The reader shows a page of it: the page's image (MangaPage's background), out of
    // the downloaded archive, and it decodes.
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

test("progress goes up to the account's own volume-data.json", async ({ reader, rig }) => {
    test.skip(!state.volumeId, 'needs the volume the first spec opened');
    const server = bunko(rig);
    // This device: download it again, read to page 3, sync.
    await connect(reader, rig, state.account);
    const { id } = await reader.open((await volumeNamed(reader, rig, 'Vol 1')).id);
    expect(id).toBe(state.volumeId);
    expect(await reader.setPage(id, 3)).toMatchObject({ page: 3 });
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });

    // The account's own file says so; another account's does not.
    const auth = basic(state.account.username, PASSWORD);
    const mine = await server.get(davPath('volume-data.json'), auth);
    expect(mine.status).toBe(200);
    expect(JSON.parse(mine.text)[id]).toMatchObject({ progress: 3 });
    const bob = rig.account('bob');
    const theirs = await server.get(davPath('volume-data.json'), basic(bob.username, bob.password));
    expect(theirs.status === 200 ? JSON.parse(theirs.text)[id] : undefined).toBeUndefined();
});

test('another device reads the progress back', async ({ reader, rig }) => {
    test.skip(!state.volumeId, 'needs the progress the spec before wrote');
    // A fresh browser context: nothing of the first device's is here.
    expect(await reader.library()).toEqual({ volumes: [] });
    await connect(reader, rig, state.account);
    expect(await reader.progress(state.volumeId)).toMatchObject({ page: 3 });
});

test("an edit goes up as the account's own edit sidecar, never into the main", async ({ reader, rig }) => {
    test.skip(!state.volumeId, 'needs the volume the first spec opened');
    const server = bunko(rig);
    const auth = basic(state.account.username, PASSWORD);
    const main = davPath(rig.library.series, 'Vol 1.mokuro');
    const edit = davPath(rig.library.series, 'Vol 1.edit.mokuro');
    const mainBefore = await server.get(main, auth);
    expect(mainBefore.status).toBe(200);

    await connect(reader, rig, state.account);
    const { id } = await reader.open((await volumeNamed(reader, rig, 'Vol 1')).id);
    await reader.editFirstBlock(id, 1, EDITED);
    // The push is the layer sync's: a sync (or its own debounce) sends it.
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                return (await server.get(edit, auth)).status;
            },
            { timeout: 60_000, intervals: [500, 1000, 2000] },
        )
        .toBe(200);
    const pushed = JSON.parse((await server.get(edit, auth)).text);
    expect(pushed.pages[0].blocks[0].lines).toEqual([EDITED]);
    expect((await reader.library()).volumes.find((v) => v.id === id).layers).toContain('edit');

    // The main is the server's as it was, and the edit is this account's alone.
    expect((await server.get(main, auth)).text).toBe(mainBefore.text);
    const bob = rig.account('bob');
    expect((await server.get(edit, basic(bob.username, bob.password))).status).toBe(404);
});
