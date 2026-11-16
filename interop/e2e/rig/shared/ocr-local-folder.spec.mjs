// shared, local-folder variant (queue file v2 §12.4): the OCR round trip of
// `shared/ocr.spec.mjs` with the reader on a Local Folder library that Bunko's
// local-folder backend manages (store root = the folder the reader picks). It runs on
// the `folder` provider only (`cargo xtask interop rig shared --provider folder`):
// Bunko keeps its library in `RIG_CONTEXT.folder.path`, and the reader's driver signs
// its Local Folder in to that same folder, dropped on the page in place of the picker
// (`connectFolder`, capability `local-folder`; local-folder.mjs says how).
//
// A local folder is read with no network at all, so the request-log checks of the
// cloud variant have nothing to look at (`wire: false`): the store side carries the
// spec (no root queue file, the series file written under the holder's id, the volume
// leaving it with a `done` entry, the sidecar beside the archive) and the card and the
// pulled OCR the reader side.
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { expect, test } from '../fixtures.mjs';
import { connectFolder } from '../local-folder.mjs';
import { ocrRoundTrip } from './ocr-flow.mjs';

test.use({ needs: ['series-queue-v2', 'local-folder'], folderBrowser: true });

test.skip(({ rig }) => rig.provider !== 'folder', 'a Local Folder library: the folder provider only');

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
    const dir = rig.folder?.path;
    if (!dir) throw new Error('RIG_CONTEXT.folder.path is not set: the folder provider names its library folder');
    await ocrRoundTrip({
        reader,
        rig,
        signIn: async () => {
            const { status, sync } = await connectFolder(reader, dir);
            expect(status).toMatchObject({ isAuthenticated: true });
            expect(sync).toEqual({ ok: true, errors: [] });
        },
        store: folderStore(dir),
        wire: false,
    });
});
