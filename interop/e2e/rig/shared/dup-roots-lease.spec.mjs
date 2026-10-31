// S1 and the end of S9 (B8.6, plan X5, C2-R5) on a cloud a Bunko holds: duplicate
// `mokuro-reader` roots (dup-roots.mjs). Bunko's lease marker says it merges
// duplicates (`dedup`), so the reader steps aside and Bunko alone merges them.
//
// - S1: the X5 seed beside the library's root A. B, the older top-level root, is the
//   canonical one: the whole library moves into it, Bunko's lease following (its marker
//   is written into the canonical root), with no Suspect and no root-change hold. Then,
//   three times, a newer duplicate root appears while the reader syncs, and the two
//   converge the same way.
// - S9's end: Bunko stopped, an older top-level root appears, and once the reader finds
//   the marker stale it merges the library into that root itself (#308). Bunko started
//   again finds its root in the trash and its files moved: no D19 hold, no Suspect, and
//   its marker is written into the new canonical root.
//
// The merges move the suite's whole library, so every spec after this one reads it
// under another root folder of the same name.
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, davPath } from '../bunko.mjs';
import { processorHold } from '../rename-ocr.mjs';
import {
    CONTROL_FILES,
    asSeeded,
    filesBelow,
    mergeProblems,
    mokuroDoc,
    nestedChanges,
    nestedState,
    nodeBytes,
    retag,
    seedFiles,
    seedNodes,
    seedMaterial,
    seedX5,
    skipWithoutDuplicates,
    topFolder,
    topRoots,
    tree,
    treeEver,
    under,
    volumeData,
    volumeId,
    writesOutside,
} from '../dup-roots.mjs';
import { LIBRARY_FOLDER } from '../cloud-direct.mjs';
import { bunkoSeenAlive, connect, tabAwayAndBack } from './shared.mjs';

test.use({ needs: ['dedup'] });
test.describe.configure({ mode: 'serial' });

const OK = { ok: true, errors: [] };
const RACES = 3;

/**
 * Bunko's Storage panel, sampled every 500 ms until `stop()`: each distinct
 * `{engine, health, lease}` it showed, in order. A sample the server could not answer
 * (it is down) is skipped.
 */
function sampleStorage(rig) {
    const seen = [];
    let running = true;
    const loop = (async () => {
        while (running) {
            try {
                const s = await bunko(rig).storage();
                const state = { engine: s.engine, health: s.health?.state, lease: s.lease?.state };
                if (s.engine === 'suspect') state.suspect = s.sync;
                const last = seen.at(-1);
                if (!last || JSON.stringify(last) !== JSON.stringify(state)) seen.push(state);
            } catch {
                // Down (a restart): nothing to sample.
            }
            await new Promise((r) => setTimeout(r, 500));
        }
    })();
    return {
        seen,
        async stop() {
            running = false;
            await loop;
            return seen;
        },
    };
}

/** No sample was Suspect (D19's hold, or a changed root's); the samples annotate the spec. */
function expectNeverSuspect(samples) {
    test.info().annotations.push({ type: 'storage-states', description: JSON.stringify(samples) });
    expect(
        samples.filter((s) => s.engine === 'suspect' || s.health === 'suspect'),
        JSON.stringify(samples),
    ).toEqual([]);
}

/** Every 503 the page got, from anyone. */
function record503s(page) {
    const seen = [];
    page.on('response', (r) => {
        if (r.status() === 503) seen.push(`${r.request().method()} ${r.url()}`);
    });
    return seen;
}

/** The library root as the suite left it: the one live top-level `mokuro-reader`. */
async function libraryRoot(rig) {
    const tops = topRoots(await tree(rig));
    expect(
        tops.map((r) => r.name),
        'one library root before the seed',
    ).toHaveLength(1);
    return tops[0].id;
}

/** The live Bunko markers below `root` whose heartbeat is at or after `since` (ms). */
async function markersBeating(rig, root, since) {
    const rows = await tree(rig);
    const out = [];
    for (const [p, r] of filesBelow(rows, root)) {
        if (p !== CONTROL_FILES[0]) continue;
        const m = JSON.parse((await nodeBytes(rig, r.id)).toString('utf8'));
        if (Date.parse(m.heartbeat_at) >= since) out.push(m);
    }
    return out;
}

/** The volume `series`/`title` as the reader lists it, or null. */
async function listed(reader, series, title) {
    return (await reader.library()).volumes.find((v) => v.series === series && v.title === title) ?? null;
}

/** What Bunko serves of `series` to its admin: the names in the folder. */
async function bunkoLists(rig, series) {
    const admin = rig.account('admin');
    const { status, hrefs } = await bunko(rig).propfind(`${davPath(series)}/`, basic(admin.username, admin.password));
    return status === 207 ? hrefs.map((h) => h.split('/').filter(Boolean).pop()) : [];
}

test('S1: Bunko merges duplicate roots under its lease into the older top-level root', async ({ reader, rig }) => {
    skipWithoutDuplicates(test, rig);
    test.setTimeout(900_000);
    const storage = sampleStorage(rig);
    const got503 = record503s(reader.page);
    // A job the processor claims and holds stops a merge that moves its folder, and
    // this merge moves the whole library (the library's own image-only volume).
    const hold = processorHold(rig);
    hold.release();
    try {
        await connect(reader, rig);
        await bunkoSeenAlive(reader);
        expect((await reader.controlState()).held, 'the reader steps aside for a Bunko that merges').toBe(true);

        const rootA = await libraryRoot(rig);
        const before = await tree(rig);
        const aVolumeData = [...filesBelow(before, rootA)].filter(([p]) => p === 'volume-data.json');
        const seed = await seedX5(rig, rootA);
        const nestedBefore = nestedState(await tree(rig), seed);
        const merged = [rootA, seed.a.get('S1 Dup')];

        // Bunko merges A into B: wait for the merged state, the reader not syncing.
        await expect
            .poll(async () => mergeProblems(await tree(rig), seed.rootB, merged), {
                timeout: 300_000,
                intervals: [1000, 2000],
            })
            .toEqual([]);
        const mergedAt = Date.now();
        let rows = await tree(rig);
        const inB = filesBelow(rows, seed.rootB);
        // The newer sidecar is live in B, the older in the trash.
        expect(inB.get('S1 Dup/Vol 1.mokuro')?.id).toBe(seed.a.get('S1 Dup/Vol 1.mokuro'));
        expect(rows.find((r) => r.id === seed.b.get('S1 Dup/Vol 1.mokuro'))?.trashed).toBe(true);
        for (const p of [
            'S1 Dup/Vol 1.cbz',
            'S1 Dup/Vol 2.cbz',
            'S1 Dup/Vol 2.mokuro',
            `${rig.library.series}/Vol 1.cbz`,
        ]) {
            expect(inB.has(p), `${p} in B`).toBe(true);
        }
        // Both volume-data.json copies kept: B's own and A's, moved in beside it.
        const copies = rows.filter((r) => r.name === 'volume-data.json' && !r.trashed && r.parent_id === seed.rootB);
        expect(copies.map((r) => r.id).sort()).toEqual(
            [seed.b.get('volume-data.json'), ...aVolumeData.map(([, r]) => r.id)].sort(),
        );
        // The nested root is left as it was: Bunko only compiles its series' index
        // and covers beside it, as for any series.
        expect(rows.find((r) => r.id === seed.nested)?.trashed).toBe(false);
        expect(nestedChanges(rows, seed, nestedBefore)).toEqual([]);
        // Bunko's lease followed the canonical root: its marker beats in B.
        await expect
            .poll(async () => (await markersBeating(rig, seed.rootB, mergedAt)).length, { timeout: 120_000 })
            .toBeGreaterThan(0);
        const merges = (await bunko(rig).storage()).recent_merges ?? [];
        expect(merges.filter((m) => m.origin === 'dedup').length).toBeGreaterThan(0);

        // The reader reads every root: B's volume and its progress, and the nested one.
        await expect
            .poll(
                async () => {
                    expect(await reader.sync()).toEqual(OK);
                    await reader.idle();
                    return {
                        dup2: (await listed(reader, 'S1 Dup', 'Vol 2'))?.cloudPath ?? null,
                        nested: (await listed(reader, 'S1 Nested', 'Vol 1'))?.cloudPath ?? null,
                        progress: (await reader.progress(volumeId(62))).page,
                    };
                },
                { timeout: 120_000, intervals: [2000] },
            )
            .toEqual({ dup2: 'S1 Dup/Vol 2.cbz', nested: 'S1 Nested/Vol 1.cbz', progress: 3 });
        // Bunko reads the nested root too.
        expect(await bunkoLists(rig, 'S1 Nested')).toContain('Vol 1.cbz');
        await reader.settleSeriesWrites();
        rows = await tree(rig);
        // After the merge every write (the reader's and Bunko's) went into B, but for
        // what Bunko compiles for the nested root's own series.
        const ever = await treeEver(rig);
        const inNested = (r) => under(ever, r.id, seed.nested);
        expect(writesOutside(ever, seed.rootB, mergedAt, inNested)).toEqual([]);
        // ...and nothing went back into what the merge trashed.
        expect(mergeProblems(rows, seed.rootB, merged)).toEqual([]);
        expect(nestedChanges(rows, seed, nestedBefore)).toEqual([]);

        // Three times: a newer duplicate root appears while the reader syncs.
        const m = await seedMaterial(rig);
        for (let i = 1; i <= RACES; i++) {
            const series = `S1 Race ${i}`;
            const n = 64 + i;
            const rootD = await topFolder(rig, LIBRARY_FOLDER, null);
            const made = await seedNodes(rig, rootD, {
                [`${series}/Vol 1.cbz`]: retag(m.cbz, `${series}/Vol 1`),
                [`${series}/Vol 1.mokuro`]: mokuroDoc(m, series, 'Vol 1', n, `競争 ${i}`),
                'volume-data.json': volumeData(series, 'Vol 1', n, 2, new Date().toISOString()),
            });
            const d = new Map([...made].map(([path, row]) => [path, row.id]));
            const asSeed = asSeeded(made.values());
            const raceStart = Date.now();
            let syncs = 0;
            await expect
                .poll(
                    async () => {
                        expect(await reader.sync()).toEqual(OK);
                        syncs++;
                        expect((await reader.controlState()).held).toBe(true);
                        return mergeProblems(await tree(rig), seed.rootB, [rootD]);
                    },
                    { timeout: 300_000, intervals: [0] },
                )
                .toEqual([]);
            test.info().annotations.push({
                type: `race-${i}`,
                description: `${syncs} syncs, ${Date.now() - raceStart} ms`,
            });
            await expect
                .poll(
                    async () => {
                        expect(await reader.sync()).toEqual(OK);
                        await reader.idle();
                        return {
                            cloudPath: (await listed(reader, series, 'Vol 1'))?.cloudPath ?? null,
                            progress: (await reader.progress(volumeId(n))).page,
                        };
                    },
                    { timeout: 120_000, intervals: [2000] },
                )
                .toEqual({ cloudPath: `${series}/Vol 1.cbz`, progress: 2 });
            rows = await tree(rig);
            merged.push(rootD);
            expect(mergeProblems(rows, seed.rootB, merged)).toEqual([]);
            expect(filesBelow(rows, seed.rootB).get(`${series}/Vol 1.cbz`)?.id).toBe(d.get(`${series}/Vol 1.cbz`));
            // What D was seeded with is no write while it is still the seed's node as
            // seeded: made in raceStart's second, it reads as one, and the reader
            // deletes its merged-away volume-data.json for good. A rewrite in place
            // after the seed (Drive keeps the id) moves its stamp and counts again.
            const ever = await treeEver(rig);
            const allowed = (r) => asSeed(r) || under(ever, r.id, seed.nested);
            expect(writesOutside(ever, seed.rootB, raceStart, allowed)).toEqual([]);
        }
        expect(got503).toEqual([]);
    } finally {
        hold.restore();
        expectNeverSuspect(await storage.stop());
    }
});

test('S9 end: the reader merges the roots while Bunko is down; Bunko restarts with no hold', async ({
    reader,
    rig,
}) => {
    skipWithoutDuplicates(test, rig);
    const ttl = rig.lease.ttl_s;
    test.setTimeout((2 * ttl + 600) * 1000);
    await connect(reader, rig);
    await bunkoSeenAlive(reader);
    const rootB = await libraryRoot(rig);
    const m = await seedMaterial(rig);

    // SIGINT: Bunko writes no `released`; its marker just stops moving.
    await rig.stopBunko();
    const rootC = await topFolder(rig, LIBRARY_FOLDER, '2019-06-01T00:00:00Z');
    const c = await seedFiles(
        rig,
        rootC,
        {
            'S9 Restart/Vol 1.cbz': retag(m.cbz, 'S9 Restart/Vol 1'),
            'S9 Restart/Vol 1.mokuro': mokuroDoc(m, 'S9 Restart', 'Vol 1', 71, '再起動'),
            'volume-data.json': volumeData('S9 Restart', 'Vol 1', 71, 5, '2019-06-01T00:00:00.000Z'),
        },
        '2019-06-01T00:00:00Z',
    );
    // 2 x ttl after the last heartbeat it saw, the reader merges on its own.
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                return (await reader.controlState()).held;
            },
            { timeout: (2 * ttl + 60) * 1000, intervals: [1000] },
        )
        .toBe(false);
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual(OK);
                await reader.idle();
                return mergeProblems(await tree(rig), rootC, [rootB]);
            },
            { timeout: 300_000, intervals: [2000] },
        )
        .toEqual([]);
    let rows = await tree(rig);
    const inC = filesBelow(rows, rootC);
    for (const p of [`${rig.library.series}/Vol 1.cbz`, 'S1 Dup/Vol 2.cbz', 'S9 Restart/Vol 1.cbz']) {
        expect(inC.has(p), `${p} in C`).toBe(true);
    }
    expect(inC.get('S9 Restart/Vol 1.cbz').id).toBe(c.get('S9 Restart/Vol 1.cbz'));
    // The marker moved in with everything else: control files keep every copy.
    expect(inC.has(CONTROL_FILES[0])).toBe(true);

    // Bunko again: it finds its root in the trash and its files under C.
    const storage = sampleStorage(rig);
    const startedAt = Date.now();
    try {
        await rig.startBunko();
        await expect
            .poll(
                async () => {
                    const s = await bunko(rig).storage();
                    return { engine: s.engine, health: s.health?.state, lease: s.lease?.state };
                },
                { timeout: 240_000, intervals: [1000] },
            )
            .toEqual({ engine: 'steady', health: 'online', lease: 'held' });
        // roots()[0] is C: the marker Bunko writes there beats, and the reader trusts it.
        await expect
            .poll(async () => (await markersBeating(rig, rootC, startedAt)).length, { timeout: 120_000 })
            .toBeGreaterThan(0);
        await bunkoSeenAlive(reader);
        expect((await reader.controlState()).held).toBe(true);
        // Nothing went missing for Bunko: the library, the seed, and the nested root.
        expect(await bunkoLists(rig, rig.library.series)).toContain('Vol 1.cbz');
        expect(await bunkoLists(rig, 'S9 Restart')).toContain('Vol 1.cbz');
        expect(await bunkoLists(rig, 'S1 Nested')).toContain('Vol 1.cbz');
        // After a few passes, still no hold.
        await expect.poll(async () => (await bunko(rig).storage()).engine, { timeout: 30_000 }).toBe('steady');
        rows = await tree(rig);
        expect(mergeProblems(rows, rootC, [rootB])).toEqual([]);
        // The nested root (S1's) gets only what Bunko compiles for its series.
        const nested = rows.filter((r) => r.folder && !r.trashed && r.name === LIBRARY_FOLDER && r.id !== rootC);
        const ever = await treeEver(rig);
        const inNested = (r) => nested.some((n) => under(ever, r.id, n.id));
        expect(writesOutside(ever, rootC, startedAt, inNested)).toEqual([]);
    } finally {
        expectNeverSuspect(await storage.stop());
    }
});
