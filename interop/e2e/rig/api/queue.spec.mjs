// api (§7.1): the reader's queue poller and pending view on a real queue. Bunko's
// remote OCR runs on the rig's processor-fake, which holds every volume it claims until
// the hold file exists (RIG_FAKEPROC_HOLD); `Vol 2` has no sidecar, so its OCR job is
// claimed and held. The reader polls `/mokuro-reader/.mokuro-queue.json` with
// `If-None-Match` (a 304 while nothing changed), shows the job on the volume's card,
// and once the job is done (the volume left the file) pulls the new sidecar through
// the volume's manifest: the OCR of the image-only volume it has installed.
import { writeFileSync } from 'node:fs';
import { test, expect } from '../fixtures.mjs';
import { bunko } from '../bunko.mjs';

test.use({ preflightsReachServers: true });

const QUEUE_FILE = '/mokuro-reader/.mokuro-queue.json';

/**
 * The tab hidden and shown again, as switching away and back does: the poller takes one
 * look then (it polls every `next_check_after`, 30 s at the least, otherwise).
 */
async function tabAwayAndBack(page) {
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

test('the poller asks with If-None-Match, shows the held job, and pulls the OCR once done', async ({ reader, rig }) => {
    test.setTimeout(180_000);
    const server = bunko(rig);
    const account = await server.createUser(`api-q-${Date.now().toString(36)}`, 'rig-queue-1');
    const polls = [];
    reader.page.on('response', async (r) => {
        const url = new URL(r.url());
        if (url.origin !== server.url || url.pathname !== QUEUE_FILE) return;
        const ifNoneMatch = (await r.request().allHeaders())['if-none-match'] ?? null;
        polls.push({ status: r.status(), ifNoneMatch, etag: (await r.allHeaders()).etag ?? null });
    });

    const { sync } = await reader.connectWebDAV({
        url: rig.bunkoUrl,
        username: account.username,
        password: account.password,
    });
    expect(sync).toEqual({ ok: true, errors: [] });
    // A connect polls the server's queue at once (not only the next app start).
    await expect.poll(() => polls.length, { timeout: 30_000 }).toBeGreaterThan(0);
    expect(polls[0].status).toBe(200);

    // Vol 2 on this device, image only (its OCR is what the server is doing), and its
    // card on screen.
    const vol2 = (await reader.library()).volumes.find((v) => v.series === rig.library.series && v.title === 'Vol 2');
    const { id } = await reader.open(vol2.id);
    expect((await reader.library()).volumes.find((v) => v.id === id).hasOcr).toBe(false);
    await reader.showSeries(rig.library.series);

    // Nothing changed on the server: the next look is a 304, and the card shows the job.
    const before = polls.length;
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                return reader.pendingOcr(id);
            },
            { timeout: 30_000, intervals: [500, 1000] },
        )
        .toMatchObject({ title: 'Server OCR', lines: [{ fullName: 'Text' }] });
    const looks = polls.slice(before);
    expect(looks.length).toBeGreaterThan(0);
    for (const p of looks) {
        expect(p).toMatchObject({ status: 304, ifNoneMatch: polls[0].etag });
    }

    // The processor finishes: the job leaves the file, the card clears, and the OCR the
    // server made is on this device.
    const released = polls.length;
    writeFileSync(rig.fakeprocHold, '');
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                const v = (await reader.library()).volumes.find((x) => x.id === id);
                return { pending: await reader.pendingOcr(id), hasOcr: v.hasOcr };
            },
            { timeout: 120_000, intervals: [1000, 2000] },
        )
        .toEqual({ pending: null, hasOcr: true });
    // Seen in a new file (a 200 under another tag), not in the cached one.
    expect(polls.slice(released).some((p) => p.status === 200 && p.etag !== polls[0].etag)).toBe(true);
});
