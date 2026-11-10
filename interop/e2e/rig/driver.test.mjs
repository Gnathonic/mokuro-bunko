// The driver's two lookups of a volume that is not installed (`open`, `pendingOcr`),
// asked again once when the reader answers that it has no such volume:
// `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { driver } from './driver.mjs';

const ID = '00000000-0000-4000-8000-000000000000';
const notOnDevice = () => new Error(`page.evaluate: Error: No volume ${ID} on this device\n    at Object.je`);

/**
 * A page whose `__mokuroInterop` answers each call with the next of `answers` (an
 * Error is thrown), and the calls it got as `[method, args]`.
 */
function fakePage(answers) {
    const calls = [];
    return {
        calls,
        evaluate: async (_fn, [name, args]) => {
            calls.push([name, args]);
            const answer = answers.shift();
            if (answer instanceof Error) throw answer;
            return answer;
        },
    };
}

for (const method of ['open', 'pendingOcr']) {
    test(`${method}: a volume made a row between the lookup's two reads is found when asked again`, async () => {
        const page = fakePage([notOnDevice(), { id: ID }]);
        assert.deepEqual(await driver(page)[method](ID), { id: ID });
        assert.deepEqual(page.calls, [
            [method, [ID]],
            [method, [ID]],
        ]);
    });

    test(`${method}: a volume that is not there says so, after two lookups and no more`, async () => {
        const page = fakePage([notOnDevice(), notOnDevice(), { id: ID }]);
        await assert.rejects(driver(page)[method](ID), new RegExp(`No volume ${ID} on this device`));
        assert.equal(page.calls.length, 2);
    });

    test(`${method}: any other failure is the spec's at once`, async () => {
        const page = fakePage([new Error('Vol 1 was not installed by its download'), { id: ID }]);
        await assert.rejects(driver(page)[method](ID), /was not installed by its download/);
        assert.equal(page.calls.length, 1);
    });
}

test('a method that changes something is never asked twice', async () => {
    const writers = ['setPage', 'renameVolume', 'deleteVolume', 'editFirstBlock', 'publishCurated', 'selectLayer'];
    for (const method of writers) {
        const page = fakePage([notOnDevice(), 'done']);
        await assert.rejects(driver(page)[method](ID), /on this device/);
        assert.equal(page.calls.length, 1, method);
    }
});
