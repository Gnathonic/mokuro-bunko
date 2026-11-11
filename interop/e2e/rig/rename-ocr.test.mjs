// What S6 reads of the library behind the reader's back: `node --test e2e/rig/` in
// interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { librarySide } from './rename-ocr.mjs';

/** Until `check()` holds, or `ms` passed: whether it held. */
async function until(check, ms = 5000) {
    const end = Date.now() + ms;
    for (;;) {
        if (await check()) return true;
        if (Date.now() > end) return false;
        await new Promise((r) => setTimeout(r, 20));
    }
}

for (const [provider, inCloud, root] of [
    ['local', false, ['bunko', 'library']],
    ['rclone', true, ['rclone-root', 'mokuro-reader']],
]) {
    test(`on ${provider}, a sidecar made and swept while watched is still among the names`, async () => {
        const workdir = mkdtempSync(path.join(tmpdir(), 'rig-library-side-'));
        const library = path.join(workdir, ...root);
        mkdirSync(library, { recursive: true });
        const side = librarySide({ workdir, provider }, inCloud);
        side.watch();
        try {
            // A series folder made after the watch started (an upload makes it), and in
            // it a numbered sidecar that lives a while, as Bunko's does until its sweep
            // (node's recursive watch on Linux reports no file gone by the time it looks).
            const series = path.join(library, 'S6 Rename');
            mkdirSync(series);
            await new Promise((r) => setTimeout(r, 300));
            const numbered = path.join(series, 'Vol 1 renamed_1.mokuro');
            writeFileSync(numbered, '{}');
            await new Promise((r) => setTimeout(r, 300));
            rmSync(numbered);
            const seen = await until(async () => (await side.everNames()).includes('Vol 1 renamed_1.mokuro'));
            assert.ok(seen, `${provider}: the swept sidecar was never seen`);
        } finally {
            side.unwatch();
            rmSync(workdir, { recursive: true, force: true });
        }
    });
}

for (const provider of ['google', 'mega']) {
    test(`on ${provider}, a sidecar Bunko wrote and deleted for good is still among the names`, async () => {
        // Drive and MEGA keep no node Bunko deleted for good (its own proven files):
        // only cloudsim's `include_deleted` rows still name it.
        const asked = [];
        const rows = [
            { id: 'a', name: 'Vol 2 renamed.mokuro', folder: false, trashed: false },
            { id: 'b', name: 'Vol 2 renamed_1.mokuro', folder: false, trashed: true, deleted: true },
        ];
        const rig = {
            workdir: tmpdir(),
            provider,
            simAccount: 'acct',
            sim: async (method, query) => {
                asked.push(`${method} ${query}`);
                return /[?&]include_deleted=1(&|$)/.test(query) ? rows : rows.filter((r) => !r.deleted);
            },
        };
        const names = await librarySide(rig, true).everNames();
        assert.deepEqual(names.sort(), ['Vol 2 renamed.mokuro', 'Vol 2 renamed_1.mokuro']);
        assert.equal(asked.length, 1);
    });
}
