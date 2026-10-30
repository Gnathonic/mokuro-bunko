// What the shared specs share: the reader signed in to the cloud Bunko holds (as the
// direct suite signs in, cloud-direct.mjs), Bunko's lease marker as the reader judges
// it, what the cloud stores of a file, and every file the reader itself wrote to the
// cloud. Bunko writes to the same folder all along, so "the reader wrote X" is read off
// the browser's own requests, never off the cloud's files.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { expect } from '../fixtures.mjs';
import { LIBRARY_FOLDER, cloudFile, signIn } from '../cloud-direct.mjs';

export async function connect(reader, rig) {
    const { sync } = await signIn(reader, rig);
    expect(sync).toEqual({ ok: true, errors: [] });
}

/**
 * The tab hidden and shown again, as switching away and back does: the reader's cloud
 * queue poller takes one look then (its marker read included), where it would
 * otherwise look every 60 s (30 s on WebDAV).
 */
export async function tabAwayAndBack(page) {
    await page.evaluate(() => {
        const set = (hidden) => {
            Object.defineProperty(document, 'visibilityState', {
                configurable: true,
                get: () => (hidden ? 'hidden' : 'visible'),
            });
            Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
            document.dispatchEvent(new Event('visibilitychange'));
        };
        set(true);
        set(false);
    });
}

/**
 * Wait until the reader finds the Bunko marker fresh, then until it has SEEN the marker
 * move (a heartbeat after its first look): before that a reader judges it by the
 * marker's own clock (ttl + 5 min), after it by what it observed (2 x ttl). The holder.
 */
export async function bunkoSeenAlive(reader) {
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                return reader.controlState();
            },
            { timeout: 120_000, intervals: [1000, 2000] },
        )
        .toMatchObject({ serverCompilesMetadata: true, marker: { fresh: true } });
    const first = (await reader.controlState()).marker.holder.heartbeatAt;
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                return (await reader.controlState()).marker?.holder?.heartbeatAt ?? null;
            },
            { timeout: 120_000, intervals: [1000, 2000] },
        )
        .not.toBe(first);
    return (await reader.controlState()).marker.holder;
}

/** A file's bytes under the library folder as the cloud stores them, or null. */
export async function cloudBytes(rig, rel) {
    if (rig.provider === 'rclone') {
        try {
            return readFileSync(path.join(rig.workdir, 'rclone-root', LIBRARY_FOLDER, rel));
        } catch {
            return null;
        }
    }
    const f = await cloudFile(rig, rel);
    if (!f) return null;
    const r = await fetch(
        `${rig.cloudsimUrl}/_sim/content?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&id=${encodeURIComponent(f.id)}`,
    );
    if (r.status === 404) return null;
    if (!r.ok) throw new Error(`cloudsim content of ${rel}: ${r.status} ${await r.text()}`);
    return Buffer.from(await r.arrayBuffer());
}

/** A JSON file under the library folder, parsed, or null. */
export async function cloudJson(rig, rel) {
    const bytes = await cloudBytes(rig, rel);
    return bytes ? JSON.parse(bytes.toString('utf8').replace(/^﻿/, '')) : null;
}

/** Every node the cloud has had, trashed ones included: id → name. */
async function nodeNames(rig) {
    const rows = await rig.sim(
        'GET',
        `tree?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&include_trashed=1`,
    );
    return new Map(rows.map((r) => [r.id, r.name]));
}

const READS = new Set(['GET', 'HEAD', 'OPTIONS', 'PROPFIND']);

/**
 * The names of every file the reader (the page and its workers) wrote to the cloud
 * since this was called: `await writes.names()`. Read off its own requests: the name a
 * write carries (WebDAV's path or MOVE destination, Graph's `<name>:/content` and
 * upload sessions, Drive's metadata), and the node a write answered or addressed
 * (Drive and Graph ids, MEGA's new node handles: its names are encrypted on the wire)
 * looked up in cloudsim's tree. `writes.unnamed()` lists the writes it could put no
 * name to (no name in the request, no node in the answer, or a node gone from the
 * tree), for a failure message.
 */
export function recordReaderWrites(page, rig) {
    const origins = new Set(
        [rig.cloudsimUrl, rig.preauthUrl, rig.rcloneUrl].filter(Boolean).map((u) => new URL(u).origin),
    );
    const names = new Set();
    const ids = new Set();
    const pending = [];
    const unnamed = [];
    const base = (p) => decodeURIComponent(p.split('/').filter(Boolean).pop() ?? '');
    async function look(request) {
        const url = new URL(request.url());
        const method = request.method();
        if (!origins.has(url.origin) || READS.has(method)) return;
        const pathname = decodeURIComponent(url.pathname);
        // What this write named: a name, or a node to look up.
        let found = 0;
        const named = (n) => (found++, names.add(n));
        const noded = (id) => (found++, ids.add(id));
        try {
            await name(request, url, method, pathname, named, noded);
        } finally {
            if (!found) unnamed.push(`${method} ${pathname}`);
        }
    }
    async function name(request, url, method, pathname, named, noded) {
        if (rig.provider === 'rclone') {
            if (method === 'PUT') named(base(url.pathname));
            const dest = (await request.allHeaders()).destination;
            if (dest && (method === 'MOVE' || method === 'COPY')) named(base(new URL(dest).pathname));
            return;
        }
        if (rig.provider === 'microsoft') {
            const m = pathname.match(/\/([^/]+):\/(content|createUploadSession)$/);
            if (m) named(m[1]);
        }
        if (rig.provider === 'google') {
            const body = request.postDataBuffer()?.toString('latin1') ?? '';
            for (const m of body.matchAll(/"name"\s*:\s*"([^"]+)"/g)) named(JSON.parse(`"${m[1]}"`));
            const patched = pathname.match(/\/files\/([^/]+)$/);
            if (patched && method === 'PATCH') noded(patched[1]);
        }
        const response = await request.response();
        if (!response) return;
        let doc = null;
        try {
            doc = JSON.parse((await response.body()).toString('utf8'));
        } catch {
            return;
        }
        if (rig.provider === 'mega') {
            // A batch of commands answers one result each; `p` (put nodes) answers the
            // nodes it made, `{f: [{h, …}]}` (a fetch answers every node the same way).
            let cmds = [];
            try {
                cmds = JSON.parse(request.postData() ?? '[]');
            } catch {
                return;
            }
            if (!Array.isArray(cmds) || !Array.isArray(doc)) return;
            cmds.forEach((c, i) => {
                if (c?.a !== 'p') return;
                for (const n of doc[i]?.f ?? []) if (typeof n?.h === 'string') noded(n.h);
            });
        } else if (typeof doc?.id === 'string') {
            noded(doc.id);
        }
    }
    page.on('requestfinished', (request) => pending.push(look(request).catch(() => {})));
    let gone = [];
    return {
        async names() {
            await Promise.all(pending);
            const out = new Set(names);
            if (ids.size) {
                const byId = await nodeNames(rig);
                for (const id of ids) if (byId.has(id)) out.add(byId.get(id));
                gone = [...ids].filter((id) => !byId.has(id)).map((id) => `node ${id}, gone from the tree`);
            }
            return [...out].sort();
        },
        /** After `names()`: the writes it could put no name to. */
        unnamed: () => [...unnamed, ...gone],
    };
}

/** The volume of the seeded series, or of `series`, titled `title`, as the reader lists it. */
export async function volumeNamed(reader, series, title) {
    const { volumes } = await reader.library();
    const v = volumes.find((x) => x.series === series && x.title === title);
    expect(v, `${series}/${title} in the library`).toBeTruthy();
    return v;
}
