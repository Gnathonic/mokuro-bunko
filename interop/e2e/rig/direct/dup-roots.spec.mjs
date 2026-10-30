// S9 (B8.6, plan X5) on a cloud with no Bunko: the reader alone merges duplicate
// `mokuro-reader` roots (#308 against cloudsim). The X5 seed (dup-roots.mjs) beside the
// library's root A, each top-level root also holding a copy of Bunko's queue file (a
// control file: every copy kept, never trashed). The reader's sync merges A into the
// older top-level root B and trashes A once it is empty, keeps the newer of two
// same-named sidecars, keeps both `volume-data.json` copies' progress, leaves the nested
// root as it is and reads it, and writes only into B from then on. The end of S9, a
// Bunko restarted after such a merge, is in shared/dup-roots-lease.spec.mjs: this
// suite runs no Bunko.
//
// The merge moves the suite's whole library: every spec after this one reads it under
// the other root folder of the same name.
import { test, expect } from '../fixtures.mjs';
import { signIn } from '../cloud-direct.mjs';
import {
    CONTROL_FILES,
    filesBelow,
    mergeProblems,
    nestedState,
    nodeBytes,
    seedX5,
    skipWithoutDuplicates,
    topRoots,
    tree,
    volumeData,
    volumeId,
    writesOutside,
} from '../dup-roots.mjs';

test.use({ needs: ['dedup'] });

const OK = { ok: true, errors: [] };
const QUEUE = CONTROL_FILES[1];

/** The volume `series`/`title` as the reader lists it, or null. */
async function listed(reader, series, title) {
    return (await reader.library()).volumes.find((v) => v.series === series && v.title === title) ?? null;
}

test('S9: the reader alone merges duplicate roots into the older top-level root', async ({ reader, rig }) => {
    skipWithoutDuplicates(test, rig);
    test.setTimeout(420_000);
    const series = rig.library.series;
    const { sync } = await signIn(reader, rig);
    expect(sync).toEqual(OK);
    const tops = topRoots(await tree(rig));
    expect(tops, 'one library root before the seed').toHaveLength(1);
    const rootA = tops[0].id;

    // A's progress is the library's Vol 1 on page 4; B's is its own Vol 2 on page 3.
    const queue = (who) => Buffer.from(JSON.stringify({ version: 1, instance_id: who, volumes: [], done: [] }));
    const seed = await seedX5(rig, rootA, {
        extra: {
            a: {
                'volume-data.json': volumeData(series, 'Vol 1', 0, 4, new Date().toISOString()),
                [QUEUE]: queue('a'),
            },
            b: { [QUEUE]: queue('b') },
        },
    });
    const nestedBefore = nestedState(await tree(rig), seed);

    // The merge runs after a listing; each sync lists.
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual(OK);
                await reader.idle();
                return mergeProblems(await tree(rig), seed.rootB, [rootA, seed.a.get('S1 Dup')]);
            },
            { timeout: 240_000, intervals: [2000] },
        )
        .toEqual([]);
    const mergedAt = Date.now();
    let rows = await tree(rig);
    const inB = filesBelow(rows, seed.rootB);
    expect(inB.get('S1 Dup/Vol 1.mokuro')?.id).toBe(seed.a.get('S1 Dup/Vol 1.mokuro'));
    expect(rows.find((r) => r.id === seed.b.get('S1 Dup/Vol 1.mokuro'))?.trashed).toBe(true);
    for (const p of ['S1 Dup/Vol 1.cbz', 'S1 Dup/Vol 2.cbz', `${series}/Vol 1.cbz`, `${series}/Vol 1.mokuro`]) {
        expect(inB.has(p), `${p} in B`).toBe(true);
    }
    // Both queue files, moved in beside each other.
    const queues = rows.filter((r) => r.name === QUEUE && !r.trashed && r.parent_id === seed.rootB);
    expect(queues.map((r) => r.id).sort()).toEqual([seed.a.get(QUEUE), seed.b.get(QUEUE)].sort());
    expect(rows.find((r) => r.id === seed.nested)?.trashed).toBe(false);
    expect(nestedState(rows, seed)).toEqual(nestedBefore);

    // Every root read: B's volume, the nested one, and both copies' progress.
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual(OK);
                await reader.idle();
                return {
                    dup2: (await listed(reader, 'S1 Dup', 'Vol 2'))?.cloudPath ?? null,
                    nested: (await listed(reader, 'S1 Nested', 'Vol 1'))?.cloudPath ?? null,
                    progress: [(await reader.progress(volumeId(0))).page, (await reader.progress(volumeId(62))).page],
                };
            },
            { timeout: 120_000, intervals: [2000] },
        )
        .toEqual({ dup2: 'S1 Dup/Vol 2.cbz', nested: 'S1 Nested/Vol 1.cbz', progress: [4, 3] });
    // The progress file B keeps holds both.
    rows = await tree(rig);
    const vd = filesBelow(rows, seed.rootB).get('volume-data.json');
    expect(vd, 'volume-data.json in B').toBeTruthy();
    const progress = JSON.parse((await nodeBytes(rig, vd.id)).toString('utf8'));
    expect(progress).toMatchObject({ [volumeId(0)]: { progress: 4 }, [volumeId(62)]: { progress: 3 } });

    // From the merge on, the reader writes only into B.
    expect(await reader.setPage((await reader.open((await listed(reader, series, 'Vol 1')).id)).id, 2)).toEqual({
        page: 2,
        completed: false,
    });
    expect(await reader.sync()).toEqual(OK);
    await reader.idle();
    await reader.settleSeriesWrites();
    rows = await tree(rig);
    expect(writesOutside(rows, seed.rootB, mergedAt)).toEqual([]);
    expect(mergeProblems(rows, seed.rootB, [rootA, seed.a.get('S1 Dup')])).toEqual([]);
    expect(nestedState(rows, seed)).toEqual(nestedBefore);
});
