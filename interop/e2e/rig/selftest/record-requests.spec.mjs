// recordRequests (bunko.mjs), the specs' view of what the page sent a server, against
// a stub page that reloads with a request still in flight.
import { test, expect } from '../fixtures.mjs';
import { createServer } from 'node:http';
import { recordRequests } from '../bunko.mjs';

const stub = createServer((req, res) => {
    // Never answered: the reload cuts it off, as it cut off the reader's cover
    // downloads in dav/auth's reload spec.
    if (req.url === '/hang') return;
    res.setHeader('content-type', 'text/html');
    res.end('<!doctype html><title>stub</title>');
});
await new Promise((resolve) => stub.listen(0, '127.0.0.1', resolve));
test.afterAll(() => {
    stub.closeAllConnections();
    stub.close();
});
const origin = `http://127.0.0.1:${stub.address().port}`;

// Watched, not routed, as dav's specs run (a routed request is Playwright's own).
test.use({ preflightsReachServers: true, refusedFails: false });

test('a request a reload cut off is taken, with the headers the page set', async ({ page }) => {
    // take() hanging is the failure: let it fail well before the suite's timeout.
    test.setTimeout(20_000);
    await page.goto(`${origin}/`);
    const requests = recordRequests(page, origin);
    const sent = page.waitForRequest((r) => new URL(r.url()).pathname === '/hang');
    await page.evaluate(() => {
        void fetch('/hang', { headers: { authorization: 'Bearer stub' } }).catch(() => {});
    });
    await sent;
    await page.reload();
    // The reload's own document request is recorded too (the stub serves both).
    const taken = await requests.take();
    expect(taken.filter((r) => r.path === '/hang')).toEqual([
        { method: 'GET', path: '/hang', authorization: 'Bearer stub' },
    ]);
});
