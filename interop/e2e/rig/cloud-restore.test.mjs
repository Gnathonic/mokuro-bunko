// A restore's deletes through Bunko, against what the cloud holds: `node --test e2e/rig/`
// in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cloudDeleter, unseenNote } from './cloud-restore.mjs';

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
