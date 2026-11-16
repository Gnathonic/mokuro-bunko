// local-folder.mjs's pure parts: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { folderDrop, folderProfile } from './local-folder.mjs';

test("the profile allows File System Access writes for the reader's origin alone", () => {
    const p = folderProfile('http://127.0.0.1:4173/some/page?x=1');
    assert.deepEqual(p.profile.content_settings.exceptions, {
        file_system_write_guard: { 'http://127.0.0.1:4173,*': { setting: 1 } },
    });
});

test('a drop is enter, over, then drop, of the folder by its path', () => {
    const events = folderDrop('/tmp/rig/folder/Library');
    assert.deepEqual(
        events.map((e) => e.type),
        ['dragEnter', 'dragOver', 'drop'],
    );
    for (const e of events) {
        assert.deepEqual(e.data, { items: [], files: ['/tmp/rig/folder/Library'], dragOperationsMask: 1 });
        assert.equal(e.x, 10);
        assert.equal(e.y, 10);
    }
});
