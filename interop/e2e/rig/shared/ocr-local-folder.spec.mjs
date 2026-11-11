// shared, local-folder variant (queue file v2 §12.4): the OCR round trip of
// `shared/ocr.spec.mjs` with the reader on a Local Folder library that Bunko's
// local-folder backend manages (store root = the folder the reader picks).
//
// SKIPPED until the rig has that provider. Bunko's local-folder backend is a separate
// pull request, not on this branch, and neither the runner (`cargo xtask interop rig
// --provider`) nor the reader's rig driver (no `connectFolder`, capability
// `local-folder`) can stand it up yet. What landing it needs: a `folder` provider in
// the runner whose Bunko serves a library folder on disk (`RIG_CONTEXT.folder.dir`),
// the reader's driver opening that folder in place of the File System Access picker
// (`connectFolder({dir})`, behind the `local-folder` capability), and this spec's skip
// condition then lets it run.
//
// A local folder is read with no network at all, so the request-log checks of the
// cloud variant have nothing to look at (`wire: false`): the store side carries the
// spec (no root queue file, the series file written under the holder's id, the volume
// leaving it with a `done` entry, the sidecar beside the archive) and the card and the
// pulled OCR the reader side.
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from '../fixtures.mjs';
import { ocrRoundTrip } from './ocr-flow.mjs';

test.use({ needs: ['series-queue-v2', 'local-folder'] });

test.skip(
    ({ rig }) => rig.provider !== 'folder',
    'needs the rig\'s local-folder provider: Bunko\'s local-folder backend is a separate PR, not on this branch',
);

/** The library folder on disk, read as Bunko's store holds it. */
function folderStore(dir) {
    const at = (rel) => path.join(dir, ...rel.split('/'));
    return {
        json: async (rel) => (existsSync(at(rel)) ? JSON.parse(readFileSync(at(rel), 'utf8').replace(/^﻿/, '')) : null),
        has: async (rel) => existsSync(at(rel)),
    };
}

test('Bunko OCRs a volume the reader uploaded to a local folder: its job on the card, its sidecar pulled', async ({
    reader,
    rig,
}) => {
    test.setTimeout(600_000);
    const dir = JSON.parse(process.env.RIG_CONTEXT ?? '{}').folder?.dir;
    if (!dir) throw new Error('RIG_CONTEXT.folder.dir is not set: the folder provider names its library folder');
    await ocrRoundTrip({
        reader,
        rig,
        signIn: async () => {
            const { sync } = await reader.page.evaluate((d) => globalThis.__mokuroInterop.connectFolder({ dir: d }), dir);
            if (!sync?.ok) throw new Error(`connectFolder: ${JSON.stringify(sync)}`);
        },
        store: folderStore(dir),
        wire: false,
    });
});
