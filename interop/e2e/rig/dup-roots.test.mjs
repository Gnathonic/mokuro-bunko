// The duplicate-root checks S1 and S9 read the cloud's tree with: `node --test e2e/rig/`
// in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { asSeeded, mergeProblems, writesOutside } from './dup-roots.mjs';

const folder = (id, parent_id, name, extra = {}) => ({
    id,
    parent_id,
    name,
    folder: true,
    modified: '2020-01-01T00:00:00.000Z',
    trashed: false,
    explicitly_trashed: false,
    ...extra,
});
const file = (id, parent_id, name, modified, extra = {}) => ({
    id,
    parent_id,
    name,
    folder: false,
    modified,
    trashed: false,
    explicitly_trashed: false,
    ...extra,
});

/** Root A merged into B and trashed; B the canonical root; the merge done at `MERGED`. */
const MERGED = Date.parse('2026-10-30T12:00:00.700Z');
const merged = () => [
    folder('B', '', 'mokuro-reader'),
    folder('A', '', 'mokuro-reader', { trashed: true, explicitly_trashed: true }),
    file('vd', 'B', 'volume-data.json', '2026-10-30T12:00:05.000Z'),
];

test('a write after the merge into the trashed root is one outside the canonical root', () => {
    // The pre-merge id of A kept as a write root: the file lands in the trash with A.
    const rows = [...merged(), file('late', 'A', 'positions.json', '2026-10-30T12:00:09.000Z', { trashed: true })];
    assert.deepEqual(
        writesOutside(rows, 'B', MERGED).map((r) => r.id),
        ['late'],
    );
    assert.deepEqual(mergeProblems(rows, 'B', ['A']), ['A (mokuro-reader) still holds positions.json (trashed)']);
});

test("what the merge sent to the trash, written before it, is no write after it", () => {
    const rows = [
        ...merged(),
        file('loser', 'A', 'Vol 1.mokuro', '2021-01-01T00:00:00.000Z', { trashed: true, explicitly_trashed: true }),
    ];
    assert.deepEqual(writesOutside(rows, 'B', MERGED), []);
    assert.deepEqual(mergeProblems(rows, 'B', ['A']), []);
});

test('a write in the merge second counts on a cloud that keeps whole seconds', () => {
    // MEGA stamps a node in whole seconds: written at 12:00:00.900, it reads 12:00:00.
    const rows = [...merged(), file('root-file', '', 'volume-data.json', '2026-10-30T12:00:00.000Z')];
    assert.deepEqual(
        writesOutside(rows, 'B', MERGED).map((r) => r.id),
        ['root-file'],
    );
});

test('writes below the canonical root, or allowed, are not outside it', () => {
    const rows = [
        ...merged(),
        folder('S', 'B', 'Series'),
        file('in-b', 'S', 'series.json', '2026-10-30T12:00:30.000Z'),
        folder('N', 'X', 'mokuro-reader'),
        file('nested', 'N', 'series.json', '2026-10-30T12:00:30.000Z'),
    ];
    assert.deepEqual(
        writesOutside(rows, 'B', MERGED, (r) => r.parent_id === 'N'),
        [],
    );
});

test("a seeded node is no write while the seed's, and one again once rewritten in place", () => {
    // Root D seeded in the race's second; Drive keeps a file's id when its content is
    // rewritten, so only the stamp tells the seed's bytes from a later write.
    const RACE = Date.parse('2026-10-30T12:00:00.400Z');
    const seededAt = '2026-10-30T12:00:00.300Z';
    const seed = [
        folder('D', '', 'mokuro-reader', { modified: seededAt }),
        file('d-vd', 'D', 'volume-data.json', seededAt),
        file('d-cbz', 'D', 'Vol 1.cbz', seededAt),
    ];
    // The rows `/_sim/node` answered as it made each node: no tree read after the seed,
    // which a write in between would already have moved.
    const allowed = asSeeded(seed.slice(1));
    // As seeded, then trashed (and deleted for good) with D: no write.
    const asSeed = [
        ...merged(),
        { ...seed[0], trashed: true, explicitly_trashed: true },
        { ...seed[1], trashed: true, deleted: true },
    ];
    assert.deepEqual(writesOutside(asSeed, 'B', RACE, allowed), []);
    // The same id rewritten after the race started: a write outside B.
    const rewritten = [
        ...merged(),
        { ...seed[0], trashed: true, explicitly_trashed: true },
        { ...seed[1], modified: '2026-10-30T12:00:02.000Z', trashed: true },
    ];
    assert.deepEqual(
        writesOutside(rewritten, 'B', RACE, allowed).map((r) => r.id),
        ['d-vd'],
    );
});
