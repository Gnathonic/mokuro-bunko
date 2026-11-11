// Catalog page timings in Chromium, for `cargo xtask interop catalog-perf --browser`.
// Prints one JSON line of metrics (milliseconds from navigation start, and counts).
//
//   BUNKO_URL=http://127.0.0.1:PORT SMALL_SERIES="Small Series" node e2e/catalog-perf.mjs
import { chromium } from 'playwright';

const base = process.env.BUNKO_URL;
const small = process.env.SMALL_SERIES;
if (!base || !small) {
    console.error('BUNKO_URL and SMALL_SERIES are required');
    process.exit(2);
}

// Records performance.now() the first time `selector` matches `count` elements, from a
// MutationObserver installed before any page script runs (no polling delay).
function watch([selector, count]) {
    const check = () => {
        if (window.__seen === undefined && document.querySelectorAll(selector).length >= count) {
            window.__seen = performance.now();
        }
    };
    new MutationObserver(check).observe(document, { childList: true, subtree: true });
}

const browser = await chromium.launch();
const metrics = {};

async function freshPage(selector, count) {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
    const page = await context.newPage();
    await page.addInitScript(watch, [selector, count]);
    return { context, page };
}

async function seenAt(page) {
    await page.waitForFunction(() => window.__seen !== undefined, null, { timeout: 60000 });
    return page.evaluate(() => ({
        ms: window.__seen,
        // The document itself plus every resource requested before the cards showed.
        requests: 1 + performance.getEntriesByType('resource').filter((e) => e.startTime < window.__seen).length,
    }));
}

// 1. The catalog root: first series cards on screen.
{
    const { context, page } = await freshPage('.volume-card[data-series]', 1);
    await page.goto(base + '/catalog/');
    const seen = await seenAt(page);
    metrics.catalog_first_cards_ms = seen.ms;
    metrics.catalog_requests_before_cards = seen.requests;

    // 2. From the loaded catalog, open the small series by clicking its card.
    const card = page.locator(`.volume-card[data-series="${small}"]:not([data-volume])`);
    await card.waitFor({ state: 'attached', timeout: 60000 });
    metrics.series_open_ms = await page.evaluate(async ([name, count]) => {
        const grid = document.getElementById('catalog-grid');
        const target = [...grid.querySelectorAll('.volume-card[data-series]')].find(
            (el) => el.dataset.series === name && el.dataset.volume === undefined,
        );
        return new Promise((resolve) => {
            const t0 = performance.now();
            const done = () => {
                if (grid.querySelectorAll('.volume-card[data-volume]').length >= count) {
                    observer.disconnect();
                    resolve(performance.now() - t0);
                }
            };
            const observer = new MutationObserver(done);
            observer.observe(grid, { childList: true, subtree: true });
            target.click();
        });
    }, [small, 3]);
    await context.close();
}

// 3. A pasted link straight to the small series: its volumes on screen.
{
    const { context, page } = await freshPage('.volume-card[data-volume]', 3);
    await page.goto(base + '/catalog/#' + encodeURIComponent(small));
    metrics.series_rendered_ms = (await seenAt(page)).ms;
    await context.close();
}

await browser.close();
console.log(JSON.stringify(metrics));
