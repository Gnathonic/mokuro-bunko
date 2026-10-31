// A restore's deletes through Bunko, against what the cloud holds: `node --test e2e/rig/`
// in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cloudDeleter } from './cloud-restore.mjs';

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
    assert.deepEqual(await deleter('positions.json'), { status: 204, unseenMs: null });
    assert.equal(calls.length, 3);
});

test('a 404 for a file the cloud no longer holds is a file gone', async () => {
    const { deleter, calls } = fake([404], () => false);
    assert.deepEqual(await deleter('positions.json'), { status: 404, unseenMs: null });
    assert.equal(calls.length, 1);
});

test('any other answer stands at once', async () => {
    for (const status of [204, 403, 500]) {
        const { deleter, calls } = fake([status]);
        assert.deepEqual(await deleter('x'), { status, unseenMs: null });
        assert.equal(calls.length, 1);
    }
});

test('a 404 that outlasts the wait stands, and says for how long Bunko did not see the file', async () => {
    const { deleter, now } = fake([404]);
    const got = await deleter('history/dev/');
    assert.equal(got.status, 404);
    assert.ok(got.unseenMs >= 10_000, JSON.stringify(got));
    assert.ok(now() < 15_000, `waited ${now()} ms`);
});

test('the wait is one budget for the whole restore, not one per file', async () => {
    const { deleter, now } = fake([404]);
    await deleter('a');
    const spent = now();
    const got = await deleter('b');
    assert.equal(got.status, 404);
    // The budget is spent: one more look, no new wait.
    assert.equal(now(), spent);
    assert.notEqual(got.unseenMs, null);
});
