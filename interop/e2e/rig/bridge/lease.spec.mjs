// bridge (§7.1): Bunko's lease on the cloud library, as the reader meets it. A Bunko
// started after its marker went stale claims the library afresh (§9.2 step 1): until
// the claim settles, a write through it answers 503 `storage-lease-claiming` with a
// Retry-After of what is left of the settle wait (30 s while Bunko then confirms its
// claim). The reader waits such a short Retry-After out (at most 30 s, at most twice
// per request) and sends the same request again, so the write lands once the lease is
// held, without touching its sign-in. A Bunko restarted while its marker is fresh
// resumes it at once (same epoch, no settle wait), and the reader goes on on the
// session it had, its next write landing.
import { test, expect } from '../fixtures.mjs';
import { recordRequests } from '../bunko.mjs';
import { firstLines, landedInCloud, leaseNow, newAccount, openVolume } from './bridge.mjs';

// Each write is a Publish of the account's edit: the shared `curated` layer.
test.use({ needs: ['edit-layers-personal', 'edit-layers-curated'] });
test.describe.configure({ mode: 'serial' });

/**
 * The claim's settle wait for the spec's restart, time to write through it: half the
 * lease ttl, since a claimant writes no heartbeat while it settles (one as long as the
 * ttl would find its own marker stale and claim again, forever).
 */
const settleMs = (rig) => Math.floor((rig.lease.ttl_s * 1000) / 2);

/**
 * Every answer the reader got from Bunko, in order: its status, and for a 503 its
 * Retry-After and text (null until read).
 */
function recordAnswers(page, bunkoUrl) {
    const origin = new URL(bunkoUrl).origin;
    const seen = [];
    page.on('response', async (r) => {
        if (new URL(r.url()).origin !== origin) return;
        const entry = {
            status: r.status(),
            method: r.request().method(),
            path: decodeURIComponent(new URL(r.url()).pathname),
            retryAfter: Number(r.headers()['retry-after']),
            text: r.status() === 503 ? null : '',
        };
        seen.push(entry);
        if (entry.status === 503) entry.text = await r.text().catch(() => '');
    });
    return seen;
}

const refusalsOf = (answers) => answers.filter((a) => a.status === 503);

const attention = async (reader) => (await reader.providerStatus())?.needsAttention ?? false;

test('a write during the claim waits out its 503 storage-lease-claiming and lands once claimed', async ({
    reader,
    rig,
}) => {
    test.setTimeout(300_000);
    const text = `請求中 ${Date.now().toString(36)}`;
    const rel = `${rig.library.series}/Vol 1.curated.mokuro`;
    const account = await newAccount(rig, 'bridge-claim');
    const id = await openVolume(reader, rig, account);
    expect(await reader.editFirstBlock(id, 1, text)).toBeTruthy();
    // The edit is on Bunko (its push makes the layer Publish takes).
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                return (await reader.library()).volumes.find((v) => v.id === id)?.layers ?? [];
            },
            { timeout: 60_000, intervals: [500, 1000, 2000] },
        )
        .toContain('edit');
    const before = await leaseNow(rig);
    expect(before?.state).toBe('held');

    await rig.restartBunko({ stale: true, tune: { lease_settle_ms: settleMs(rig) } });
    // The new process claims: its marker written, the settle wait running.
    await expect
        .poll(async () => (await leaseNow(rig))?.claiming_left_s ?? 0, { timeout: 60_000, intervals: [100, 250] })
        .toBeGreaterThan(2);

    const answers = recordAnswers(reader.page, rig.bunkoUrl);
    // One Publish, sent during the claim: held back by the fence, never failed by it.
    const during = await reader.publishCurated(id);
    expect(during.outcome).toBe('published');
    const after = await leaseNow(rig);
    expect(after?.state, 'the lease held when the write came back').toBe('held');
    // A new claim: the epoch moved on.
    expect(after.holder.epoch).toBeGreaterThan(before.holder.epoch);

    const refusals = refusalsOf(answers);
    // The Publish's own upload was among them, and its last answer let it through.
    const isPublish = (a) => a.method === 'PUT' && a.path.endsWith(`/${rel}`);
    expect(refusals.some(isPublish), 'the fence refused the Publish itself').toBe(true);
    await expect
        .poll(() => Math.floor((answers.filter(isPublish).at(-1)?.status ?? 0) / 100), {
            message: 'the Publish landed',
        })
        .toBe(2);
    await expect.poll(() => refusals.every((r) => r.text !== null)).toBe(true);
    for (const r of refusals) {
        expect(r.text, `${r.method} ${r.path}`).toContain('storage-lease-claiming');
        expect(r.retryAfter, `${r.method} ${r.path} Retry-After`).toBeGreaterThanOrEqual(1);
    }
    // Each refused request was answered later (the one publishCurated call above is what
    // makes it the same Publish).
    const through = (r) =>
        answers
            .slice(answers.indexOf(r) + 1)
            .some((a) => a.status !== 503 && a.method === r.method && a.path === r.path);
    expect(
        refusals.filter((r) => !through(r)).map((r) => `${r.method} ${r.path}`),
        'refused requests that never got through',
    ).toEqual([]);

    const doc = await landedInCloud(rig, account, rel, (d) => (firstLines(d) ?? []).includes(text));
    expect(firstLines(doc)).toEqual([text]);
    // A wait, not a sign-in problem: the session stands.
    expect(await attention(reader)).toBe(false);
});

test('a restart resumes the lease at once; the reader goes on with its session', async ({ reader, rig }) => {
    test.setTimeout(180_000);
    const text = `再開 ${Date.now().toString(36)}`;
    const rel = `${rig.library.series}/Vol 1.curated.mokuro`;
    const account = await newAccount(rig, 'bridge-resume');
    const id = await openVolume(reader, rig, account);
    expect(await reader.editFirstBlock(id, 1, text)).toBeTruthy();
    await expect
        .poll(
            async () => {
                expect(await reader.sync()).toEqual({ ok: true, errors: [] });
                return (await reader.library()).volumes.find((v) => v.id === id)?.layers ?? [];
            },
            { timeout: 60_000, intervals: [500, 1000, 2000] },
        )
        .toContain('edit');
    // Held, and settled from any earlier claim (an earlier spec's restart).
    await expect.poll(async () => (await leaseNow(rig))?.state, { timeout: 60_000 }).toBe('held');
    const before = await leaseNow(rig);

    const requests = recordRequests(reader.page, rig.bunkoUrl);
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
    const tokensBefore = new Set((await requests.take()).map((r) => r.authorization).filter(Boolean));
    expect(tokensBefore.size).toBe(1);
    expect([...tokensBefore][0]).toMatch(/^Bearer /);

    await rig.restartBunko();
    // Its own fresh marker: resumed, never a settle wait, the epoch kept.
    const states = [];
    await expect
        .poll(
            async () => {
                const l = await leaseNow(rig);
                if (l) states.push(l);
                return l?.state;
            },
            { timeout: 60_000, intervals: [100, 250] },
        )
        .toBe('held');
    expect(states.filter((l) => l.claiming_left_s !== undefined)).toEqual([]);
    expect((await leaseNow(rig)).holder.epoch).toBe(before.holder.epoch);

    const answers = recordAnswers(reader.page, rig.bunkoUrl);
    expect(await reader.sync()).toEqual({ ok: true, errors: [] });
    const published = await reader.publishCurated(id);
    expect(published.outcome).toBe('published');
    const doc = await landedInCloud(rig, account, rel, (d) => (firstLines(d) ?? []).includes(text));
    expect(firstLines(doc)).toEqual([text]);
    expect(refusalsOf(answers)).toEqual([]);
    // The same session: the token it had, no new sign-in.
    const after = await requests.take();
    expect(after.filter((r) => r.path.startsWith('/login/'))).toEqual([]);
    expect(new Set(after.map((r) => r.authorization).filter(Boolean))).toEqual(tokensBefore);
    expect(await attention(reader)).toBe(false);
});
