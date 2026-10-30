// The rig's browser suites (docs/design/interop-testing.md §7.1). `cargo xtask interop
// rig` runs one suite per invocation (RIG_SUITE names its folder) with the parties'
// addresses in the environment (fixtures.mjs). This is Bunko's own Playwright install
// (interop/package.json), not the reader's.
import { defineConfig } from '@playwright/test';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const work = process.env.RIG_WORKDIR ?? path.join(here, '..', '..', 'test-results');

const reporter = [['list']];
if (process.env.RIG_PW_JSON) reporter.push(['json', { outputFile: process.env.RIG_PW_JSON }]);
if (process.env.RIG_JUNIT) reporter.push(['junit', { outputFile: process.env.RIG_JUNIT }]);

export default defineConfig({
    testDir: path.resolve(here, process.env.RIG_SUITE ?? '.'),
    testMatch: '**/*.spec.mjs',
    // One server and one cloud per suite: specs run one at a time, each in a fresh
    // browser context (fixtures.mjs).
    workers: 1,
    fullyParallel: false,
    // A flaky rig spec is a failure to look at, not to retry past.
    retries: 0,
    forbidOnly: true,
    timeout: 120_000,
    expect: { timeout: 30_000 },
    outputDir: path.join(work, 'playwright'),
    reporter,
    use: {
        browserName: 'chromium',
        headless: !process.env.RIG_HEADED,
        // A service worker's fetches bypass context.route, which keeps every request
        // on this machine.
        serviceWorkers: 'block',
        launchOptions: {
            // What the route never sees (a WebSocket, a preconnect, a DNS prefetch)
            // still must not leave: every host but the loopback ones fails to resolve,
            // IP literals included, so nothing off this machine is looked up or reached.
            // localhost stays: cloudsim's pre-authenticated URLs name it. (The IPv6
            // loopback is excluded as `::1`; `[::1]` would not match it.)
            args: ['--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1, EXCLUDE localhost, EXCLUDE ::1'],
        },
        viewport: { width: 1280, height: 900 },
        trace: 'retain-on-failure',
    },
});
