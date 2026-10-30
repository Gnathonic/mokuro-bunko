// S4 and S6 (B8.6): a reader renames volumes Bunko OCRs, on a library Bunko keeps on
// its own disk (dav: the reader signed in to Bunko) or in a cloud it shares with the
// reader (shared: the reader on the cloud itself, Bunko holding its lease). Each
// scenario imports and backs up series of its own (`RIG_CONTEXT.uploads`), so its
// renames move nothing another spec reads. The rig's processor-fake OCRs them: it
// stamps the library's engine (`ocr_engine.id`, this build's revision) and gives every
// page one text block; Bunko seals what it installs.
//
// - S4 (renameKeepsProvenance): two volumes OCRed, the second then edited; the reader
//   renames both volumes, then their series. The regenerated mains carry the block
//   they were installed with (seal included), the edited volume's `edit` layer keeps
//   its `edited` marker, and over two Bunko walks after Bunko saw the renames nothing
//   is queued for either volume (the processor held all along, so a job would stay).
// - S6 (renameDuringOcr): a volume's job claimed and held (an image-only volume's
//   first OCR; a volume the reader has OCR of, its sidecar deleted so Bunko OCRs it
//   again); the reader renames the volume; the hold released. No `<stem>_<n>.mokuro`
//   (nor `<stem>.<layer>_<n>.mokuro`) ever exists, the sidecar sits under the new name,
//   and the queue settles.
//
// Bunko's walks are counted by `/queue/api/status`'s admin-only `walks` (a testing
// build's): "two passes after X" is a condition, never a timer.
import { existsSync, readdirSync, readFileSync, rmSync, watch, writeFileSync } from 'node:fs';
import path from 'node:path';
import { basic, bunko, davPath } from './bunko.mjs';
import { LIBRARY_FOLDER, cloudFiles } from './cloud-direct.mjs';
import { bunkoSeenAlive, cloudJson, connect as connectCloud, tabAwayAndBack } from './shared/shared.mjs';

const PASSWORD = 'rig-rename-1';
/** A numbered sidecar (`Vol 1_1.mokuro`, `Vol 1.paddle-manga_2.mokuro.gz`). */
const NUMBERED = /_\d+\.mokuro(\.gz)?$/;

/**
 * The processor-fake hold (RIG_FAKEPROC_HOLD): a volume it starts while the file is
 * missing waits until it exists. `restore()` leaves it as the spec found it, so a spec
 * after this one sees the suite's hold as its own setup left it.
 */
export function processorHold(rig) {
    const file = rig.fakeprocHold;
    if (!file) throw new Error('RIG_FAKEPROC_HOLD is not set: this suite runs no processor-fake');
    const found = existsSync(file);
    return {
        hold: () => rmSync(file, { force: true }),
        release: () => writeFileSync(file, ''),
        restore: () => (found ? writeFileSync(file, '') : rmSync(file, { force: true })),
    };
}

/** Bunko's OCR queue as its admin sees it (`/queue/api/status`). */
export async function queueStatus(rig) {
    const admin = rig.account('admin');
    const r = await bunko(rig).request('GET', '/queue/api/status', { auth: basic(admin.username, admin.password) });
    if (!r.ok) throw new Error(`the queue status: ${r.status} ${await r.text()}`);
    return r.json();
}

/** The volumes of `series` (every series: null) the queue runs, waits on and has failed. */
export function jobsOf(status, series = null) {
    const machines = status.machines ?? [];
    const of = (list) => list.filter((j) => series === null || j.series === series).map((j) => j.volume);
    return {
        running: of(machines.flatMap((m) => m.jobs ?? [])),
        pending: of([...(status.pending ?? []), ...machines.flatMap((m) => m.next ?? [])]),
        failed: of(status.failed ?? []),
    };
}

/**
 * Resolves once Bunko has finished `n` library walks that STARTED after this call (one
 * in flight when it is called is not counted).
 */
export async function walksAfter(expect, rig, n) {
    const start = (await queueStatus(rig)).walks;
    if (typeof start !== 'number') {
        throw new Error('/queue/api/status names no walks: the rig needs a `--features testing` server');
    }
    await expect
        .poll(async () => (await queueStatus(rig)).walks, { timeout: 60_000, intervals: [250, 500] })
        .toBeGreaterThanOrEqual(start + n + 1);
}

/** Every file below `dir` on disk, relative to it. */
function filesBelow(dir, rel = '') {
    if (!existsSync(path.join(dir, rel))) return [];
    const out = [];
    for (const e of readdirSync(path.join(dir, rel), { withFileTypes: true })) {
        const r = rel ? `${rel}/${e.name}` : e.name;
        if (e.isDirectory()) out.push(...filesBelow(dir, r));
        else out.push(r);
    }
    return out;
}

/**
 * Where the library is, as a spec reads it behind the reader's back: Bunko's own disk
 * (`inCloud: false`), or the cloud it keeps the library in.
 */
export function librarySide(rig, inCloud) {
    const disk = path.join(rig.workdir, 'bunko', 'library');
    // rclone serves a directory of this machine: the library is on a disk there too.
    const rcloneDisk = path.join(rig.workdir, 'rclone-root', LIBRARY_FOLDER);
    const onDisk = !inCloud ? disk : rig.provider === 'rclone' ? rcloneDisk : null;
    // Bunko sweeps a numbered sidecar it can prove is its own off the disk: every name
    // the disk had while the spec ran is watched for, not just read at the end.
    // cloudsim keeps every node, trashed ones included, so it needs no watch.
    const seen = new Set();
    let watcher = null;
    return {
        /** From now on, every name the library's disk has is remembered (`everNames`). */
        watch() {
            if (!onDisk || watcher) return;
            watcher = watch(onDisk, { recursive: true }, (_event, name) => {
                if (name) seen.add(String(name).split('/').pop());
            });
        },
        unwatch() {
            watcher?.close();
            watcher = null;
        },
        /** Every file of the library now, relative to its folder. */
        async files() {
            if (!inCloud) return filesBelow(disk);
            return (await cloudFiles(rig)).map((f) => f.path);
        },
        /**
         * The name of every file the library has held: on a disk (Bunko's, rclone's),
         * what is there now and what it had while watched; in cloudsim, every node it
         * ever kept, the trashed ones too.
         */
        async everNames() {
            if (onDisk) return [...new Set([...filesBelow(onDisk).map((r) => r.split('/').pop()), ...seen])];
            const rows = await rig.sim(
                'GET',
                `tree?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&include_trashed=1`,
            );
            return rows.filter((r) => !r.folder).map((r) => r.name);
        },
        /** A JSON file of the library, parsed, or null. */
        async json(rel) {
            if (inCloud) return cloudJson(rig, rel);
            const file = path.join(disk, rel);
            return existsSync(file) ? JSON.parse(readFileSync(file, 'utf8')) : null;
        },
    };
}

/** The reader signed in, as each suite's reader is; the Bunko account a spec reads as. */
async function signIn(reader, rig, inCloud, expect) {
    const admin = rig.account('admin');
    if (inCloud) {
        await connectCloud(reader, rig);
        // Bunko's marker fresh and seen moving: the reader treats the library as Bunko's
        // (series.json and the queue are Bunko's) and watches its queue file.
        await bunkoSeenAlive(reader);
        return admin;
    }
    // An editor: it uploads, renames and edits.
    const account = await bunko(rig).createUser(`rename-${Date.now().toString(36)}`, PASSWORD, 'editor');
    const { sync } = await reader.connectWebDAV({ url: rig.bunkoUrl, username: account.username, password: PASSWORD });
    expect(sync).toEqual({ ok: true, errors: [] });
    return account;
}

/** Import `<series>/<title>.cbz` (a rig upload) and back it up: the volume's id. */
async function uploadVolume(reader, rig, expect, series, title) {
    const rel = `${series}/${title}`;
    const { id } = await reader.importVolume(rig.upload(`${rel}.cbz`));
    expect(await reader.backupVolume(id)).toEqual({ cloudPath: `${rel}.cbz` });
    return id;
}

/** Until the reader has each of `ids`' OCR (Bunko's, pulled once its job is done). */
async function ocrOnDevice(reader, expect, ids) {
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                const { volumes } = await reader.library();
                return ids.filter((id) => !volumes.find((v) => v.id === id)?.hasOcr);
            },
            { timeout: 240_000, intervals: [1000, 2000, 5000] },
        )
        .toEqual([]);
}

/** Until Bunko lists `names` in `series` (its own index of the library: what it walks). */
async function bunkoLists(rig, expect, series, names) {
    const admin = rig.account('admin');
    const auth = basic(admin.username, admin.password);
    await expect
        .poll(
            async () => {
                const { status, hrefs } = await bunko(rig).propfind(`${davPath(series)}/`, auth);
                if (status !== 207) return `PROPFIND ${status}`;
                const listed = new Set(hrefs.map((h) => h.split('/').pop()));
                return names.filter((n) => !listed.has(n));
            },
            { timeout: 180_000, intervals: [500, 1000, 2000] },
        )
        .toEqual([]);
}

/** S4: rename keeps provenance (and an edited volume its `edited` marker). */
export function renameKeepsProvenance(test, expect, { inCloud }) {
    test.use({ needs: ['provenance'] });

    test('renamed volumes and series keep their provenance, and Bunko queues nothing for them', async ({
        reader,
        rig,
    }) => {
        test.setTimeout(600_000);
        const series = 'S4 Provenance';
        const renamedSeries = 'S4 Provenance renamed';
        const titles = ['Vol 1', 'Vol 2'];
        const renamed = (t) => `${t} renamed`;
        const library = librarySide(rig, inCloud);
        const hold = processorHold(rig);
        try {
            hold.release();
            const account = await signIn(reader, rig, inCloud, expect);
            const auth = basic(account.username, account.password);

            // Both volumes up, image only; Bunko OCRs them and the reader pulls the OCR.
            const ids = [];
            for (const t of titles) ids.push(await uploadVolume(reader, rig, expect, series, t));
            await reader.showSeries(series);
            await ocrOnDevice(reader, expect, ids);
            const blocks = {};
            for (const t of titles) {
                const main = await library.json(`${series}/${t}.mokuro`);
                expect(main?.ocr_engine?.seal, `${t}: Bunko sealed what it installed`).toMatch(/^v1:[0-9a-f]{64}$/);
                expect(main).not.toHaveProperty('edited');
                blocks[t] = main.ocr_engine;
            }

            // Vol 2 edited: its corrections go up as the account's `edit` layer, marked.
            const text = `S4の訂正 ${Date.now().toString(36)}`;
            await reader.editFirstBlock(ids[1], 1, text);
            const editOf = async (s, t) => {
                if (inCloud) return library.json(`${s}/${t}.edit.mokuro`);
                const r = await bunko(rig).get(davPath(s, `${t}.edit.mokuro`), auth);
                return r.status === 200 ? JSON.parse(r.text) : null;
            };
            await expect
                .poll(
                    async () => {
                        expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                        return (await editOf(series, 'Vol 2'))?.pages?.[0]?.blocks?.[0]?.lines ?? null;
                    },
                    { timeout: 60_000, intervals: [500, 1000, 2000] },
                )
                .toEqual([text]);
            const marker = (await editOf(series, 'Vol 2')).edited;
            expect(marker, 'the edit layer says whose corrections it holds').toBeTruthy();

            // From here the processor holds whatever it is given: a job Bunko queued for
            // either volume would still be in the queue when it is read below.
            hold.hold();
            await expect
                .poll(async () => jobsOf(await queueStatus(rig)), { timeout: 60_000 })
                .toEqual({ running: [], pending: [], failed: [] });

            for (const [i, t] of titles.entries()) {
                expect(await reader.renameVolume(ids[i], renamed(t))).toEqual({ title: renamed(t) });
            }
            const result = await reader.renameSeries(series, renamedSeries);
            expect(result).toMatchObject({ finalTitle: renamedSeries, failures: [] });

            const want = titles.flatMap((t) => [`${renamed(t)}.cbz`, `${renamed(t)}.mokuro`]);
            await bunkoLists(rig, expect, renamedSeries, want);
            const files = await library.files();
            for (const n of want) expect(files, n).toContain(`${renamedSeries}/${n}`);
            // The old folder empties: the volumes moved, and its series.json (Bunko's
            // compile of a factless series) is retired once no archive is left.
            await expect
                .poll(async () => (await library.files()).filter((f) => f.startsWith(`${series}/`)), {
                    timeout: 120_000,
                    intervals: [1000, 2000],
                })
                .toEqual([]);

            // Two walks after Bunko saw the renames: nothing queued for them.
            await walksAfter(expect, rig, 2);
            const status = await queueStatus(rig);
            for (const s of [series, renamedSeries]) {
                expect(jobsOf(status, s), `the queue on ${s}`).toEqual({ running: [], pending: [], failed: [] });
            }

            // Each main is the file it was, under the new names: the same block, seal and all.
            for (const t of titles) {
                const main = await library.json(`${renamedSeries}/${renamed(t)}.mokuro`);
                expect(main).toMatchObject({ title: renamedSeries, volume: renamed(t) });
                expect(main.ocr_engine, `${renamed(t)} keeps its provenance`).toEqual(blocks[t]);
                expect(main).not.toHaveProperty('edited');
            }
            // The edited volume's corrections moved with it, marked as they were.
            const edit = await editOf(renamedSeries, renamed('Vol 2'));
            expect(edit?.pages?.[0]?.blocks?.[0]?.lines).toEqual([text]);
            expect(edit.edited, 'the edit layer keeps its edited marker').toEqual(marker);
            expect(await editOf(series, 'Vol 2')).toBeNull();
        } finally {
            library.unwatch();
            hold.restore();
        }
    });
}

/** S6: a rename while the volume's OCR runs leaves no numbered sidecar. */
export function renameDuringOcr(test, expect, { inCloud }) {
    const series = 'S6 Rename';
    const settled = { running: [], pending: [], failed: [] };

    /** Whatever the suite left queued done, so a job held after this is the spec's own. */
    async function drained(rig, hold) {
        hold.release();
        await expect
            .poll(async () => jobsOf(await queueStatus(rig)), { timeout: 180_000, intervals: [500, 1000] })
            .toEqual(settled);
    }

    /** Until Bunko's processor holds `title`'s job (claimed, started, waiting). */
    async function heldJob(rig, title) {
        await expect
            .poll(async () => jobsOf(await queueStatus(rig), series).running, {
                timeout: 180_000,
                intervals: [500, 1000, 2000],
            })
            .toEqual([title]);
    }

    /**
     * Released, the queue settles and stays settled two walks on; `title`'s only sidecar
     * is `<title>.mokuro`, and no numbered sidecar was ever made: the main for `title`.
     */
    async function settledUnder(rig, library, title) {
        await expect
            .poll(async () => (await library.files()).filter((f) => f.startsWith(`${series}/`)), {
                timeout: 180_000,
                intervals: [500, 1000, 2000],
            })
            .toEqual(expect.arrayContaining([`${series}/${title}.cbz`, `${series}/${title}.mokuro`]));
        await expect
            .poll(async () => jobsOf(await queueStatus(rig), series), { timeout: 180_000, intervals: [500, 1000] })
            .toEqual(settled);
        await walksAfter(expect, rig, 2);
        expect(jobsOf(await queueStatus(rig), series)).toEqual(settled);
        const stem = title.replace(/ renamed$/, '');
        const sidecars = (await library.files()).filter(
            (f) => f.startsWith(`${series}/${stem}`) && /\.mokuro(\.gz)?$/.test(f),
        );
        expect(sidecars).toEqual([`${series}/${title}.mokuro`]);
        expect((await library.everNames()).filter((n) => NUMBERED.test(n))).toEqual([]);
        const main = await library.json(`${series}/${title}.mokuro`);
        expect(main).toMatchObject({ title: series, volume: title });
        expect(main.ocr_engine?.seal, 'a sealed main').toMatch(/^v1:[0-9a-f]{64}$/);
        return main;
    }

    test('an image-only volume renamed while Bunko OCRs it gets its sidecar under the new name', async ({
        reader,
        rig,
    }) => {
        test.setTimeout(600_000);
        const library = librarySide(rig, inCloud);
        const hold = processorHold(rig);
        library.watch();
        try {
            await signIn(reader, rig, inCloud, expect);
            await drained(rig, hold);
            hold.hold();
            const id = await uploadVolume(reader, rig, expect, series, 'Vol 1');
            await heldJob(rig, 'Vol 1');

            // The reader has no OCR of it: the rename moves the archive (and cover) alone.
            expect(await reader.renameVolume(id, 'Vol 1 renamed')).toEqual({ title: 'Vol 1 renamed' });
            await bunkoLists(rig, expect, series, ['Vol 1 renamed.cbz']);
            hold.release();
            await settledUnder(rig, library, 'Vol 1 renamed');
        } finally {
            library.unwatch();
            hold.restore();
        }
    });

    test("a volume renamed while Bunko re-OCRs it keeps the reader's main under the new name", async ({
        reader,
        rig,
    }) => {
        test.setTimeout(600_000);
        const library = librarySide(rig, inCloud);
        const hold = processorHold(rig);
        const admin = rig.account('admin');
        library.watch();
        try {
            await signIn(reader, rig, inCloud, expect);
            await drained(rig, hold);
            const id = await uploadVolume(reader, rig, expect, series, 'Vol 2');
            await reader.showSeries(series);
            await ocrOnDevice(reader, expect, [id]);
            const block = (await library.json(`${series}/Vol 2.mokuro`)).ocr_engine;

            // Bunko OCRs it again (its sidecar gone), held; the reader still has the OCR,
            // so its rename writes the main at the new name while that job runs.
            hold.hold();
            const gone = await bunko(rig).request('DELETE', davPath(series, 'Vol 2.mokuro'), {
                auth: basic(admin.username, admin.password),
            });
            expect([200, 204]).toContain(gone.status);
            await heldJob(rig, 'Vol 2');
            expect(await reader.renameVolume(id, 'Vol 2 renamed')).toEqual({ title: 'Vol 2 renamed' });
            await bunkoLists(rig, expect, series, ['Vol 2 renamed.cbz', 'Vol 2 renamed.mokuro']);
            hold.release();
            const main = await settledUnder(rig, library, 'Vol 2 renamed');
            // The reader's main: the same OCR, its block carried, seal and all.
            expect(main.ocr_engine).toEqual(block);
        } finally {
            library.unwatch();
            hold.restore();
        }
    });
}
