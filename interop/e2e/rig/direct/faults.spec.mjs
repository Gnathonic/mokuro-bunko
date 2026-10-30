// direct (§7.1): the reader alone, when the cloud refuses it. An expired token is
// refreshed and the sync goes on; a revoked one raises the reconnect banner, the app
// settles behind it, and a reconnect clears it; a full account fails the write as storage
// full, never as a sign-in, and loses nothing; a rate limit is waited out in the same
// sync; a conflict on the write loses neither device's progress. Every write faulted
// is the progress file's (cloud-direct.mjs `writeFaults`).
import { test, expect } from '../fixtures.mjs';
import { cloudFile, signIn, writeFaults } from '../cloud-direct.mjs';

const VOLUME_DATA = 'volume-data.json';
const PAGES = 4;

async function connect(reader, rig) {
    const { sync } = await signIn(reader, rig);
    expect(sync).toEqual({ ok: true, errors: [] });
}

/**
 * Signed in, the volume `title` downloaded and open in the reader, and the syncs the
 * app starts by itself (after the sign-in, when a reader opens) done: its id. A fault
 * set after this meets the spec's own sync, not one of those.
 */
async function ready(reader, rig, title = 'Vol 1') {
    await connect(reader, rig);
    const { volumes } = await reader.library();
    const v = volumes.find((x) => x.series === rig.library.series && x.title === title);
    expect(v, `${rig.library.series}/${title} in the library`).toBeTruthy();
    const { id } = await reader.open(v.id);
    // The reader view mounted (its foreground sync asked for), a page on screen.
    await reader.page.waitForFunction(
        () => location.hash.includes('/reader/') && document.querySelector('.pageImage') !== null,
        null,
        { timeout: 30_000 },
    );
    await reader.idle();
    return id;
}

/**
 * Read on to a page other than the one this device (and so the cloud) has: that page.
 * Its own sync follows 5 s after (activity-tracker.ts): a spec's sync comes first.
 */
async function turn(reader, id) {
    const page = ((await reader.progress(id)).page % PAGES) + 1;
    expect(await reader.setPage(id, page)).toMatchObject({ page });
    return page;
}

/** A device signed in afresh sees `page` for the volume. */
async function seenElsewhere(anotherDevice, rig, id, page) {
    const other = await anotherDevice();
    await connect(other, rig);
    expect(await other.progress(id)).toMatchObject({ page });
}

const attention = async (reader) => (await reader.providerStatus())?.needsAttention ?? false;

/** The token the page signed in with (fixtures.mjs `interopConfig`). */
const configToken = (page, key) => page.evaluate((k) => globalThis.__mokuroInteropConfig.tokens[k], key);

/** A fresh token from cloudsim, as Google's or Microsoft's sign-in would hand out. */
const mint = (rig) => rig.sim('POST', 'token', { provider: rig.provider, account: rig.simAccount });

/**
 * The cloudsim log since `from` (a seq). One account, and a request whose token names
 * none (a revoked or expired one) is logged under no account.
 */
async function logSince(rig, from) {
    return (await rig.sim('GET', 'log')).filter((e) => e.seq > from);
}
const lastSeq = async (rig) => (await rig.sim('GET', 'log')).reduce((m, e) => Math.max(m, e.seq), 0);

test('401: an expired token is refreshed and the sync goes on', async ({ reader, rig, page, anotherDevice }) => {
    // Only Drive's sign-in (GIS) hands out a new token on a 401. MSAL refreshes before
    // expiry, so a Graph 401 is a rejected token: the banner below. A MEGA session
    // does not expire; WebDAV sends its credentials on every request.
    test.skip(rig.provider !== 'google', `no token refresh on a 401 on ${rig.provider} (the banner spec covers it)`);
    const id = await ready(reader, rig);
    const target = await turn(reader, id);
    const old = await configToken(page, 'google');
    await rig.sim('POST', 'token/expire', { token: old.access_token });
    // What Google's sign-in hands out at the silent re-auth.
    const fresh = await mint(rig);
    await page.evaluate((t) => globalThis.__mokuroCloudTestHooks.setCloudTestToken('google', t), {
        access_token: fresh.access_token,
        expires_in: fresh.expires_in,
    });
    const from = await lastSeq(rig);
    // The sync that meets the 401 may fail; the re-auth's own sync carries on.
    await reader.sync();
    await expect.poll(async () => (await reader.sync()).ok, { timeout: 30_000 }).toBe(true);
    expect(await attention(reader)).toBe(false);
    const statuses = (await logSince(rig, from)).map((e) => e.status);
    expect(statuses).toContain(401);
    expect(statuses.at(-1)).toBeLessThan(400);
    await seenElsewhere(anotherDevice, rig, id, target);
});

test('revoked: the reconnect banner, and the app settles behind it; a reconnect clears it', async ({
    reader,
    rig,
    page,
}) => {
    // Nothing to write: the sync that meets the refusal only reads.
    await ready(reader, rig);
    let undo = async () => {};
    switch (rig.provider) {
        case 'google':
            await rig.sim('POST', 'token/revoke', { token: (await configToken(page, 'google')).access_token });
            break;
        case 'microsoft':
            await rig.sim('POST', 'token/revoke', { token: await configToken(page, 'onedrive') });
            break;
        case 'mega':
            await rig.sim('POST', 'knobs', { provider: 'mega', account: rig.simAccount, kill_session: true });
            break;
        case 'rclone': {
            // rclone takes one fixed password: a server that stopped accepting it is
            // answered at the browser's door.
            const origin = new URL(rig.webdav.url).origin;
            const refuse = (route) =>
                route.fulfill({ status: 401, headers: { 'WWW-Authenticate': 'Basic realm="rclone"' }, body: '' });
            await page.route(`${origin}/**`, refuse);
            undo = () => page.unroute(`${origin}/**`, refuse);
            break;
        }
    }
    const from = rig.cloudsimUrl ? await lastSeq(rig) : 0;
    expect((await reader.sync()).ok).toBe(false);
    await expect.poll(() => attention(reader), { timeout: 30_000 }).toBe(true);
    // The cloud did refuse it (a 401, MEGA's -15), and behind the banner the app
    // settles: no sync or listing of its own left running, the banner still up after
    // another sync. (Google's re-auth loop is pinned in the reader's cloudsim suite.)
    if (rig.cloudsimUrl) {
        const refused = (await logSince(rig, from)).filter((e) => e.status === 401 || e.error === -15);
        expect(refused.length).toBeGreaterThan(0);
    }
    await reader.idle();
    expect((await reader.sync()).ok).toBe(false);
    expect(await attention(reader)).toBe(true);

    // The user reconnects.
    await undo();
    switch (rig.provider) {
        case 'google': {
            const fresh = await mint(rig);
            await page.evaluate((t) => globalThis.__mokuroCloudTestHooks.setCloudTestToken('google', t), {
                access_token: fresh.access_token,
                expires_in: fresh.expires_in,
            });
            await connect(reader, rig);
            break;
        }
        case 'microsoft': {
            // The reconnect is MSAL's redirect: the page comes back with a new token.
            const fresh = await mint(rig);
            await page.addInitScript((t) => {
                globalThis.__mokuroInteropConfig.tokens.onedrive = t;
            }, fresh.access_token);
            await page.reload();
            await page.waitForFunction(() => globalThis.__mokuroInterop?.apiVersion === 1);
            await expect.poll(async () => (await reader.sync()).ok, { timeout: 30_000 }).toBe(true);
            break;
        }
        default:
            await connect(reader, rig);
    }
    expect(await attention(reader)).toBe(false);
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
});

test('quota: a full account fails the write as storage full, not a sign-in, and loses nothing', async ({
    reader,
    rig,
    page,
    anotherDevice,
}) => {
    const id = await ready(reader, rig);
    const target = await turn(reader, id);
    const before = await cloudFile(rig, VOLUME_DATA);
    expect(before, `${VOLUME_DATA} in the cloud`).toBeTruthy();
    await writeFaults(rig, page).quota(VOLUME_DATA);

    const failed = await reader.sync();
    expect(failed.ok).toBe(false);
    // The reader's own words for a full Drive, OneDrive or MEGA; a WebDAV server's
    // status, as it answered.
    const full = rig.provider === 'rclone' ? /\b507 Insufficient Storage\b/ : /storage is full/;
    expect(failed.errors.join('\n')).toMatch(full);
    // Still signed in, no reconnect asked for.
    expect(await reader.providerStatus()).toMatchObject({ isAuthenticated: true, needsAttention: false });
    // The cloud's copy is as it was. Not on a plain WebDAV server: there the reader
    // deletes a file before its PUT on purpose (webdav-core.ts, for servers that do
    // not overwrite on PUT), so a refused PUT leaves none until the next write.
    if (rig.provider !== 'rclone') {
        expect((await cloudFile(rig, VOLUME_DATA))?.sha256).toBe(before.sha256);
    }

    // Space again: the next sync lands it.
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
    await seenElsewhere(anotherDevice, rig, id, target);
});

test('429: a rate-limited write waits Retry-After and lands in the same sync', async ({
    reader,
    rig,
    page,
    anotherDevice,
}) => {
    const faults = writeFaults(rig, page);
    test.skip(!faults.rateLimit, `${rig.provider} has no rate limit to wait out (no Retry-After rule for WebDAV)`);
    const id = await ready(reader, rig);
    const target = await turn(reader, id);
    await faults.rateLimit(VOLUME_DATA);
    const from = await lastSeq(rig);
    const t0 = Date.now();
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
    const log = await logSince(rig, from);
    if (rig.provider === 'mega') {
        // Both EAGAINs were answered, and retried past.
        expect(log.filter((e) => e.error === -3)).toHaveLength(2);
    } else {
        const statuses = log.filter((e) => e.class === 'upload').map((e) => e.status);
        expect(statuses).toContain(429);
        expect(statuses.slice(statuses.indexOf(429) + 1).some((s) => s < 300)).toBe(true);
        expect(Date.now() - t0).toBeGreaterThanOrEqual(1000);
    }
    await seenElsewhere(anotherDevice, rig, id, target);
});

test('412: a conflicted write loses neither device progress', async ({ reader, rig, page, anotherDevice }) => {
    const faults = writeFaults(rig, page);
    test.skip(!faults.conflict, `${rig.provider} has no conditional write to conflict with`);
    const id = await ready(reader, rig);
    const mine = await turn(reader, id);
    await faults.conflict(VOLUME_DATA);
    const failed = await reader.sync();
    expect(failed.ok).toBe(false);
    expect(failed.errors.join('\n')).toMatch(/412/);
    expect(await attention(reader)).toBe(false);

    // Meanwhile another device writes its own reading of the other volume.
    const other = await anotherDevice();
    const otherId = await ready(other, rig, 'Vol 2');
    const theirs = await turn(other, otherId);
    expect(await other.sync()).toEqual({ ok: true, errors: [] });

    // This device's next sync merges the two; each device then has both.
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
    expect(await reader.progress(otherId)).toMatchObject({ page: theirs });
    expect(await other.sync()).toEqual({ ok: true, errors: [] });
    expect(await other.progress(id)).toMatchObject({ page: mine });
});
