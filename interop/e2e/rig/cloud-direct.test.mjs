// The specs' view of the cloud's files: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { cloudFile, cloudFiles } from './cloud-direct.mjs';

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

test("a folder library's files are the folder's own, Bunko's trash left out", async () => {
    const root = mkdtempSync(path.join(tmpdir(), 'rig-folder-'));
    const put = (rel, text) => {
        mkdirSync(path.dirname(path.join(root, rel)), { recursive: true });
        writeFileSync(path.join(root, rel), text);
    };
    put('Rig Series/Vol 1.cbz', 'one');
    put('.mokuro-queue.json', '{}');
    put('.mokuro-bunko-trash/20261111T000000Z-ab/Rig Series/Vol 1.edit.mokuro.bunko-trashed', '{}');
    const rig = { provider: 'folder', folder: { path: root } };
    const paths = (await cloudFiles(rig)).map((f) => f.path).sort();
    assert.deepEqual(paths, ['.mokuro-queue.json', 'Rig Series/Vol 1.cbz']);
    const vol = await cloudFile(rig, 'Rig Series/Vol 1.cbz');
    assert.equal(vol.size, 3);
    assert.equal(await cloudFile(rig, 'Rig Series/Vol 1.edit.mokuro'), null);
    await assert.rejects(cloudFiles({ provider: 'folder' }), /names no library folder/);
});
