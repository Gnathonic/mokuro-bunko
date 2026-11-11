// api (§7.1): what the reader shows when Bunko answers 401, 429 or 503. At connect the
// cloud screen's WebDAV error dialog shows the refused login's ProviderError (its
// message, and its type for the guidance); a sign-in lost mid-session raises "Action
// Required" (provider status `needsAttention`) and, after a reload, the pre-filled
// sign-in form; a failed sync is reported, the session kept. Each spec signs in as an
// account of its own (the limiter counts failures per address and account).
import { test, expect } from '../fixtures.mjs';
import { bunko } from '../bunko.mjs';

test.use({ preflightsReachServers: true });

const PASSWORD = 'rig-banner-1';
let serial = 0;
const newAccount = (rig) => bunko(rig).createUser(`api-b-${Date.now().toString(36)}-${serial++}`, PASSWORD);

/** connectWebDAV's refusal, as the cloud screen hands it to the WebDAV error dialog. */
function connectRefusal(page, credentials) {
    return page.evaluate(async (o) => {
        try {
            await globalThis.__mokuroInterop.connectWebDAV(o);
            return null;
        } catch (e) {
            return { message: e.message, type: e.webdavErrorType ?? null, transient: e.isNetworkError ?? null };
        }
    }, credentials);
}

async function connect(reader, rig, account) {
    const { sync } = await reader.connectWebDAV({
        url: rig.bunkoUrl,
        username: account.username,
        password: account.password,
    });
    expect(sync).toEqual({ ok: true, errors: [] });
}

test('401: a wrong password is refused at connect, as a sign-in error', async ({ reader, rig }) => {
    const account = await newAccount(rig);
    const refusal = await connectRefusal(reader.page, {
        url: rig.bunkoUrl,
        username: account.username,
        password: 'not-the-password',
    });
    expect(refusal).toEqual({
        message: 'Authentication failed - check your credentials',
        type: 'auth',
        transient: false,
    });
    expect((await reader.providerStatus())?.isAuthenticated ?? false).toBe(false);
});

test('401: a password changed elsewhere asks for a new sign-in', async ({ reader, rig }) => {
    const server = bunko(rig);
    const account = await newAccount(rig);
    await connect(reader, rig, account);
    // The account page's change signs every token of the account out.
    await server.changePassword(account, 'rig-banner-2');

    expect(await reader.sync()).toEqual({ ok: false, errors: ['Authentication failed - please sign in again'] });
    // The cloud screen's "Action Required", the navbar's red cloud.
    expect(await reader.providerStatus()).toMatchObject({ needsAttention: true });
    // After a reload the stale password is gone: the pre-filled sign-in form, not a
    // silent anonymous session.
    await reader.page.reload();
    await reader.page.waitForFunction(() => globalThis.__mokuroInterop?.apiVersion === 1);
    await expect
        .poll(async () => reader.providerStatus(), { timeout: 30_000 })
        .toMatchObject({ isAuthenticated: false, needsAttention: true, hasStoredCredentials: true });
});

test('429: past the sign-in limiter, the right password is refused as "try again later"', async ({ reader, rig }) => {
    const server = bunko(rig);
    const account = await newAccount(rig);
    // Ten failures in five minutes block the address and account (http/limiter.rs).
    for (let i = 0; i < 10; i++) {
        const r = await server.request('POST', '/login/api/check', {
            body: { username: account.username, password: `wrong-${i}` },
        });
        expect(r.status).toBe(401);
    }
    const refusal = await connectRefusal(reader.page, {
        url: rig.bunkoUrl,
        username: account.username,
        password: account.password,
    });
    // Transient: a restore keeps the stored password for a later try (M-6).
    expect(refusal).toEqual({ message: 'Too many failed attempts - try again later', type: 'auth', transient: true });
});

// A Bunko on its own disk has no 503 a spec can call up: its 503s are the cloud
// storage's waits (the lease, an outage, a lapsed sign-in; the bridge suite) and OCR's
// own API. So the answer is Bunko's own shape for an outage (DavError::from_io: 503,
// Retry-After, the reason as text), made at the browser's door for one spec.
test('503: the library storage waiting fails the sync and keeps the session', async ({ reader, rig, page }) => {
    const server = bunko(rig);
    const account = await newAccount(rig);
    await connect(reader, rig, account);
    const origin = new URL(rig.readerUrl).origin;
    const outage = (route) =>
        route.request().method() === 'OPTIONS'
            ? route.fallback()
            : route.fulfill({
                  status: 503,
                  headers: {
                      'content-type': 'text/plain; charset=utf-8',
                      'retry-after': '30',
                      'access-control-allow-origin': origin,
                  },
                  body: 'the library storage is unavailable: rig outage',
              });
    await page.route(`${server.url}/mokuro-reader/**`, outage);
    await page.route(`${server.url}/mokuro-reader`, outage);

    const failed = await reader.sync();
    expect(failed.ok).toBe(false);
    expect(failed.errors.join(' ')).toMatch(/503/);
    // A wait, not a refusal: still signed in, nothing to do, not made read-only.
    expect(await reader.providerStatus()).toMatchObject({
        isAuthenticated: true,
        needsAttention: false,
        isReadOnly: false,
    });

    await page.unroute(`${server.url}/mokuro-reader/**`, outage);
    await page.unroute(`${server.url}/mokuro-reader`, outage);
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
});
