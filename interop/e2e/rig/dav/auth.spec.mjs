// dav (§7.1): how the reader signs in to Bunko's WebDAV and keeps its sign-in. A Basic
// check of the account's password (UTF-8 on the wire), traded once for a bearer token
// that every later request carries; the token reused across a reload, renewed when the
// server signed it out or it nears its expiry, and signed out at logout. Each spec signs
// in as an account of its own, so one spec's failed or revoked sign-ins never reach
// another's.
import { test, expect } from '../fixtures.mjs';
import { bearerOf, bunko, fromBasic, recordRequests } from '../bunko.mjs';

// The browser's own preflights reach Bunko (fixtures.mjs): its CORS is part of the check.
test.use({ preflightsReachServers: true });

/** Not ASCII, not Latin-1 either: a Basic encoder that is not UTF-8 mangles it. */
const UTF8_PASSWORD = 'pässwörd-日本語-✓';

let serial = 0;
async function newAccount(rig, testInfo) {
    const name = `dav-${testInfo.workerIndex}-${Date.now().toString(36)}-${serial++}`.slice(0, 32);
    return bunko(rig).createUser(name, UTF8_PASSWORD);
}

async function connect(reader, rig, account) {
    const { sync } = await reader.connectWebDAV({
        url: rig.bunkoUrl,
        username: account.username,
        password: account.password,
    });
    expect(sync).toEqual({ ok: true, errors: [] });
}

/** The one bearer token in `requests`; fails on none, or on two. */
function onlyToken(requests) {
    const tokens = [...new Set(requests.map((r) => bearerOf(r.authorization)).filter(Boolean))];
    const sent = requests.map((r) => `${r.method} ${r.path} ${r.authorization?.slice(0, 16)}`).join('\n');
    expect(tokens, `the bearer tokens the reader sent:\n${sent}`).toHaveLength(1);
    return tokens[0];
}

/**
 * After a reload, the reader once it restored its session from storage. The status
 * reads authenticated before the restore has made the provider current (a sync then
 * answers "Not connected"), so the restore is done when a sync goes through.
 */
async function reloadConnected(reader) {
    await reader.page.reload();
    await reader.page.waitForFunction(() => globalThis.__mokuroInterop?.apiVersion === 1);
    await expect
        .poll(async () => (await reader.sync()).errors, { timeout: 30_000, intervals: [100, 250, 500] })
        .toEqual([]);
}

test('Basic with a UTF-8 password at connect, then the bearer token on every request', async ({
    reader,
    rig,
}, testInfo) => {
    const server = bunko(rig);
    const account = await newAccount(rig, testInfo);
    const requests = recordRequests(reader.page, server.url);
    await connect(reader, rig, account);
    const sent = await requests.take();

    // Before the token: Basic, the password exactly as typed.
    const issued = sent.findIndex((r) => r.method === 'POST' && r.path === '/login/api/token');
    expect(issued, 'the reader asked for a token').toBeGreaterThan(0);
    const before = sent.slice(0, issued);
    expect(before.some((r) => r.authorization?.startsWith('Basic '))).toBe(true);
    for (const r of before.filter((x) => x.authorization)) {
        expect(fromBasic(r.authorization), `${r.method} ${r.path}`).toBe(`${account.username}:${UTF8_PASSWORD}`);
    }
    // After it: the token, and never the password again.
    const after = sent.slice(issued + 1);
    expect(after.length).toBeGreaterThan(0);
    for (const r of after) {
        expect(r.authorization?.startsWith('Bearer '), `${r.method} ${r.path} carries the token`).toBe(true);
    }
    const token = onlyToken(after);
    expect((await server.me(`Bearer ${token}`)).body).toMatchObject({
        authenticated: true,
        username: account.username,
    });

    // And the session works: the library is listed.
    const { volumes } = await reader.library();
    expect(
        volumes
            .filter((v) => v.series === rig.library.series)
            .map((v) => v.title)
            .sort(),
    ).toEqual([...rig.library.volumes].sort());
});

test('a reload resumes on the bearer token alone', async ({ reader, rig }, testInfo) => {
    const server = bunko(rig);
    const account = await newAccount(rig, testInfo);
    const requests = recordRequests(reader.page, server.url);
    await connect(reader, rig, account);
    const token = onlyToken(await requests.take());

    await reloadConnected(reader);
    const sent = await requests.take();
    expect(sent.length).toBeGreaterThan(0);
    expect(sent.filter((r) => r.path === '/login/api/token')).toEqual([]);
    for (const r of sent) {
        expect(r.authorization, `${r.method} ${r.path}`).toBe(`Bearer ${token}`);
    }
});

test('a token the server signed out is replaced, and the request retried', async ({ reader, rig }, testInfo) => {
    const server = bunko(rig);
    const account = await newAccount(rig, testInfo);
    const requests = recordRequests(reader.page, server.url);
    await connect(reader, rig, account);
    const old = onlyToken(await requests.take());

    await server.revoke(old);
    expect((await server.me(`Bearer ${old}`)).status).toBe(401);
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
    const sent = await requests.take();
    const posts = sent.filter((r) => r.method === 'POST' && r.path === '/login/api/token');
    // One re-issue, however many requests were refused at once (single-flight).
    expect(posts, 'the reader asked for one new token').toHaveLength(1);
    const issued = sent.indexOf(posts[0]);
    expect(issued).toBeGreaterThan(0);
    // Requests already in flight still carry the old token; everything after uses the new.
    const fresh = bearerOf(sent.at(-1).authorization);
    expect(fresh).toBeTruthy();
    expect(fresh).not.toBe(old);
    for (const r of sent.slice(issued + 1)) {
        expect([`Bearer ${old}`, `Bearer ${fresh}`], `${r.method} ${r.path}`).toContain(r.authorization);
    }
    expect((await server.me(`Bearer ${fresh}`)).body).toMatchObject({ authenticated: true });
});

test('a token near its expiry is renewed at the next connect, and the old one signed out', async ({
    reader,
    rig,
}, testInfo) => {
    const server = bunko(rig);
    const account = await newAccount(rig, testInfo);
    const requests = recordRequests(reader.page, server.url);
    await connect(reader, rig, account);
    const old = onlyToken(await requests.take());

    // A reader token lives 90 days and is renewed in its last 7 (bunko-token.ts): stand
    // in for 84 days passing by moving the expiry the reader holds, not the clock.
    await reader.page.evaluate(() =>
        localStorage.setItem('webdav_token_expires_at', String(Date.now() + 24 * 3600 * 1000)),
    );
    await reloadConnected(reader);
    const sent = await requests.take();
    expect(sent.filter((r) => r.method === 'POST' && r.path === '/login/api/token')).toHaveLength(1);
    const fresh = bearerOf(sent.at(-1).authorization);
    expect(fresh).toBeTruthy();
    expect(fresh).not.toBe(old);
    expect((await server.me(`Bearer ${fresh}`)).body).toMatchObject({ authenticated: true });
    await expect.poll(async () => (await server.me(`Bearer ${old}`)).status, { timeout: 15_000 }).toBe(401);
});

test('logout signs the token out on the server', async ({ reader, rig }, testInfo) => {
    const server = bunko(rig);
    const account = await newAccount(rig, testInfo);
    const requests = recordRequests(reader.page, server.url);
    await connect(reader, rig, account);
    const token = onlyToken(await requests.take());
    expect((await server.me(`Bearer ${token}`)).status).toBe(200);

    await reader.logout();
    await expect.poll(async () => (await server.me(`Bearer ${token}`)).status, { timeout: 15_000 }).toBe(401);
    expect((await reader.providerStatus())?.isAuthenticated ?? false).toBe(false);
});

test("the browser's CORS preflights from the reader's origin pass", async ({ reader, rig, context }, testInfo) => {
    const server = bunko(rig);
    const account = await newAccount(rig, testInfo);
    const origin = new URL(rig.readerUrl).origin;
    const cdp = await context.newCDPSession(reader.page);
    const preflights = new Map();
    cdp.on('Network.requestWillBeSent', (e) => {
        if (e.initiator?.type === 'preflight' && e.request.url.startsWith(server.url)) {
            preflights.set(e.requestId, { url: e.request.url, headers: e.request.headers, response: null });
        }
    });
    cdp.on('Network.responseReceived', (e) => {
        const p = preflights.get(e.requestId);
        if (p) p.response = { status: e.response.status, headers: e.response.headers };
    });
    cdp.on('Network.loadingFailed', (e) => {
        const p = preflights.get(e.requestId);
        if (p) p.failed = e.errorText + (e.corsErrorStatus ? ` ${JSON.stringify(e.corsErrorStatus)}` : '');
    });
    await cdp.send('Network.enable');

    await connect(reader, rig, account);
    // The reader keeps working after connect: a preflight it has just sent has no answer
    // yet. Each one recorded is judged once it has its answer (or failed).
    const unsettled = () => [...preflights.values()].filter((p) => !p.response && !p.failed).map((p) => p.url);
    await expect.poll(unsettled, { message: 'preflights still waiting for an answer', timeout: 15_000 }).toEqual([]);
    const seen = [...preflights.values()];
    expect(seen.length, 'preflights the browser sent to Bunko').toBeGreaterThan(0);
    const lower = (h) => Object.fromEntries(Object.entries(h ?? {}).map(([k, v]) => [k.toLowerCase(), v]));
    const asked = new Set();
    for (const p of seen) {
        const req = lower(p.headers);
        const res = lower(p.response?.headers);
        expect(p.failed, p.url).toBeUndefined();
        expect(req.origin, p.url).toBe(origin);
        expect(p.response?.status, p.url).toBeGreaterThanOrEqual(200);
        expect(p.response?.status, p.url).toBeLessThan(300);
        expect(res['access-control-allow-origin'], p.url).toBe(origin);
        // Bunko's own answer, not one made up on the way (Playwright's has neither).
        expect(res['access-control-max-age'], p.url).toBeDefined();
        if (new URL(p.url).pathname.startsWith('/mokuro-reader')) expect(res['x-mokuro-put'], p.url).toBe('verified');
        const method = req['access-control-request-method'];
        asked.add(method);
        const methods = (res['access-control-allow-methods'] ?? '').split(/,\s*/).map((m) => m.toUpperCase());
        expect(methods, `${p.url} allows ${method}`).toContain(method);
        const wanted = (req['access-control-request-headers'] ?? '')
            .split(',')
            .map((x) => x.trim())
            .filter(Boolean);
        const allowed = (res['access-control-allow-headers'] ?? '').toLowerCase().split(/,\s*/);
        for (const h of wanted) expect(allowed, `${p.url} allows header ${h}`).toContain(h.toLowerCase());
    }
    // The WebDAV listing and the authenticated requests are what needs one.
    expect([...asked]).toContain('PROPFIND');
});
