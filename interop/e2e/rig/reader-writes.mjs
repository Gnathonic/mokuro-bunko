// Every file the reader itself wrote to the cloud, read off the browser's own requests
// (shared/shared.mjs re-exports it for the shared specs). Kept apart so the naming is
// tested without a browser (reader-writes.test.mjs).

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
 * looked up in cloudsim's tree. An upload session's content goes to a URL that names
 * nothing (Drive's `upload_id`, Graph's `uploadUrl`), and its answer may be gone (a
 * Worker that ended before the body was asked for), so it takes what the request
 * that opened the session named. `writes.unnamed()` lists the writes it could put no
 * name to (no name in the request, no node in the answer, no session seen opening, or
 * a node gone from the tree), for a failure message.
 */
export function recordReaderWrites(page, rig) {
    const origins = new Set(
        [rig.cloudsimUrl, rig.preauthUrl, rig.rcloneUrl].filter(Boolean).map((u) => new URL(u).origin),
    );
    const names = new Set();
    const ids = new Set();
    const pending = [];
    const unnamed = [];
    // Upload session → what its opening request named; the session's content writes.
    const opened = new Map();
    const sessionWrites = [];
    const base = (p) => decodeURIComponent(p.split('/').filter(Boolean).pop() ?? '');
    const add = (got) => {
        for (const n of got.names) names.add(n);
        for (const id of got.ids) ids.add(id);
    };
    async function look(request) {
        const url = new URL(request.url());
        const method = request.method();
        if (!origins.has(url.origin) || READS.has(method)) return;
        // No answer at all (refused, or cancelled before one): it wrote nothing.
        const response = await request.response();
        if (!response) return;
        const pathname = decodeURIComponent(url.pathname);
        const what = `${method} ${pathname}`;
        // What this write named: names, and nodes to look up.
        const got = { names: [], ids: [] };
        const named = (n) => got.names.push(n);
        const noded = (id) => got.ids.push(id);
        let session = {};
        try {
            session = await name(request, response, url, method, pathname, named, noded);
        } finally {
            const found = got.names.length + got.ids.length > 0;
            if (session.opens) {
                // An opening writes nothing yet: its names go with the session's content.
                opened.set(session.opens, got);
            } else {
                add(got);
                if (session.writes) sessionWrites.push({ key: session.writes, what, found });
                else if (!found) unnamed.push(what);
            }
        }
    }
    /** Name one write; answers `{opens}` or `{writes}` with its upload session's key. */
    async function name(request, response, url, method, pathname, named, noded) {
        if (rig.provider === 'rclone') {
            if (method === 'PUT') named(base(url.pathname));
            const dest = (await request.allHeaders()).destination;
            if (dest && (method === 'MOVE' || method === 'COPY')) named(base(new URL(dest).pathname));
            return {};
        }
        let session = {};
        if (rig.provider === 'microsoft') {
            const m = pathname.match(/\/([^/]+):\/(content|createUploadSession)$/);
            if (m) named(m[1]);
            // A fragment PUT to an `uploadUrl`; any other PUT is no session's, and
            // stays named by what it answers.
            if (method === 'PUT') session = { writes: `${url.origin}${url.pathname}` };
        }
        if (rig.provider === 'google') {
            const body = request.postDataBuffer()?.toString('latin1') ?? '';
            for (const m of body.matchAll(/"name"\s*:\s*"([^"]+)"/g)) named(JSON.parse(`"${m[1]}"`));
            const patched = pathname.match(/\/files\/([^/]+)$/);
            if (patched && method === 'PATCH') noded(patched[1]);
            if (url.searchParams.get('uploadType') === 'resumable') {
                const id = url.searchParams.get('upload_id');
                if (id) {
                    session = { writes: id };
                } else {
                    // The init: its session is the `upload_id` of the Location it answers.
                    const location = await response.headerValue('location');
                    const opens = location && new URL(location, url).searchParams.get('upload_id');
                    if (opens) return { opens };
                }
            }
        }
        let doc = null;
        try {
            doc = JSON.parse((await response.body()).toString('utf8'));
        } catch {
            return session;
        }
        if (rig.provider === 'mega') {
            // A batch of commands answers one result each; `p` (put nodes) answers the
            // nodes it made, `{f: [{h, …}]}` (a fetch answers every node the same way).
            let cmds = [];
            try {
                cmds = JSON.parse(request.postData() ?? '[]');
            } catch {
                return session;
            }
            if (!Array.isArray(cmds) || !Array.isArray(doc)) return session;
            cmds.forEach((c, i) => {
                if (c?.a !== 'p') return;
                for (const n of doc[i]?.f ?? []) if (typeof n?.h === 'string') noded(n.h);
            });
        } else if (rig.provider === 'microsoft' && typeof doc?.uploadUrl === 'string') {
            // createUploadSession: its fragments take its name. With the answer gone,
            // the name stays this request's own (the path above).
            const u = new URL(doc.uploadUrl, url);
            return { opens: `${u.origin}${u.pathname}` };
        } else if (typeof doc?.id === 'string') {
            noded(doc.id);
        }
        return session;
    }
    // A Worker that reads only the headers (Drive's init: its Location) leaves the body
    // unread, and Chromium reports such a request failed though it was answered.
    for (const event of ['requestfinished', 'requestfailed']) {
        page.on(event, (request) => pending.push(look(request).catch(() => {})));
    }
    let late = [];
    return {
        async names() {
            await Promise.all(pending);
            late = [];
            const out = new Set(names);
            const all = new Set(ids);
            for (const w of sessionWrites) {
                // An opening that named nothing names none of its content either.
                const got = opened.get(w.key);
                if (got && got.names.length + got.ids.length > 0) {
                    for (const n of got.names) out.add(n);
                    for (const id of got.ids) all.add(id);
                } else if (!w.found) {
                    late.push(w.what);
                }
            }
            if (all.size) {
                const byId = await nodeNames(rig);
                for (const id of all) if (byId.has(id)) out.add(byId.get(id));
                late.push(...[...all].filter((id) => !byId.has(id)).map((id) => `node ${id}, gone from the tree`));
            }
            return [...out].sort();
        },
        /** After `names()`: the writes it could put no name to. */
        unnamed: () => [...unnamed, ...late],
    };
}
