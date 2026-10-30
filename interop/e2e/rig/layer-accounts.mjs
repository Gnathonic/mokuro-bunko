// S2 (B8.6, D27, plan §1 "Layer ids and who may write each"): edit layers across
// accounts, on a library read through Bunko (dav: its own disk; bridge: a cloud it
// alone talks to). Three accounts on one volume of a series the scenario seeds itself
// (so a rename and a delete touch nothing another spec reads):
// - alice (an editor) edits: her `<Vol>.edit.mokuro` is hers alone (bob reads none at
//   that path), and the main `<Vol>.mokuro` is the server's as it was;
// - alice publishes: `<Vol>.curated.mokuro` is in the volume's manifest for bob and
//   carol; bob selects it and his reader shows alice's text;
// - bob edits on top of curated: the edit forks into his own `edit`, curated stays;
// - carol (registered) edits her own, but her publish is refused (by her reader, and
//   by Bunko when she PUTs the file herself);
// - alice renames the volume: every account's overlay and the curated layer follow;
// - alice deletes the volume: every account's overlay goes with it.
// An overlay of an account that owns no cloud root lives under Bunko's
// `users/<u>/edits/`, never beside the archive in the library (nor in the cloud).
import { createHash, randomUUID } from 'node:crypto';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { basic, bunko, davPath } from './bunko.mjs';
import { cloudFile, cloudFiles } from './cloud-direct.mjs';

const PASSWORD = 'rig-layers-1';
const VOLUME = 'Vol 1';
const RENAMED = 'Vol 1 renamed';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** `bytes` (a .cbz) with its zip comment overwritten by `tag`: the same archive, other bytes. */
function retagZip(bytes, tag) {
    const out = Buffer.from(bytes);
    const eocd = out.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
    if (eocd < 0) throw new Error('not a zip archive');
    const n = out.readUInt16LE(eocd + 20);
    Buffer.from(tag.padEnd(n, ' ').slice(0, n), 'latin1').copy(out, eocd + 22);
    return out;
}

/** The first block's lines of a `.mokuro` document's first page. */
const firstLines = (doc) => doc?.pages?.[0]?.blocks?.[0]?.lines ?? null;

/**
 * Define S2's specs (serial, one fresh device each) in the calling spec file.
 * `inCloud`: Bunko keeps the library in the rig's cloud, so the cloud is checked too.
 */
export function editLayersAcrossAccounts(test, expect, { inCloud }) {
    test.describe.configure({ mode: 'serial' });
    test.use({ needs: ['edit-layers-personal', 'edit-layers-curated'] });

    /** Shared by the specs below, in order. */
    const s = { ready: false };

    const auth = (a) => basic(a.username, a.password);
    const file = (stem, id) => (id ? `${stem}.${id}.mokuro` : `${stem}.mokuro`);
    const dav = (stem, id) => davPath(s.series, file(stem, id));
    /** `<stem>[.<id>].mokuro` as `account` reads it: `{status, doc}`. */
    async function read(rig, account, stem, id) {
        const r = await bunko(rig).get(dav(stem, id), auth(account));
        return { status: r.status, doc: r.status === 200 ? JSON.parse(r.text) : null };
    }
    /** An account's overlay on Bunko's disk (`users/<u>/edits/<library rel>`). */
    const overlayOnDisk = (rig, account, stem) =>
        existsSync(path.join(rig.workdir, 'bunko', 'users', account.username, 'edits', s.series, file(stem, 'edit')));
    /** The library's own copy of a file: Bunko's disk, or the cloud it keeps the library in. */
    async function inLibrary(rig, name) {
        if (inCloud) return (await cloudFile(rig, `${s.series}/${name}`)) !== null;
        return existsSync(path.join(rig.workdir, 'bunko', 'library', s.series, name));
    }
    /** Every account's edit at `stem`: what each reads, in order alice, bob, carol. */
    const edits = (rig, stem) => Promise.all([s.alice, s.bob, s.carol].map((a) => read(rig, a, stem, 'edit')));

    async function connect(reader, rig, account) {
        const { sync } = await reader.connectWebDAV({
            url: rig.bunkoUrl,
            username: account.username,
            password: account.password,
        });
        expect(sync).toEqual({ ok: true, errors: [] });
    }

    /** Signed in as `account`, the scenario's volume downloaded and open: its id. */
    async function openVolume(reader, rig, account, title = VOLUME) {
        await connect(reader, rig, account);
        const { volumes } = await reader.library();
        const v = volumes.find((x) => x.series === s.series && x.title === title);
        expect(v, `${s.series}/${title} in ${account.username}'s library`).toBeTruthy();
        const { id } = await reader.open(v.id);
        expect(id).toBe(s.volumeId);
        return id;
    }

    /** Sync until the device holds (at least) the layers `ids` of the volume `id`. */
    async function layersOnDevice(reader, id, ids) {
        await expect
            .poll(
                async () => {
                    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                    await reader.idle();
                    return (await reader.library()).volumes.find((v) => v.id === id).layers;
                },
                { timeout: 60_000, intervals: [500, 1000, 2000] },
            )
            .toEqual(expect.arrayContaining(ids));
    }

    /** Sync until `account` reads `lines` in its own edit at `stem`. */
    async function pushedEdit(reader, rig, account, lines, stem = VOLUME) {
        await expect
            .poll(
                async () => {
                    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                    return firstLines((await read(rig, account, stem, 'edit')).doc);
                },
                { timeout: 60_000, intervals: [500, 1000, 2000] },
            )
            .toEqual(lines);
    }

    test.beforeAll(async ({ rig }) => {
        const server = bunko(rig);
        const tag = Date.now().toString(36);
        s.alice = await server.createUser(`s2-alice-${tag}`, PASSWORD, 'editor');
        s.bob = await server.createUser(`s2-bob-${tag}`, PASSWORD, 'uploader');
        s.carol = await server.createUser(`s2-carol-${tag}`, PASSWORD, 'registered');
        s.series = `S2 Layers ${tag}`;
        s.volumeId = randomUUID();
        s.text = { alice: `アリスの訂正 ${tag}`, bob: `ボブの訂正 ${tag}`, carol: `キャロルの訂正 ${tag}` };
        // The seeded volume's archive under other bytes, and an OCR sidecar of its own
        // (its pages name the archive's images), PUT by alice as any uploader would.
        const a = auth(s.alice);
        const source = await server.bytes(davPath(rig.library.series, 'Vol 1.cbz'), a);
        expect(source.status).toBe(200);
        const pages = JSON.parse((await server.get(davPath(rig.library.series, 'Vol 1.mokuro'), a)).text).pages;
        expect(await server.mkcol(davPath(s.series), a)).toBe(201);
        const archive = retagZip(source.bytes, `s2 ${tag}`);
        expect(await server.put(davPath(s.series, `${VOLUME}.cbz`), archive, a)).toBe(201);
        const doc = {
            version: '0.2.2',
            title: s.series,
            title_uuid: randomUUID(),
            volume: VOLUME,
            volume_uuid: s.volumeId,
            pages,
        };
        expect([201, 204]).toContain(await server.put(dav(VOLUME), JSON.stringify(doc), a));
        s.ready = true;
    });

    test("alice's edit is hers alone and leaves the main; her publish reaches bob and carol", async ({
        reader,
        rig,
    }) => {
        test.skip(!s.ready, 'needs the seeded volume');
        test.setTimeout(180_000);
        const server = bunko(rig);
        const id = await openVolume(reader, rig, s.alice);
        const main = await server.bytes(dav(VOLUME), auth(s.alice));
        expect(main.status).toBe(200);

        await reader.editFirstBlock(id, 1, s.text.alice);
        await pushedEdit(reader, rig, s.alice, [s.text.alice]);
        // bob and carol read no edit at that path: alice's is hers.
        const [, bobs, carols] = await edits(rig, VOLUME);
        expect(bobs.status, "bob's GET of alice's edit path").toBe(404);
        expect(carols.status, "carol's GET of alice's edit path").toBe(404);
        // The main is the server's as it was; the edit is an overlay, not a library file.
        expect(sha256((await server.bytes(dav(VOLUME), auth(s.alice))).bytes)).toBe(sha256(main.bytes));
        expect(overlayOnDisk(rig, s.alice, VOLUME), "alice's overlay under users/").toBe(true);
        expect(await inLibrary(rig, file(VOLUME, 'edit')), 'an edit file in the library').toBe(false);

        expect(await reader.publishCurated(id)).toMatchObject({ outcome: 'published' });
        for (const who of [s.bob, s.carol]) {
            const m = await server.manifest(s.series, VOLUME, auth(who));
            expect(m.status).toBe(200);
            const ids = m.body.layers.map((l) => l.id);
            expect(ids, `the manifest's layers for ${who.username}`).toContain('curated');
            expect(ids, `the manifest's layers for ${who.username}`).not.toContain('edit');
        }
        expect(firstLines((await read(rig, s.bob, VOLUME, 'curated')).doc)).toEqual([s.text.alice]);
        if (inCloud) {
            await expect.poll(() => inLibrary(rig, file(VOLUME, 'curated')), { timeout: 60_000 }).toBe(true);
        }
    });

    test("bob reads alice's curated text; his own edit forks and leaves curated alone", async ({ reader, rig }) => {
        test.skip(!s.ready, 'needs the seeded volume');
        test.setTimeout(180_000);
        const server = bunko(rig);
        const id = await openVolume(reader, rig, s.bob);
        // The layer sync pulls curated for the volume now on this device.
        await layersOnDevice(reader, id, ['curated']);
        await reader.selectLayer(id, 'curated');
        // The reader shows alice's text on the open page.
        await reader.page.waitForFunction(
            (t) => [...document.querySelectorAll('.textBox')].some((e) => e.textContent.includes(t)),
            s.text.alice,
            { timeout: 30_000 },
        );

        const curated = await server.bytes(dav(VOLUME, 'curated'), auth(s.bob));
        expect(curated.status).toBe(200);
        expect(await reader.editFirstBlock(id, 1, s.text.bob)).toEqual({ layerId: 'edit' });
        await pushedEdit(reader, rig, s.bob, [s.text.bob]);
        const [alices, , carols] = await edits(rig, VOLUME);
        expect(firstLines(alices.doc), "alice's edit after bob's").toEqual([s.text.alice]);
        expect(carols.status).toBe(404);
        expect(sha256((await server.bytes(dav(VOLUME, 'curated'), auth(s.bob))).bytes), 'curated after bob edits').toBe(
            sha256(curated.bytes),
        );
    });

    test("carol's own edit goes up; her publish is refused", async ({ reader, rig }) => {
        test.skip(!s.ready, 'needs the seeded volume');
        test.setTimeout(180_000);
        const server = bunko(rig);
        const id = await openVolume(reader, rig, s.carol);
        const curated = await server.bytes(dav(VOLUME, 'curated'), auth(s.carol));
        expect(curated.status).toBe(200);
        await reader.editFirstBlock(id, 1, s.text.carol);
        await pushedEdit(reader, rig, s.carol, [s.text.carol]);

        expect(await reader.publishCurated(id)).toMatchObject({ outcome: 'refused' });
        // The server refuses it too: carol's own PUT of the curated file, a valid layer.
        const body = JSON.stringify({ ...JSON.parse(curated.bytes.toString('utf8')), pages: [] });
        expect(await server.put(dav(VOLUME, 'curated'), body, auth(s.carol))).toBe(403);
        expect(sha256((await server.bytes(dav(VOLUME, 'curated'), auth(s.bob))).bytes), 'curated after carol').toBe(
            sha256(curated.bytes),
        );
        const [alices, bobs] = await edits(rig, VOLUME);
        expect(firstLines(alices.doc)).toEqual([s.text.alice]);
        expect(firstLines(bobs.doc)).toEqual([s.text.bob]);
    });

    test("alice's rename takes every overlay and curated along; her delete takes the overlays", async ({
        reader,
        rig,
    }) => {
        test.skip(!s.ready, 'needs the seeded volume');
        test.setTimeout(240_000);
        const server = bunko(rig);
        const id = await openVolume(reader, rig, s.alice);
        // The rename moves the layer files this device's layer rows vouch for (a file it
        // never pulled is not known to be the volume's): wait, as the layer picker shows
        // them, until the layer sync after the download has pulled both.
        await layersOnDevice(reader, id, ['curated', 'edit']);
        expect(await reader.renameVolume(id, RENAMED)).toEqual({ title: RENAMED });

        // Each account reads its own edit under the new name, and nothing under the old.
        await expect
            .poll(async () => (await edits(rig, RENAMED)).map((r) => firstLines(r.doc)), { timeout: 60_000 })
            .toEqual([[s.text.alice], [s.text.bob], [s.text.carol]]);
        expect(
            (await edits(rig, VOLUME)).map((r) => r.status),
            'edits under the old name',
        ).toEqual([404, 404, 404]);
        for (const who of [s.alice, s.bob, s.carol]) {
            expect(overlayOnDisk(rig, who, RENAMED), `${who.username}'s overlay, renamed`).toBe(true);
            expect(overlayOnDisk(rig, who, VOLUME), `${who.username}'s overlay, old name`).toBe(false);
        }
        const cur = await read(rig, s.carol, RENAMED, 'curated');
        expect({ status: cur.status, lines: firstLines(cur.doc) }).toEqual({ status: 200, lines: [s.text.alice] });
        expect((await read(rig, s.carol, VOLUME, 'curated')).status).toBe(404);
        const m = await server.manifest(s.series, RENAMED, auth(s.bob));
        expect(m.status).toBe(200);
        expect(m.body.layers.map((l) => l.id)).toEqual(['curated']);
        if (inCloud) {
            await expect
                .poll(
                    async () =>
                        (await cloudFiles(rig))
                            .filter((f) => f.path.startsWith(`${s.series}/`))
                            .map((f) => f.path.slice(s.series.length + 1))
                            .filter((n) => n.endsWith('.mokuro'))
                            .sort(),
                    { timeout: 60_000 },
                )
                .toEqual([file(RENAMED, 'curated'), file(RENAMED)]);
        }

        expect(await reader.deleteVolume(id)).toEqual({ cloud: 'deleted' });
        await expect
            .poll(async () => (await edits(rig, RENAMED)).map((r) => r.status), { timeout: 60_000 })
            .toEqual([404, 404, 404]);
        for (const who of [s.alice, s.bob, s.carol]) {
            expect(overlayOnDisk(rig, who, RENAMED), `${who.username}'s overlay after the delete`).toBe(false);
        }
        expect((await read(rig, s.bob, RENAMED, 'curated')).status).toBe(404);
        expect((await server.manifest(s.series, RENAMED, auth(s.bob))).status).toBe(404);
        if (inCloud) {
            await expect
                .poll(async () => (await cloudFiles(rig)).filter((f) => f.path.startsWith(`${s.series}/${RENAMED}.`)), {
                    timeout: 60_000,
                })
                .toEqual([]);
        }
    });
}
