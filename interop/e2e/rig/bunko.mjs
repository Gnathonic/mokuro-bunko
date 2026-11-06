// The Bunko server as another client sees it: what a spec asks the server directly, from
// Node, beside what the reader does in the browser. Requests carry the account's own
// credentials, so they see exactly what that account may see.

/** `Basic …` for `username:password`, UTF-8 (RFC 7617 `charset="UTF-8"`). */
export function basic(username, password) {
    return `Basic ${Buffer.from(`${username}:${password}`, 'utf8').toString('base64')}`;
}

/** `username:password` out of a `Basic …` header, read as UTF-8; null for any other. */
export function fromBasic(header) {
    if (!header?.startsWith('Basic ')) return null;
    return Buffer.from(header.slice('Basic '.length), 'base64').toString('utf8');
}

/** The token a `Bearer …` header carries; null for any other. */
export function bearerOf(header) {
    return header?.startsWith('Bearer ') ? header.slice('Bearer '.length) : null;
}

/** A WebDAV path under the reader's root, each segment encoded. */
export function davPath(...segments) {
    return `/mokuro-reader/${segments.map(encodeURIComponent).join('/')}`;
}

/** How many lease-fence 503s `request` waits out before it hands one back. */
const FENCE_WAITS = 2;

export function bunko(rig) {
    const base = rig.bunkoUrl;
    if (!base) throw new Error('this suite runs no Bunko server');
    const admin = rig.account('admin');

    function send(method, path, { auth, body, headers = {} }) {
        const h = { ...headers };
        if (auth) h.authorization = auth;
        let payload = body;
        if (body !== undefined && typeof body !== 'string' && !(body instanceof Uint8Array)) {
            h['content-type'] = 'application/json';
            payload = JSON.stringify(body);
        }
        return fetch(`${base}${path}`, { method, headers: h, body: payload });
    }

    /**
     * One request, as a client sends it. Bunko's lease fence answers a write 503 with a
     * short Retry-After while it claims the library or confirms its claim (outbox.rs):
     * waited out as the reader does (`fetchWithRetryAfter`), at most twice, a
     * Retry-After of 30 s at most. Any other answer, and a third 503, come back as is.
     */
    async function request(method, path, options = {}) {
        for (let waits = 0; ; waits++) {
            const r = await send(method, path, options);
            const after = Number(r.headers.get('retry-after'));
            if (r.status !== 503 || waits >= FENCE_WAITS || !(after > 0 && after <= 30)) return r;
            await r.arrayBuffer();
            await new Promise((resolve) => setTimeout(resolve, after * 1000));
        }
    }

    return {
        url: base,
        request,
        /** A new account (admin API), so a spec's sign-ins never touch another's limiter key. */
        async createUser(username, password, role = 'registered') {
            const r = await request('POST', '/_admin/api/users', {
                auth: basic(admin.username, admin.password),
                body: { username, password, role },
            });
            if (!r.ok) throw new Error(`creating ${username}: ${r.status} ${await r.text()}`);
            return { username, password, role };
        },
        /** `/login/api/me` under `auth`: its status and body. */
        async me(auth) {
            const r = await request('GET', '/login/api/me', { auth });
            return { status: r.status, body: await r.json().catch(() => null) };
        },
        /** The account page's password change (it signs out every token of the account). */
        async changePassword(account, next) {
            const r = await request('POST', '/api/account/password', {
                auth: basic(account.username, account.password),
                body: { current_password: account.password, new_password: next },
            });
            if (!r.ok) throw new Error(`changing ${account.username}'s password: ${r.status} ${await r.text()}`);
            return { ...account, password: next };
        },
        /** Sign a token out, as the reader's logout does. */
        async revoke(token) {
            const r = await request('DELETE', '/login/api/token', { auth: `Bearer ${token}` });
            if (!r.ok) throw new Error(`revoking a token: ${r.status} ${await r.text()}`);
        },
        /** The admin Storage panel (`/_admin/api/storage`): the cloud library and its lease. */
        async storage() {
            const r = await request('GET', '/_admin/api/storage', { auth: basic(admin.username, admin.password) });
            if (!r.ok) throw new Error(`the storage panel: ${r.status} ${await r.text()}`);
            return r.json();
        },
        /**
         * The Storage panel's Disconnect (`/_admin/api/storage/disconnect`): the outbox
         * drained (`wait`, up to `wait_s`), `released` written into the lease marker, the
         * cloud's credentials removed; the server then restarts on no cloud. The report.
         */
        async disconnectStorage(body = { mode: 'wait', wait_s: 60 }) {
            const r = await request('POST', '/_admin/api/storage/disconnect', {
                auth: basic(admin.username, admin.password),
                body,
            });
            if (!r.ok) throw new Error(`disconnecting the library: ${r.status} ${await r.text()}`);
            return r.json();
        },
        /**
         * The Readers setting (`PUT /_admin/api/storage/readers`): whether readers are
         * served, and whose files the cloud root's own are (`progress_user`, or null).
         */
        async setReaders(serve_readers, progress_user) {
            const r = await request('PUT', '/_admin/api/storage/readers', {
                auth: basic(admin.username, admin.password),
                body: { serve_readers, progress_user },
            });
            if (!r.ok) throw new Error(`the Readers setting: ${r.status} ${await r.text()}`);
            return r.json();
        },
        /** A file's bytes as `auth` reads them (status, bytes); 404 is an answer, not an error. */
        async bytes(path, auth) {
            const r = await request('GET', path, { auth });
            return { status: r.status, bytes: r.status === 200 ? Buffer.from(await r.arrayBuffer()) : null };
        },
        /** A file's bytes as `auth` reads them (status, text); 404 is an answer, not an error. */
        async get(path, auth) {
            const r = await request('GET', path, { auth });
            return { status: r.status, text: r.status === 200 ? await r.text() : null };
        },
        /** `PUT path` under `auth`: its status (`body` a string, bytes, or JSON). */
        async put(path, body, auth) {
            const r = await request('PUT', path, { auth, body });
            await r.arrayBuffer();
            return r.status;
        },
        /** `MKCOL path` under `auth`: its status. */
        async mkcol(path, auth) {
            const r = await request('MKCOL', path, { auth });
            await r.arrayBuffer();
            return r.status;
        },
        /**
         * `PROPFIND path` at `depth` under `auth`: its status, and the paths of the
         * resources it lists (decoded, as the hrefs name them).
         */
        async propfind(path, auth, depth = '1') {
            const r = await request('PROPFIND', path, { auth, headers: { depth } });
            const text = await r.text();
            const hrefs = [...text.matchAll(/<(?:[\w-]+:)?href>([^<]*)<\/(?:[\w-]+:)?href>/g)].map((m) =>
                decodeURIComponent(new URL(m[1].replaceAll('&amp;', '&'), base).pathname),
            );
            return { status: r.status, hrefs };
        },
        /** The volume's manifest (`/catalog/api/manifest`) as `auth` reads it: status and body. */
        async manifest(series, volume, auth) {
            const q = `series=${encodeURIComponent(series)}&volume=${encodeURIComponent(volume)}`;
            const r = await request('GET', `/catalog/api/manifest?${q}`, { auth });
            return { status: r.status, body: await r.json().catch(() => null) };
        },
    };
}

/**
 * Every request the page (and its workers) sends to `origin`, in order. `take()` answers
 * those since the last `take()` as `{method, path, authorization}`, headers read.
 */
export function recordRequests(page, origin) {
    let pending = [];
    // A request a reload cancelled never gets its full headers (allHeaders() waits
    // forever), and one still in flight when the spec ends rejects: both are read from
    // the headers the page set, which carry its Authorization. A request the old
    // document still had in flight at a reload gets no requestfailed either (Chromium
    // drops it at the commit and Playwright reports nothing: the reader's cover
    // downloads right after a connect), so a main-frame navigation settles every
    // request recorded before it the same way.
    const cancelled = new WeakMap();
    const unsettled = new Set();
    page.on('requestfailed', (req) => cancelled.get(req)?.());
    page.on('framenavigated', (frame) => {
        if (frame !== page.mainFrame()) return;
        for (const settle of unsettled) settle();
        unsettled.clear();
    });
    page.on('request', (req) => {
        const url = new URL(req.url());
        if (url.origin !== origin) return;
        let settle;
        const failed = new Promise((resolve) => (settle = resolve));
        cancelled.set(req, settle);
        unsettled.add(settle);
        pending.push(
            Promise.race([req.allHeaders(), failed.then(() => req.headers())])
                .catch(() => req.headers())
                .finally(() => unsettled.delete(settle))
                .then((h) => ({
                    method: req.method(),
                    path: decodeURIComponent(url.pathname),
                    authorization: h.authorization ?? null,
                })),
        );
    });
    return {
        async take() {
            const taken = pending;
            pending = [];
            return Promise.all(taken);
        },
    };
}
