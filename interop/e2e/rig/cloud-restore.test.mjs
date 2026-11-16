// A restore's deletes through Bunko, against what the cloud holds: `node --test e2e/rig/`
// in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
    cloudDeleter,
    copiesByPath,
    copyLines,
    copyProblems,
    perUserPath,
    unseenNote,
    untilQuiet,
} from './cloud-restore.mjs';

/**
 * A Bunko that answers `answers` in turn (the last one from then on), a cloud that holds
 * the file while `held()`, and a clock that moves only when the deleter pauses.
 */
function fake(answers, held = () => true) {
    let t = 0;
    const calls = [];
    const deleter = cloudDeleter({
        del: async (target) => {
            calls.push(target);
            return answers[Math.min(calls.length, answers.length) - 1];
        },
        holds: async () => held(calls.length),
        waitMs: 10_000,
        pause: async (ms) => {
            t += ms;
        },
        clock: () => t,
    });
    return { deleter, calls, now: () => t };
}

test('a 404 for a file the cloud still holds is asked again until Bunko finds it', async () => {
    const { deleter, calls } = fake([404, 404, 204]);
    // Two looks 2 s apart: how long Bunko did not see it is kept, though it found it.
    assert.deepEqual(await deleter('positions.json'), { status: 204, unseenMs: 4000, gaveUpMs: null });
    assert.equal(calls.length, 3);
});

test('a 404 for a file the cloud no longer holds is a file gone', async () => {
    const { deleter, calls } = fake([404], () => false);
    assert.deepEqual(await deleter('positions.json'), { status: 404, unseenMs: null, gaveUpMs: null });
    assert.equal(calls.length, 1);
});

test('any other answer stands at once', async () => {
    for (const status of [204, 403, 500]) {
        const { deleter, calls } = fake([status]);
        assert.deepEqual(await deleter('x'), { status, unseenMs: null, gaveUpMs: null });
        assert.equal(calls.length, 1);
    }
});

test('a 404 that outlasts the wait stands, and says for how long Bunko did not see the file', async () => {
    const { deleter, now } = fake([404]);
    const got = await deleter('history/dev/');
    assert.equal(got.status, 404);
    assert.ok(got.unseenMs >= 10_000, JSON.stringify(got));
    assert.ok(got.gaveUpMs >= 10_000, JSON.stringify(got));
    assert.ok(now() < 15_000, `waited ${now()} ms`);
});

test('the wait is one budget for the whole restore, not one per file', async () => {
    const { deleter, now } = fake([404]);
    await deleter('a');
    const spent = now();
    const got = await deleter('b');
    assert.equal(got.status, 404);
    // The budget is spent: one more look, no new wait. It gave up on this file too,
    // as long into the wait as the first.
    assert.equal(now(), spent);
    assert.equal(got.unseenMs, 0);
    assert.ok(got.gaveUpMs >= 10_000, JSON.stringify(got));
});

test('a file Bunko did not see is noted when it gave up, or when it took longer than lag', () => {
    const after = 10_000;
    assert.equal(unseenNote('a', { status: 204, unseenMs: null, gaveUpMs: null }, after), null);
    assert.equal(unseenNote('a', { status: 204, unseenMs: 4000, gaveUpMs: null }, after), null);
    assert.equal(
        unseenNote('positions.json', { status: 204, unseenMs: 40_000, gaveUpMs: null }, after),
        'positions.json: Bunko answered 404 for 40 s while the cloud held it, then listed it',
    );
    assert.equal(
        unseenNote('history/dev/', { status: 404, unseenMs: 0, gaveUpMs: 62_000 }, after),
        "history/dev/: Bunko still answered 404 while the cloud held it, 62 s into the restore's wait",
    );
});

const copy = (id, sha256, modified = '2026-11-13T09:13:00.000Z') => ({ id, sha256, modified });

test('copies at one path all count, oldest first', () => {
    const files = [
        { path: 'volume-data.json', id: 'N2', sha256: 'b', modified: '2026-11-13T09:13:05.000Z' },
        { path: 'Rig Series/Vol 1.cbz', id: 'N3', sha256: 'c', modified: '2026-11-13T09:00:00.000Z' },
        { path: 'volume-data.json', id: 'N1', sha256: 'a', modified: '2026-11-13T09:13:00.000Z' },
        { path: 'history/dev/2026-11.json', id: 'N4', sha256: 'd', modified: '2026-11-13T09:00:00.000Z' },
    ];
    assert.deepEqual(copiesByPath(files, perUserPath), {
        'volume-data.json': [files[2], files[0]],
        'history/dev/2026-11.json': [files[3]],
    });
});

test('one copy of the wanted bytes is no problem; a doubled write is extra copies', () => {
    const want = { 'volume-data.json': 'a' };
    assert.deepEqual(copyProblems(want, { 'volume-data.json': [copy('N1', 'a')] }), []);
    // The regression (reader PR #368): Bunko's write of the owner's file reached MEGA
    // twice, the same bytes as two nodes. Removing any one leaves the right bytes.
    assert.deepEqual(copyProblems(want, { 'volume-data.json': [copy('N1', 'a'), copy('N2', 'a')] }), [
        { path: 'volume-data.json', why: '2 copies', extra: 'copies' },
    ]);
    assert.deepEqual(copyLines(want, { 'volume-data.json': [copy('N1', 'a'), copy('N2', 'a')] }), [
        '2 copies volume-data.json',
    ]);
});

test('a foreign copy beside the wanted bytes, other bytes, a missing or an added file are problems', () => {
    const want = { 'volume-data.json': 'a', 'positions.json': 'p' };
    assert.deepEqual(
        copyProblems(want, {
            // The direct reader's own copy, which Bunko's Replace left beside its own.
            'volume-data.json': [copy('N1', 'reader'), copy('N2', 'a')],
            'goals.json': [copy('N3', 'g')],
        }),
        [
            { path: 'volume-data.json', why: '2 copies, other bytes', extra: 'all' },
            { path: 'positions.json', why: 'gone', extra: null },
            { path: 'goals.json', why: 'added', extra: 'all' },
        ],
    );
    assert.deepEqual(copyLines(want, { 'volume-data.json': [copy('N1', 'b')], 'positions.json': [copy('N4', 'p')] }), [
        'changed volume-data.json',
    ]);
    assert.deepEqual(copyLines({}, { 'goals.json': [copy('N3', 'g'), copy('N5', 'g')] }), [
        'added, 2 copies goals.json',
    ]);
});

/** A clock that moves only when the wait pauses, and a read that answers `answers` in turn. */
function quietFake(answers) {
    let t = 0;
    let reads = 0;
    return {
        read: async () => answers[Math.min(reads++, answers.length - 1)],
        pause: async (ms) => {
            t += ms;
        },
        clock: () => t,
        reads: () => reads,
    };
}

test('the wait ends once the tree has held still for the quiet period', async () => {
    // The second node of a doubled write lands 3 s in; quiet for 5 s after that.
    const f = quietFake([['N1'], ['N1'], ['N1'], ['N1', 'N2']]);
    const got = await untilQuiet(f.read, { quietMs: 5000, timeoutMs: 60_000, pause: f.pause, clock: f.clock });
    assert.deepEqual(got, { value: ['N1', 'N2'], waitedMs: 8000 });
});

test('a tree that never holds still is an error, not a pass', async () => {
    let n = 0;
    let t = 0;
    await assert.rejects(
        untilQuiet(async () => [`N${n++}`], {
            quietMs: 5000,
            timeoutMs: 20_000,
            pause: async (ms) => {
                t += ms;
            },
            clock: () => t,
        }),
        /still changed 20000 ms on/,
    );
});

test('only what `key` picks must hold still, and the answer is the latest look', async () => {
    // Bunko's marker beats on its own; the per-user files are what the wait is for.
    const f = quietFake([
        { marker: 1, own: ['N1'] },
        { marker: 2, own: ['N1'] },
        { marker: 3, own: ['N1'] },
        { marker: 4, own: ['N1'] },
    ]);
    const got = await untilQuiet(f.read, {
        quietMs: 3000,
        timeoutMs: 60_000,
        key: (v) => v.own,
        pause: f.pause,
        clock: f.clock,
    });
    assert.deepEqual(got, { value: { marker: 4, own: ['N1'] }, waitedMs: 3000 });
});
