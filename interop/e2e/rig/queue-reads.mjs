// Every read of an OCR queue file or the lease marker the reader made, read off the
// browser's own requests (queue file v2, docs/design/queue-file-v2.md §12.4): the
// series files `<Series>/.mokuro-series-queue.json`, the v1 root `.mokuro-queue.json`
// and the marker `.mokuro-bunko.json`. Kept apart so the naming is tested without a
// browser (queue-reads.test.mjs).
//
// What names the file, per wire:
// - WebDAV (Bunko's own, or rclone): the path, `/mokuro-reader/[<Series>/]<name>`.
// - Drive: a `files.list` whose `q` names it (`name='…'`), its folders the ids `in
//   parents`; a download, `files/<id>?alt=media`, by the node it reads.
// - Graph: `root:/mokuro-reader/[<Series>/]<name>`; a download, `items/<id>/content` or
//   the pre-authenticated URL it redirects to (`…/preauth/<account>/<id>`), by its node.
// - MEGA: a download's `g` command (`n`: the node's handle). A MEGA read of an unchanged
//   file is a lookup in the live tree, no request at all, so only downloads show.
// Node ids are looked up in cloudsim's tree (trashed nodes included).

import { LIBRARY_FOLDER } from './cloud-direct.mjs';

export const SERIES_QUEUE_FILE = '.mokuro-series-queue.json';
export const ROOT_QUEUE_FILE = '.mokuro-queue.json';
export const MARKER_FILE = '.mokuro-bunko.json';

/** The kind of read a file name is, or null for any other file. */
const KIND = new Map([
    [SERIES_QUEUE_FILE, 'series'],
    [ROOT_QUEUE_FILE, 'root'],
    [MARKER_FILE, 'marker'],
]);

/**
 * What one request reads, before node ids are looked up: `[{name, folder}]` for the
 * names it carries (folder null at the library root, or `{ids}` for folders named by
 * id) and `[id]` for the nodes it reads by id. Exported for the tests.
 */
export function namedInRequest({ provider, method, url, body }) {
    const u = new URL(url);
    const names = [];
    const nodes = [];
    const segs = u.pathname.split('/').filter(Boolean).map((s) => decodeURIComponent(s));
    if (provider === 'webdav') {
        if (method !== 'GET' && method !== 'HEAD') return { names, nodes };
        const at = segs.indexOf(LIBRARY_FOLDER);
        const rest = at < 0 ? [] : segs.slice(at + 1);
        if (rest.length === 1) names.push({ name: rest[0], folder: null });
        else if (rest.length === 2) names.push({ name: rest[1], folder: rest[0] });
        return { names, nodes };
    }
    if (provider === 'google') {
        const q = u.searchParams.get('q');
        if (q && /\/drive\/v3\/files$/.test(u.pathname)) {
            const m = q.match(/name\s*=\s*'((?:[^'\\]|\\.)*)'/);
            if (m) {
                const name = m[1].replace(/\\(.)/g, '$1');
                const ids = [...q.matchAll(/'((?:[^'\\]|\\.)*)'\s+in\s+parents/g)].map((x) =>
                    x[1].replace(/\\(.)/g, '$1'),
                );
                names.push({ name, folder: { ids } });
            }
        }
        const media = u.pathname.match(/\/drive\/v3\/files\/([^/]+)$/);
        if (media && u.searchParams.get('alt') === 'media') nodes.push(decodeURIComponent(media[1]));
        return { names, nodes };
    }
    if (provider === 'microsoft') {
        const p = decodeURIComponent(u.pathname);
        const byPath = p.match(/\/root:\/(.+?)(?::(?:\/.*)?)?$/);
        if (byPath) {
            const parts = byPath[1].split('/');
            if (parts[0] === LIBRARY_FOLDER && parts.length === 2) names.push({ name: parts[1], folder: null });
            if (parts[0] === LIBRARY_FOLDER && parts.length === 3) names.push({ name: parts[2], folder: parts[1] });
        }
        const item = p.match(/\/items\/([^/]+)\/content$/);
        if (item) nodes.push(item[1]);
        const preauth = p.match(/\/graph\/preauth\/[^/]+\/([^/]+)$/);
        if (preauth) nodes.push(preauth[1]);
        return { names, nodes };
    }
    if (provider === 'mega') {
        let cmds = [];
        try {
            cmds = JSON.parse(body ?? '[]');
        } catch {
            return { names, nodes };
        }
        for (const c of Array.isArray(cmds) ? cmds : []) {
            if (c?.a === 'g' && typeof c.n === 'string') nodes.push(c.n);
        }
        return { names, nodes };
    }
    return { names, nodes };
}

/** cloudsim's tree, trashed nodes included: id → {name, parent} (parent: its row). */
async function nodeTree(rig) {
    const rows = await rig.sim(
        'GET',
        `tree?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&include_trashed=1`,
    );
    const byId = new Map(rows.map((r) => [r.id, r]));
    return (id) => {
        const r = byId.get(id);
        if (!r) return null;
        return { name: r.name, parent: byId.get(r.parent_id) ?? null };
    };
}

/** A node's folder as a read names it: null at the library root. */
const folderOf = (parent) => (!parent || parent.name === LIBRARY_FOLDER ? null : parent.name);

/**
 * Every queue-file and marker read the reader (the page and its workers) made since
 * this was called: `await queueReads.reads()` gives `[{file: 'series'|'root'|'marker',
 * folder, what}]` in request order (`folder`: the series folder of a series file, null
 * at the library root), `queueReads.mark()` a position to slice from:
 * `reads(mark)` gives only those made after it. `origins` are where the reader reads
 * the library (cloudsim's two, rclone's, or Bunko's).
 */
export function recordQueueReads(page, rig) {
    const provider = rig.provider === 'local' || rig.provider === 'rclone' ? 'webdav' : rig.provider;
    const origins = new Set(
        (provider === 'webdav' ? [rig.provider === 'rclone' ? rig.rcloneUrl : rig.bunkoUrl] : [rig.cloudsimUrl, rig.preauthUrl])
            .filter(Boolean)
            .map((u) => new URL(u).origin),
    );
    const seen = [];
    page.on('request', (request) => {
        const url = request.url();
        let origin;
        try {
            origin = new URL(url).origin;
        } catch {
            return;
        }
        if (!origins.has(origin)) return;
        const method = request.method();
        if (method === 'OPTIONS') return;
        const got = namedInRequest({ provider, method, url, body: request.postData() });
        if (got.names.length + got.nodes.length === 0) return;
        seen.push({ ...got, what: `${method} ${url}` });
    });
    return {
        mark: () => seen.length,
        async reads(from = 0) {
            const slice = seen.slice(from);
            const needsTree = slice.some((s) => s.nodes.length || s.names.some((n) => n.folder?.ids));
            const lookup = needsTree ? await nodeTree(rig) : () => null;
            const out = [];
            for (const s of slice) {
                for (const n of s.names) {
                    const kind = KIND.get(n.name);
                    if (!kind) continue;
                    if (n.folder && typeof n.folder === 'object') {
                        // Drive: one list may ask several copies of a folder at once.
                        const folders = new Set(n.folder.ids.map((id) => lookup(id)?.name ?? `<node ${id}>`));
                        for (const f of folders) out.push({ file: kind, folder: f === LIBRARY_FOLDER ? null : f, what: s.what });
                    } else {
                        out.push({ file: kind, folder: n.folder, what: s.what });
                    }
                }
                for (const id of s.nodes) {
                    const node = lookup(id);
                    const kind = node && KIND.get(node.name);
                    if (kind) out.push({ file: kind, folder: folderOf(node.parent), what: s.what });
                }
            }
            return out;
        },
    };
}

/** The series folders a list of reads touched (series files only), sorted. */
export function seriesFolders(reads) {
    return [...new Set(reads.filter((r) => r.file === 'series').map((r) => r.folder))].sort();
}
