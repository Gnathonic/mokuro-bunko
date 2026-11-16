// api (§7.1; queue file v2 §12.4): the OCR queue on a real Bunko server. Bunko's remote
// OCR runs on the rig's processor-fake, which holds every volume it claims until the
// hold file exists (RIG_FAKEPROC_HOLD); `Vol 2` has no sidecar, so its OCR job is
// claimed and held.
//
// - An older reader's client still gets the v1 root file `/mokuro-reader/.mokuro-queue.json`
//   (virtual and frozen) with its 304 under `If-None-Match`: asked from Node exactly as
//   released readers ask it (`interop/contract/reader/queue-get*.json`).
// - A v2 reader reads nothing of the queue on the library grid or in the reader. On the
//   series page it reads that one series' virtual file
//   `/mokuro-reader/<Series>/.mokuro-series-queue.json`: a 200, then 304s under
//   `If-None-Match`, and the volume's card shows the job. Once the job is done it reads
//   a new file (a 200 under another tag), the card clears, and the OCR the server made
//   reaches the image-only volume it has installed (the scoped pull of that one series).
//   It never reads the v1 root file of a v2 Bunko.
import { writeFileSync } from 'node:fs';
import { test, expect } from '../fixtures.mjs';
import { basic, bunko } from '../bunko.mjs';
import { ROOT_QUEUE_FILE, SERIES_QUEUE_FILE, recordQueueReads, seriesFolders } from '../queue-reads.mjs';

test.use({ preflightsReachServers: true });

const V1_QUEUE = `/mokuro-reader/${ROOT_QUEUE_FILE}`;

/** The tab hidden and shown again, as switching away and back does. */
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

test('an older reader still gets the v1 root file, and its 304', async ({ rig }) => {
    test.setTimeout(120_000);
    const server = bunko(rig);
    const account = await server.createUser(`api-v1-${Date.now().toString(36)}`, 'rig-queue-v1');
    const auth = basic(account.username, account.password);
    // Bunko claims Vol 2 (it has no sidecar) for the held processor shortly after start.
    let first;
    await expect
        .poll(
            async () => {
                const r = await server.request('GET', V1_QUEUE, { auth });
                first = { status: r.status, etag: r.headers.get('etag'), body: await r.json().catch(() => null) };
                return (first.body?.volumes ?? []).map((v) => `${v.series}/${v.volume}`);
            },
            { timeout: 60_000, intervals: [500, 1000] },
        )
        .toContain(`${rig.library.series}/Vol 2`);
    expect(first.status).toBe(200);
    expect(first.etag).toBeTruthy();
    // The v1 shape, frozen: released readers parse exactly this.
    expect(first.body).toMatchObject({ version: 1 });
    expect(first.body).toHaveProperty('next_check_after');
    expect(first.body).toHaveProperty('pending_volumes');
    const vol2 = first.body.volumes.find((v) => v.volume === 'Vol 2');
    expect(vol2.jobs.length).toBeGreaterThan(0);
    expect(vol2.jobs[0]).toMatchObject({ kind: expect.any(String), id: expect.any(String) });
    // Asked again with its tag while nothing changed: 304 (the v1 `same_queue` fix keeps
    // the tag still while the document is, `generated_at` aside).
    const again = await server.request('GET', V1_QUEUE, { auth, headers: { 'if-none-match': first.etag } });
    expect(again.status).toBe(304);
    expect(again.headers.get('etag')).toBe(first.etag);
});

test.describe('queue file v2', () => {
    test.use({ needs: ['series-queue-v2'] });

    test('the series page reads its one file with If-None-Match, shows the held job, and pulls the OCR once done', async ({
        reader,
        rig,
    }) => {
        test.setTimeout(300_000);
        const server = bunko(rig);
        const series = rig.library.series;
        const seriesPath = `/mokuro-reader/${encodeURIComponent(series)}/${SERIES_QUEUE_FILE}`;
        const account = await server.createUser(`api-q-${Date.now().toString(36)}`, 'rig-queue-1');
        const reads = recordQueueReads(reader.page, rig);
        const looks = [];
        reader.page.on('response', async (r) => {
            const url = new URL(r.url());
            if (url.origin !== server.url || url.pathname !== seriesPath) return;
            const ifNoneMatch = (await r.request().allHeaders())['if-none-match'] ?? null;
            looks.push({ status: r.status(), ifNoneMatch, etag: (await r.allHeaders()).etag ?? null });
        });

        const { sync } = await reader.connectWebDAV({
            url: rig.bunkoUrl,
            username: account.username,
            password: account.password,
        });
        expect(sync).toEqual({ ok: true, errors: [] });

        // The library grid, then Vol 2 opened (installed image only: its OCR is what the
        // server is doing): no queue reads at all, through more than one WebDAV floor
        // (30 s, the old root poller's period) and a tab switch.
        const vol2 = (await reader.library()).volumes.find((v) => v.series === series && v.title === 'Vol 2');
        const { id } = await reader.open(vol2.id);
        expect((await reader.library()).volumes.find((v) => v.id === id).hasOcr).toBe(false);
        await tabAwayAndBack(reader.page);
        await reader.page.waitForTimeout(31_000);
        await tabAwayAndBack(reader.page);
        expect((await reads.reads()).filter((r) => r.file !== 'marker')).toEqual([]);

        // The series page: its one file, the job on the card.
        const opened = reads.mark();
        await reader.showSeries(series);
        await expect
            .poll(() => reader.pendingOcr(id), { timeout: 30_000, intervals: [500, 1000] })
            .toMatchObject({
                title: 'Server OCR',
                lines: [{ fullName: expect.stringMatching(/^(Text|Hayai Nova\b.*)$/) }],
            });
        expect(looks[0]).toMatchObject({ status: 200, ifNoneMatch: null });
        expect(looks[0].etag).toBeTruthy();
        // The next look (every 30 s while it lists a job) asks with the tag: 304.
        await expect
            .poll(() => looks.slice(1).some((l) => l.status === 304 && l.ifNoneMatch === looks[0].etag), {
                timeout: 60_000,
                intervals: [1000],
            })
            .toBe(true);
        expect(await reader.pendingOcr(id)).toMatchObject({ title: 'Server OCR' });

        // The processor finishes: a new file (a 200 under another tag), the card clears,
        // and the OCR the server made is on this device.
        const released = looks.length;
        writeFileSync(rig.fakeprocHold, '');
        await expect
            .poll(
                async () => {
                    const v = (await reader.library()).volumes.find((x) => x.id === id);
                    return { pending: await reader.pendingOcr(id), hasOcr: v.hasOcr };
                },
                { timeout: 150_000, intervals: [1000, 2000] },
            )
            .toEqual({ pending: null, hasOcr: true });
        expect(looks.slice(released).some((l) => l.status === 200 && l.etag !== looks[0].etag)).toBe(true);

        // Only that series' file, never the v1 root file of a v2 Bunko.
        const all = await reads.reads();
        expect(seriesFolders(await reads.reads(opened))).toEqual([series]);
        expect(all.filter((r) => r.file === 'root')).toEqual([]);
    });
});
