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

export function bunko(rig) {
    const base = rig.bunkoUrl;
    if (!base) throw new Error('this suite runs no Bunko server');
    const admin = rig.account('admin');

    async function request(method, path, { auth, body, headers = {} } = {}) {
        const h = { ...headers };
        if (auth) h.authorization = auth;
        let payload = body;
        if (body !== undefined && typeof body !== 'string' && !(body instanceof Uint8Array)) {
            h['content-type'] = 'application/json';
            payload = JSON.stringify(body);
        }
        return fetch(`${base}${path}`, { method, headers: h, body: payload });
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
        /** A file's bytes as `auth` reads them (status, text); 404 is an answer, not an error. */
        async get(path, auth) {
            const r = await request('GET', path, { auth });
            return { status: r.status, text: r.status === 200 ? await r.text() : null };
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
    // the headers the page set, which carry its Authorization.
    const cancelled = new WeakMap();
    page.on('requestfailed', (req) => cancelled.get(req)?.());
    page.on('request', (req) => {
        const url = new URL(req.url());
        if (url.origin !== origin) return;
        const failed = new Promise((resolve) => cancelled.set(req, resolve));
        pending.push(
            Promise.race([req.allHeaders(), failed.then(() => req.headers())])
                .catch(() => req.headers())
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
