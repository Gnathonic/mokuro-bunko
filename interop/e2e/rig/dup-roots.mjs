// S1 and S9 (B8.6, plan X5): duplicate `mokuro-reader` roots on a cloud that allows two
// folders of one name (Drive, MEGA), made the way another client makes them
// (`/_sim/node`, which can date a node in the past). The seed: the library's own root
// (A, made when the suite started), an OLDER top-level duplicate (B), and an older still
// root nested in another folder (`Backup/mokuro-reader`). Both mergers (the reader's
// #308 and Bunko's B5.6) group same-named folders under one parent, keep the oldest of
// a group, and never merge roots with different parents: B is the canonical root, A
// merges into it and goes to the trash once empty, and the nested root is left as it
// is and still read. Two same-named files keep the newer write (the other trashed),
// except the files merged across copies (`volume-data.json`) and Bunko's control
// files, which keep every copy.
import { createHash } from 'node:crypto';
import { LIBRARY_FOLDER } from './cloud-direct.mjs';
import { cloudBytes } from './shared/shared.mjs';

export const CONTROL_FILES = ['.mokuro-bunko.json', '.mokuro-queue.json'];
const VOLUME_DATA = 'volume-data.json';

/** OneDrive refuses a second name (409) and rclone's WebDAV has paths: nothing to merge. */
export function skipWithoutDuplicates(test, rig) {
    test.skip(!['google', 'mega'].includes(rig.provider), `${rig.provider} holds no two folders of one name`);
}

/** Every node cloudsim has had, trashed ones included. */
export const tree = (rig) =>
    rig.sim('GET', `tree?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&include_trashed=1`);

/** `/_sim/node` for the suite's account; answers the new node's id. */
async function node(rig, body) {
    const { id } = await rig.sim('POST', 'node', { provider: rig.provider, account: rig.simAccount, ...body });
    return id;
}

/** The live top-level `mokuro-reader` folders (a top-level node's parent is no listed node). */
export function topRoots(rows) {
    const ids = new Set(rows.map((r) => r.id));
    return rows.filter((r) => r.folder && !r.trashed && r.name === LIBRARY_FOLDER && !ids.has(r.parent_id));
}

/** Whether `id` is `ancestor` or lies below it. */
export function under(rows, id, ancestor) {
    const byId = new Map(rows.map((r) => [r.id, r]));
    for (let n = byId.get(id), i = 0; n && i < 64; n = byId.get(n.parent_id), i++) {
        if (n.id === ancestor) return true;
    }
    return false;
}

/** The path of `id` below the folder `root` (`S1 Dup/Vol 1.cbz`), or null when not below it. */
export function pathBelow(rows, id, root) {
    const byId = new Map(rows.map((r) => [r.id, r]));
    const parts = [];
    for (let n = byId.get(id), i = 0; n && i < 64; n = byId.get(n.parent_id), i++) {
        if (n.id === root) return parts.join('/');
        parts.unshift(n.name);
    }
    return null;
}

/** The live files below the folder `root`, by path below it: `{path → row}`. */
export function filesBelow(rows, root) {
    const out = new Map();
    for (const r of rows) {
        if (r.folder || r.trashed) continue;
        const p = pathBelow(rows, r.id, root);
        if (p !== null) out.set(p, r);
    }
    return out;
}

/** A node's own bytes as the cloud stores them. */
export async function nodeBytes(rig, id) {
    const r = await fetch(
        `${rig.cloudsimUrl}/_sim/content?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&id=${encodeURIComponent(id)}`,
    );
    if (!r.ok) throw new Error(`cloudsim content of ${id}: ${r.status} ${await r.text()}`);
    return Buffer.from(await r.arrayBuffer());
}

/**
 * What the seeds are made of, read off the library the rig put in the cloud: its OCRed
 * volume's archive (each seed re-tagged, so its bytes are its own) and sidecar.
 */
export async function seedMaterial(rig) {
    const series = rig.library.series;
    const cbz = await cloudBytes(rig, `${series}/Vol 1.cbz`);
    const mokuro = await cloudBytes(rig, `${series}/Vol 1.mokuro`);
    if (!cbz || !mokuro) throw new Error(`the library's ${series}/Vol 1 is not in the cloud to seed from`);
    return { cbz, doc: JSON.parse(mokuro.toString('utf8')) };
}

/** `cbz` with its zip comment set to `tag`: the same pages, bytes of its own. */
export function retag(cbz, tag) {
    // The end-of-central-directory record: signature, 16 bytes, the comment's length.
    for (let i = cbz.length - 22; i >= 0; i--) {
        if (cbz.readUInt32LE(i) !== 0x06054b50) continue;
        const comment = Buffer.from(tag, 'utf8');
        const head = Buffer.from(cbz.subarray(0, i + 22));
        head.writeUInt16LE(comment.length, i + 20);
        return Buffer.concat([head, comment]);
    }
    throw new Error('not a zip: no end-of-central-directory record');
}

/** `volume_uuid` of seed number `n` (genlib's form; the rig's own volumes are 0 and 1). */
export const volumeId = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;

/** The OCR sidecar of `series`/`volume`, each page reading `line` (genlib's form). */
export function mokuroDoc(material, series, volume, n, line) {
    const hex = createHash('sha256').update(series).digest('hex').slice(0, 12);
    return Buffer.from(
        JSON.stringify({
            ...material.doc,
            title: series,
            title_uuid: `00000000-0000-4000-9000-${hex}`,
            volume,
            volume_uuid: volumeId(n),
            pages: material.doc.pages.map((p) => ({ ...p, blocks: [{ ...p.blocks[0], lines: [line] }] })),
        }),
    );
}

/** `volume-data.json` holding one volume's progress (`page`), written at `at`. */
export function volumeData(series, volume, n, page, at) {
    return Buffer.from(
        JSON.stringify({
            [volumeId(n)]: { progress: page, lastProgressUpdate: at, series_title: series, volume_title: volume },
        }),
    );
}

/**
 * Seed `files` (`{path: bytes}`, paths below `parent`, folders made on the way) with
 * every node dated `at` (RFC 3339; null: now). Answers `{path → id}`, folders included.
 */
export async function seedFiles(rig, parent, files, at) {
    const times = at ? { created: at, modified: at } : {};
    const ids = new Map();
    for (const [rel, bytes] of Object.entries(files)) {
        const parts = rel.split('/');
        let at_ = parent;
        for (let i = 0; i < parts.length - 1; i++) {
            const dir = parts.slice(0, i + 1).join('/');
            if (!ids.has(dir))
                ids.set(dir, await node(rig, { parent_id: at_, name: parts[i], folder: true, ...times }));
            at_ = ids.get(dir);
        }
        ids.set(
            rel,
            await node(rig, { parent_id: at_, name: parts.at(-1), content_base64: bytes.toString('base64'), ...times }),
        );
    }
    return ids;
}

/** A new top-level folder `name` dated `at` (null: now): its id. */
export function topFolder(rig, name, at) {
    const times = at ? { created: at, modified: at } : {};
    return node(rig, { name, folder: true, ...times });
}

/**
 * The X5 seed beside the library root A (`rootA`, its id): `S1 Dup/Vol 1` into A (its
 * sidecar the NEWER write); the older top-level root B with `S1 Dup/Vol 1.mokuro` (the
 * OLDER write), its own volume `S1 Dup/Vol 2` and its own `volume-data.json` (Vol 2 on
 * page 3); and `Backup/mokuro-reader` (older than both) with `S1 Nested/Vol 1`.
 * `extra` adds files below A and B (`{a: {path: bytes}, b: {...}}`). Answers the ids.
 */
export async function seedX5(rig, rootA, { extra = {} } = {}) {
    const m = await seedMaterial(rig);
    const now = new Date().toISOString();
    const a = await seedFiles(rig, rootA, {
        'S1 Dup/Vol 1.cbz': retag(m.cbz, 'S1 Dup/Vol 1'),
        'S1 Dup/Vol 1.mokuro': mokuroDoc(m, 'S1 Dup', 'Vol 1', 61, '新しい版'),
        ...(extra.a ?? {}),
    });
    const rootB = await topFolder(rig, LIBRARY_FOLDER, '2020-01-01T00:00:00Z');
    const b = await seedFiles(
        rig,
        rootB,
        {
            'S1 Dup/Vol 1.mokuro': mokuroDoc(m, 'S1 Dup', 'Vol 1', 61, '古い版'),
            'S1 Dup/Vol 2.cbz': retag(m.cbz, 'S1 Dup/Vol 2'),
            'S1 Dup/Vol 2.mokuro': mokuroDoc(m, 'S1 Dup', 'Vol 2', 62, '二巻'),
            [VOLUME_DATA]: volumeData('S1 Dup', 'Vol 2', 62, 3, '2021-01-01T00:00:00.000Z'),
            ...(extra.b ?? {}),
        },
        '2021-01-01T00:00:00Z',
    );
    const backup = await topFolder(rig, 'Backup', '2019-01-01T00:00:00Z');
    const nested = await node(rig, {
        parent_id: backup,
        name: LIBRARY_FOLDER,
        folder: true,
        created: '2019-01-01T00:00:00Z',
        modified: '2019-01-01T00:00:00Z',
    });
    const n = await seedFiles(
        rig,
        nested,
        {
            'S1 Nested/Vol 1.cbz': retag(m.cbz, 'S1 Nested/Vol 1'),
            'S1 Nested/Vol 1.mokuro': mokuroDoc(m, 'S1 Nested', 'Vol 1', 63, '入れ子'),
        },
        '2019-01-01T00:00:00Z',
    );
    return { material: m, rootA, rootB, a, b, backup, nested, n };
}

/** The nested root's live files, as `{path → "id:sha256"}`. */
export function nestedState(rows, seed) {
    return Object.fromEntries([...filesBelow(rows, seed.nested)].map(([p, r]) => [p, `${r.id}:${r.sha256}`]));
}

/** What a Bunko compiles beside a series wherever the series lives: its index and covers. */
const COMPILED = /\/(series\.json|[^/]+\.webp)$/;

/**
 * The nested root left as it was (`before`, nestedState's): every file in it the same
 * node with the same bytes, and nothing new but what a Bunko compiles for the series
 * that lives there. Answers what differs.
 */
export function nestedChanges(rows, seed, before) {
    const now = nestedState(rows, seed);
    const changed = Object.keys(before).filter((p) => now[p] !== before[p]);
    const added = Object.keys(now).filter((p) => !(p in before) && !COMPILED.test(p));
    return [...changed.map((p) => `changed ${p}`), ...added.map((p) => `added ${p}`)];
}

/**
 * Checks of the merged state, `rows` the tree including the trash: one live top-level
 * root, `canonical`; every merged-away `folders` (ids) sent to the trash itself and
 * holding nothing but what went to the trash on its own (nothing was trashed along with
 * it); no control file in the trash.
 * Answers what failed, so a poll can wait for `[]`.
 */
export function mergeProblems(rows, canonical, folders) {
    const problems = [];
    const tops = topRoots(rows).map((r) => r.id);
    if (tops.length !== 1 || tops[0] !== canonical) problems.push(`top-level roots ${tops} (want ${canonical})`);
    for (const f of folders) {
        const row = rows.find((r) => r.id === f);
        if (!row?.explicitly_trashed) problems.push(`${f} (${row?.name}) not sent to the trash`);
        // A copy trashed on its own stays where it was on Drive (the losing copy of a
        // same-named file, or a `volume-data.json` a reader's sync folded into
        // another): only a node trashed along with the folder was not moved out.
        const kids = rows
            .filter((r) => r.parent_id === f && !r.explicitly_trashed)
            .map((r) => `${r.name}${r.trashed ? ' (trashed)' : ''}`);
        if (kids.length) problems.push(`${f} (${row?.name}) still holds ${kids.join(', ')}`);
    }
    const trashedControl = rows.filter((r) => r.trashed && CONTROL_FILES.includes(r.name));
    if (trashedControl.length) problems.push(`control files in the trash: ${trashedControl.map((r) => r.id)}`);
    return problems;
}

/**
 * Every file written (or made) at or after `since` (ms) lies below `canonical`, or is
 * one of `allowed(row)`: answers the rows that do not. Trashed files count too: a write
 * into a merged-away root (its pre-merge id kept as a write root) lands in the trash
 * with it, and a merge's trashing moves no write time. `since` is taken to its whole
 * second, as MEGA stamps a node: a write in the same second still reads as after it.
 */
export function writesOutside(rows, canonical, since, allowed = () => false) {
    const from = Math.floor(since / 1000) * 1000;
    return rows
        .filter((r) => !r.folder && Date.parse(r.modified) >= from)
        .filter((r) => !under(rows, r.id, canonical) && !allowed(r))
        .map((r) => ({ id: r.id, name: r.name, parent_id: r.parent_id, modified: r.modified }));
}
