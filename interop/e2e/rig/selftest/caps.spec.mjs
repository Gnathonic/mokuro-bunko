// The fixtures' own checks, against a stub page instead of a reader (no build, no
// parties): `npx playwright test --config e2e/rig/playwright.config.mjs` with
// RIG_SUITE=selftest, in interop/.
import { test, expect } from '../fixtures.mjs';

const STUB = `<!doctype html><title>stub</title><script>
globalThis.__mokuroInterop = { apiVersion: 1, capabilities: ['history'] };
</script>`;

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

test.describe('a missing capability the pending list lacks', () => {
    test.use({ needs: ['rig-selftest-unlisted'] });
    // Expected to fail: the reader fixture throws before the body runs.
    test.fail('fails the spec', async ({ reader }) => {
        void reader;
    });
});

test.describe('a missing capability the pending list names', () => {
    test.use({ needs: ['history', 'rig-selftest-pending'] });
    test('is skipped, with the capability reported', async ({ reader }) => {
        void reader;
        throw new Error('a pending capability must skip the spec before it runs');
    });
});

test('a request off this machine is refused and recorded; the Google scripts load empty', async ({
    reader,
    page,
    refusedHosts,
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
});

test.describe('a stale pending entry', () => {
    // Pending, yet the reader has it: the spec runs (and the entry should go).
    test.use({ needs: ['history'], pendingCapsFile: new URL('./pending-has.txt', import.meta.url) });
    test('does not skip a spec the reader can run', async ({ reader }) => {
        expect(await reader.capabilities()).toContain('history');
    });
});
