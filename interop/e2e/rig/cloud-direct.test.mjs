// The specs' view of the cloud's files: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cloudFile } from './cloud-direct.mjs';

/** cloudsim's control API answering `/_sim/tree` with `rows`. */
function fakeRig(rows) {
    return {
        provider: 'mega',
        simAccount: 'rig',
        sim: async (method, p) => {
            assert.equal(method, 'GET');
            assert.match(p, /^tree\?/);
            return rows;
        },
    };
}

const folder = (id, parent_id, name) => ({ id, parent_id, name, folder: true });
const file = (id, parent_id, name, modified) => ({
    id,
    parent_id,
    name,
    folder: false,
    size: 1,
    sha256: id,
    modified,
});

test('of two copies of one path, the one written last', async () => {
    // A merge of two `mokuro-reader` roots keeps both copies of a control file; Bunko
    // goes on writing one of them, the other stays as the merge left it. Which comes
    // first in the tree says nothing.
    const rows = [
        folder('root', null, 'mokuro-reader'),
        file('live', 'root', '.mokuro-queue.json', '2026-10-30T12:05:00Z'),
        file('stale', 'root', '.mokuro-queue.json', '2026-10-30T12:00:00Z'),
    ];
    assert.equal((await cloudFile(fakeRig(rows), '.mokuro-queue.json')).id, 'live');
    assert.equal((await cloudFile(fakeRig([rows[0], rows[2], rows[1]]), '.mokuro-queue.json')).id, 'live');
});

test('one copy, or none', async () => {
    const rows = [folder('root', null, 'mokuro-reader'), file('only', 'root', 'volume-data.json', '2026-10-30T12:00:00Z')];
    assert.equal((await cloudFile(fakeRig(rows), 'volume-data.json')).id, 'only');
    assert.equal(await cloudFile(fakeRig(rows), 'profiles.json'), null);
});
