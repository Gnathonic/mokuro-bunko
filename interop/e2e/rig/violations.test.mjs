// Taking cloudsim's violations: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { takeViolations } from './violations.mjs';

/** cloudsim's control API over a list a client (Bunko's poll) adds to at any moment. */
function fakeSim({ legacy = false } = {}) {
    const list = ['drive GET /files: unknown clause'];
    const recorded = [...list];
    const calls = [];
    const sim = async (method, path) => {
        calls.push(`${method} ${path}`);
        assert.equal(path, 'violations');
        if (method === 'GET') {
            const seen = [...list];
            // Recorded after this read, before any clear.
            list.push('graph PUT /content: no body');
            recorded.push('graph PUT /content: no body');
            return seen;
        }
        assert.equal(method, 'DELETE');
        const taken = list.splice(0);
        return legacy ? null : taken;
    };
    return { sim, calls, list, recorded };
}

test('one call reads and clears: a violation recorded meanwhile is never wiped unseen', async () => {
    const { sim, calls, list, recorded } = fakeSim();
    assert.deepEqual(await takeViolations(sim), recorded);
    assert.deepEqual(calls, ['DELETE violations']);
    assert.deepEqual(list, []);
});

test("a cloudsim that answers no list is an error, not 'no violations'", async () => {
    const { sim } = fakeSim({ legacy: true });
    await assert.rejects(takeViolations(sim), /not the list it cleared/);
});
