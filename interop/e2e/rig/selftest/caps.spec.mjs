// The fixtures' own checks, against a stub page instead of a reader (no build, no
// parties): `npm run selftest` in interop/ (selftest/run.mjs).
import { test, expect, openReader } from '../fixtures.mjs';
import { V1_METHODS } from '../driver.mjs';
import { createServer } from 'node:net';

/** A stub reader: driver v1 (every method but `lacking`) and the cloud test hooks. */
const stubReader = (lacking = []) => `<!doctype html><title>stub</title><script>
globalThis.__mokuroInterop = { apiVersion: 1, capabilities: ['history'] };
for (const m of ${JSON.stringify(V1_METHODS.filter((m) => !lacking.includes(m)))}) {
    globalThis.__mokuroInterop[m] = async () => null;
}
globalThis.__mokuroCloudTestHooks = { setCloudTestBases() {}, setCloudTestToken() {} };
</script>`;
const STUB = stubReader();

test.use({
    readerUrl: 'http://127.0.0.1:9/',
    readerStub: STUB,
    pendingCapsFile: new URL('./pending-caps.txt', import.meta.url),
});

test.describe('a capability the reader has', () => {
    test.use({ needs: ['history'] });
    test('runs the spec, with the interop config set before the page ran', async ({ reader, page }) => {
        expect(await reader.apiVersion()).toBe(1);
        expect(await reader.capabilities()).toEqual(['history']);
        expect(await page.evaluate(() => globalThis.__mokuroInteropConfig)).toMatchObject({ v: 1, gapiShim: true });
    });
});

// openReader is what the `reader` fixture runs, called here so the test sees its
// answer: a `test.fail` around the fixture would pass on any error at all, and on a
// skip.
test('a missing capability the pending list lacks fails the spec, naming it', async ({
    page,
    readerUrl,
    pendingCapsFile,
}) => {
    const opening = openReader(page, { readerUrl, needs: ['history', 'rig-selftest-unlisted'], pendingCapsFile });
    await expect(opening).rejects.toThrow(/^the reader lacks capability rig-selftest-unlisted \(it has: history\)/);
});

test.describe('a v1 driver without one of its methods', () => {
    test.use({ readerStub: stubReader(['settleSeriesWrites']) });
    test('fails the spec at the gate, naming the method', async ({ page, readerUrl, pendingCapsFile }) => {
        const opening = openReader(page, { readerUrl, needs: ['history'], pendingCapsFile });
        await expect(opening).rejects.toThrow(/^the reader's rig driver v1 lacks settleSeriesWrites/);
    });
});

test('a missing capability the pending list names skips the spec, naming it', async ({
    page,
    readerUrl,
    pendingCapsFile,
}) => {
    const opened = await openReader(page, { readerUrl, needs: ['history', 'rig-selftest-pending'], pendingCapsFile });
    expect(opened).toEqual({ skip: ['rig-selftest-pending'] });
});

// The fixture's own skip; run.mjs checks it is this suite's only skipped test.
test.describe('a missing capability the pending list names', () => {
    test.use({ needs: ['history', 'rig-selftest-pending'] });
    test('is skipped, with the capability reported', async ({ reader }) => {
        void reader;
        throw new Error('a pending capability must skip the spec before it runs');
    });
});

const STUBBED = ['https://apis.google.com/js/api.js', 'https://accounts.google.com/gsi/client'];

test.describe('a request off this machine', () => {
    // It fails the spec (egress.mjs refusedError); here it is made on purpose.
    test.use({ refusedFails: false });
    test('is refused and recorded; the Google scripts load empty', async ({
        reader,
        page,
        refusedHosts,
        stubbedUrls,
    }) => {
        void reader;
        const answer = await page.evaluate(() =>
            fetch('https://example.invalid/x').then(
                () => 'answered',
                () => 'refused',
            ),
        );
        expect(answer).toBe('refused');
        expect(refusedHosts).toEqual(['example.invalid']);
        for (const src of ['https://apis.google.com/js/api.js', 'https://accounts.google.com/gsi/client']) {
            const loaded = await page.evaluate(
                (s) =>
                    new Promise((resolve) => {
                        const el = document.createElement('script');
                        el.src = s;
                        el.onload = () => resolve(true);
                        el.onerror = () => resolve(false);
                        document.head.append(el);
                    }),
                src,
            );
            expect(loaded, src).toBe(true);
        }
        expect(refusedHosts).toEqual(['example.invalid']);
        expect(stubbedUrls).toEqual(['https://apis.google.com/js/api.js', 'https://accounts.google.com/gsi/client']);
    });

    // A second device in the same spec (its own browser context) is held to the same
    // rule, and what it was refused counts against the spec.
    test('from another device is refused and recorded on the spec, its config set first', async ({
        anotherDevice,
        refusedHosts,
    }) => {
        const other = await anotherDevice();
        expect(await other.apiVersion()).toBe(1);
        expect(await other.page.evaluate(() => globalThis.__mokuroInteropConfig)).toMatchObject({
            v: 1,
            gapiShim: true,
        });
        const answer = await other.page.evaluate(() =>
            fetch('https://example.invalid/other').then(
                () => 'answered',
                () => 'refused',
            ),
        );
        expect(answer).toBe('refused');
        expect(refusedHosts).toEqual(['example.invalid']);
    });
});

// A route sees requests only: a WebSocket, a preconnect or a DNS prefetch never asks
// it, so the browser itself must not resolve or reach any host off this machine
// (playwright.config.mjs). `offbox.localhost` and 127.0.0.2 stand for such hosts: on
// this machine both reach the listener below, so a connection that escapes lands where
// the test sees it. (Preconnects need no case: this Chromium makes none, even to
// 127.0.0.1.)
test('a host off the loopback gets no connection, not even a WebSocket', async ({ reader, page }) => {
    void reader;
    const listen = (host) =>
        new Promise((resolve, reject) => {
            const hits = [];
            const server = createServer((socket) => {
                hits.push(socket.remoteAddress);
                socket.destroy();
            });
            server.once('error', reject);
            server.listen(0, host, () => resolve({ server, hits, port: server.address().port }));
        });
    const named = await listen('127.0.0.1');
    const literal = await listen('127.0.0.2');
    try {
        for (const url of [`ws://offbox.localhost:${named.port}/`, `ws://127.0.0.2:${literal.port}/`]) {
            const outcome = await page.evaluate(
                (u) =>
                    new Promise((resolve) => {
                        const sock = new WebSocket(u);
                        sock.onopen = () => resolve('open');
                        sock.onclose = () => resolve('closed');
                    }),
                url,
            );
            expect(outcome, url).toBe('closed');
        }
        expect(named.hits).toEqual([]);
        expect(literal.hits).toEqual([]);
    } finally {
        named.server.close();
        literal.server.close();
    }
});

test.describe('a stale pending entry', () => {
    // Pending, yet the reader has it: the spec runs (and the entry should go).
    test.use({ needs: ['history'], pendingCapsFile: new URL('./pending-has.txt', import.meta.url) });
    test('does not skip a spec the reader can run', async ({ reader }) => {
        expect(await reader.capabilities()).toContain('history');
    });
});

// While any route is on, Playwright answers a CORS preflight itself, permissively, and
// the server never sees it: a server whose CORS a browser would refuse still passes.
// `preflightsReachServers` watches requests instead of routing them; the rest of the
// rule holds (the Google scripts load empty, a host off this machine fails the spec).
// The page comes from a real loopback server here: Chromium refuses a page a route
// fulfilled any request into the loopback address space, preflight or not.
const { createServer: http } = await import('node:http');
const davSeen = [];
const dav = http((req, res) => {
    if (req.url === '/') {
        res.writeHead(200, { 'content-type': 'text/html' });
        return res.end(STUB);
    }
    davSeen.push({ method: req.method, origin: req.headers.origin ?? null });
    res.writeHead(req.method === 'OPTIONS' ? 204 : 207, {
        'access-control-allow-origin': req.headers.origin ?? '*',
        'access-control-allow-methods': 'PROPFIND',
        'access-control-allow-headers': 'authorization, depth',
    });
    res.end();
});
await new Promise((resolve) => dav.listen(0, '127.0.0.1', resolve));
test.afterAll(() => dav.close());
// The page on `localhost`, the DAV server on 127.0.0.1: two origins, one machine.
const davPage = `http://localhost:${dav.address().port}/`;
const davUrl = `http://127.0.0.1:${dav.address().port}/dav`;
const propfind = (url) =>
    fetch(url, { method: 'PROPFIND', headers: { authorization: 'Bearer x', depth: '0' } }).then((r) => r.status);

test.describe('the DAV server', () => {
    test.use({ readerUrl: davPage, readerStub: null });
    test.beforeEach(() => {
        davSeen.length = 0;
    });

    test.describe('with preflights reaching the servers', () => {
        test.use({ preflightsReachServers: true, refusedFails: false });
        test('answers the preflight of a cross-origin PROPFIND itself', async ({ reader, page }) => {
            void reader;
            expect(await page.evaluate(propfind, davUrl)).toBe(207);
            const origin = davPage.replace(/\/$/, '');
            expect(davSeen).toEqual([
                { method: 'OPTIONS', origin },
                { method: 'PROPFIND', origin },
            ]);
        });
        test('a host off this machine is still refused and recorded; the Google scripts load empty', async ({
            reader,
            page,
            refusedHosts,
            stubbedUrls,
        }) => {
            void reader;
            const answer = await page.evaluate(() =>
                fetch('https://example.invalid/x').then(
                    () => 'answered',
                    () => 'refused',
                ),
            );
            expect(answer).toBe('refused');
            // A blob: or data: URL names no host: nothing to refuse.
            const local = await page.evaluate(async () => {
                const blob = URL.createObjectURL(new Blob(['x']));
                return [await (await fetch(blob)).text(), await (await fetch('data:text/plain,y')).text()];
            });
            expect(local).toEqual(['x', 'y']);
            for (const src of STUBBED) {
                const loaded = await page.evaluate(
                    (s) =>
                        new Promise((resolve) => {
                            const el = document.createElement('script');
                            el.src = s;
                            el.onload = () => resolve(true);
                            el.onerror = () => resolve(false);
                            document.head.append(el);
                        }),
                    src,
                );
                expect(loaded, src).toBe(true);
            }
            expect(refusedHosts).toEqual(['example.invalid']);
            expect(stubbedUrls).toEqual(STUBBED);
        });
    });

    test('never sees the preflight under the route: Playwright answers it', async ({ reader, page }) => {
        void reader;
        expect(await page.evaluate(propfind, davUrl)).toBe(207);
        expect(davSeen.map((s) => s.method)).toEqual(['PROPFIND']);
    });
});
