// S7 (B8.6, plan X9): Allow on first connect. A reader that connects to Bunko for the
// first time (a fresh browser: nothing remembered of this server) as an editor, an
// uploader, a registered account (progress only), a processor or anonymously judges
// the account read-only exactly when `/login/api/me` says it may write nothing, and
// OPTIONS /mokuro-reader/ lists PUT and MKCOL exactly for the accounts that may write,
// as the browser reads it (CORS-exposed) and as any other client does.
//
// The anonymous role never writes progress on Bunko (bunko-core `has_perm`): there is
// no server whose anonymous account may, so anonymous is the read-only case here.
//
// A reader that cannot reach the identity endpoint judges the server by its OPTIONS
// answer instead: one that names Bunko (`X-Mokuro-Put: verified`) is taken as writable
// whatever Allow lists (R3.1), so a writer is never shut out by a stale Allow.
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, recordRequests } from '../bunko.mjs';

test.use({ preflightsReachServers: true });

const PASSWORD = 'rig-allow-1';
const ROOT = '/mokuro-reader/';

/**
 * Each role, and whether it may write anything (progress or files) under the reader's
 * root. Pinned here as well as read off `/me`, so a `/me` that changed its mind cannot
 * carry the Allow answer along with it.
 */
const ROLES = [
    { role: 'editor', writes: true },
    { role: 'uploader', writes: true },
    { role: 'registered', writes: true },
    { role: 'processor', writes: false },
    { role: 'anonymous', writes: false },
];

/** A fresh account of `role` (one per spec, so no sign-in shares a limiter key), or null for anonymous. */
async function accountFor(rig, role) {
    if (role === 'anonymous') return null;
    return bunko(rig).createUser(`allow-${role}-${Date.now().toString(36)}`, PASSWORD, role);
}

/** The methods of an Allow header, upper-cased; null when there is none. */
function methods(allow) {
    return allow === null
        ? null
        : allow
              .split(',')
              .map((m) => m.trim().toUpperCase())
              .filter(Boolean);
}

/** OPTIONS /mokuro-reader/ from the page, as the reader would send it: its status and the Allow it can read. */
function optionsInPage(page, url, authorization) {
    return page.evaluate(
        async ([u, auth]) => {
            const r = await fetch(u, { method: 'OPTIONS', headers: auth ? { Authorization: auth } : {} });
            return { status: r.status, allow: r.headers.get('Allow'), put: r.headers.get('X-Mokuro-Put') };
        },
        [url, authorization],
    );
}

for (const { role, writes } of ROLES) {
    test(`first connect as ${role}: read-only matches /me, Allow matches what it may write`, async ({
        reader,
        rig,
        page,
    }) => {
        const server = bunko(rig);
        const account = await accountFor(rig, role);
        const auth = account ? basic(account.username, account.password) : undefined;

        const me = await server.me(auth);
        expect(me.status).toBe(200);
        expect(me.body.role).toBe(role);
        const { canWriteProgress, canAddFiles } = me.body.permissions;
        expect(canWriteProgress || canAddFiles, `/me: may ${role} write?`).toBe(writes);

        // A fresh browser context: this is the reader's first look at this server.
        expect(await page.evaluate(() => Object.keys(localStorage).filter((k) => /webdav/i.test(k)))).toEqual([]);
        const requests = recordRequests(page, new URL(rig.bunkoUrl).origin);
        const { status, sync } = await reader.connectWebDAV({
            url: rig.bunkoUrl,
            ...(account ? { username: account.username, password: account.password } : {}),
        });
        expect(status).toMatchObject({ isAuthenticated: true, needsAttention: false, isReadOnly: !writes });
        if (writes) expect(sync).toEqual({ ok: true, errors: [] });
        // A read-only account's progress sync still tries its upload and is refused
        // (the reader hides its Sync button then, and the cloud screen swallows the
        // post-login one): not what this scenario is about, so only noted.
        else test.info().annotations.push({ type: 'read-only-sync', description: JSON.stringify(sync) });
        expect((await reader.providerStatus()).isReadOnly).toBe(!writes);

        // The credential the reader itself sends (a bearer token for an account,
        // nothing anonymously): Allow is answered for whoever that header names.
        const sent = (await requests.take()).filter((r) => r.path.startsWith(ROOT));
        expect(sent.length, 'the reader asked the server for its library').toBeGreaterThan(0);
        const readerAuth = sent.at(-1).authorization;
        if (account) expect(readerAuth).toMatch(/^(Bearer|Basic) /);
        else expect(readerAuth).toBeNull();

        const url = `${rig.bunkoUrl}${ROOT}`;
        const inPage = await optionsInPage(page, url, readerAuth);
        expect(inPage.status).toBe(200);
        expect(inPage.put).toBe('verified');
        const seen = methods(inPage.allow);
        expect(seen, 'Allow, as the reader can read it').not.toBeNull();
        expect(seen.includes('PUT'), `Allow lists PUT for ${role}: ${inPage.allow}`).toBe(writes);
        expect(seen.includes('MKCOL'), `Allow lists MKCOL for ${role}: ${inPage.allow}`).toBe(writes);

        // Any other client, under the account's password, gets the same answer.
        const direct = await server.request('OPTIONS', ROOT, { auth });
        expect(direct.status).toBe(200);
        expect(direct.headers.get('allow')).toBe(inPage.allow);
    });
}

test.describe('the identity endpoint out of reach', () => {
    // A route is how the endpoint is taken away; Playwright answers preflights itself
    // while any route is on, so these run routed (fixtures.mjs).
    test.use({ preflightsReachServers: false });

    for (const role of ['uploader', 'registered']) {
        test(`first connect as ${role}: the OPTIONS answer names Bunko, so it is writable`, async ({
            reader,
            rig,
            page,
        }) => {
            const account = await accountFor(rig, role);
            const blocked = [];
            await page.route('**/login/api/me', (route) => {
                blocked.push(route.request().url());
                return route.abort('connectionrefused');
            });
            const { status, sync } = await reader.connectWebDAV({
                url: rig.bunkoUrl,
                username: account.username,
                password: account.password,
            });
            expect(blocked.length, 'the reader asked for its identity').toBeGreaterThan(0);
            expect(status).toMatchObject({ isAuthenticated: true, needsAttention: false, isReadOnly: false });
            expect(sync).toEqual({ ok: true, errors: [] });
            expect((await reader.providerStatus()).isReadOnly).toBe(false);
        });
    }
});
