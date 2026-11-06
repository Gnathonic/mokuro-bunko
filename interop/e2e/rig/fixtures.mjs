// What every rig spec gets: the parties' addresses (the environment `cargo xtask
// interop rig` sets, C6-contract), a fresh browser context that cannot reach off this
// machine (a spec in which the reader asks for any other host fails: egress.mjs), the
// reader loaded with `__mokuroInteropConfig` set before any page script runs, and its
// rig driver (driver.mjs) once `__mokuroInterop.apiVersion === 1`.
//
//   import { test, expect } from '../fixtures.mjs';
//   test.use({ needs: ['history'] });          // capabilities the spec needs (caps.mjs)
//   test('…', async ({ reader, rig }) => { await reader.connectWebDAV({ url: rig.bunkoUrl, … }) });
//
// Environment (unset when the suite has no such party): READER_URL, BUNKO_URL,
// CLOUDSIM_URL, CLOUDSIM_PREAUTH_URL, RCLONE_URL, RIG_PROVIDER (local|google|microsoft|
// mega|rclone), RIG_ACCOUNTS (a JSON file: [{username, password, role}] on the Bunko
// server), RIG_WORKDIR, RIG_FAKEPROC_HOLD (processor-fake holds each volume until this
// file exists), RIG_CONTEXT (JSON: {sim_account, mega?: {email, password},
// webdav?: {url, username, password}, library: {series, volumes}, lease?: {ttl_s},
// uploads?: {dir, files}}) and RIG_CONTROL_URL (the runner's control port, with a Bunko
// server: `rig.restartBunko`, `rig.stopBunko`, `rig.startBunko`).
import { test as base, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { gateCapabilities, parsePendingCaps } from './caps.mjs';
import { CLOUD_TEST_HOOKS, V1_METHODS, driver } from './driver.mjs';
import { STUBBED_SCRIPTS, refusedError, routeFor } from './egress.mjs';
import { takeViolations } from './violations.mjs';

export { expect };

const DEFAULT_PENDING = new URL('../../pending-reader-caps.txt', import.meta.url);

/** The parties, read from the environment once per worker. */
function rigFromEnv() {
    const env = process.env;
    /** `POST /bunko/<action>` on the runner's control port (control.rs). */
    async function control(action, body) {
        if (!env.RIG_CONTROL_URL) throw new Error('RIG_CONTROL_URL is not set: this suite runs no Bunko server');
        const r = await fetch(`${env.RIG_CONTROL_URL}/bunko/${action}`, {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(body),
        });
        const answer = await r.json().catch(() => null);
        if (!r.ok) throw new Error(`${action} Bunko: ${r.status} ${answer?.error ?? ''}`);
        return answer;
    }
    const accounts = env.RIG_ACCOUNTS ? JSON.parse(readFileSync(env.RIG_ACCOUNTS, 'utf8')) : [];
    const context = env.RIG_CONTEXT ? JSON.parse(env.RIG_CONTEXT) : {};
    return {
        readerUrl: env.READER_URL,
        bunkoUrl: env.BUNKO_URL,
        cloudsimUrl: env.CLOUDSIM_URL,
        preauthUrl: env.CLOUDSIM_PREAUTH_URL,
        rcloneUrl: env.RCLONE_URL,
        provider: env.RIG_PROVIDER,
        workdir: env.RIG_WORKDIR,
        fakeprocHold: env.RIG_FAKEPROC_HOLD,
        accounts,
        /** The cloud account Bunko and the reader share in cloudsim. */
        simAccount: context.sim_account ?? 'bunko',
        mega: context.mega,
        webdav: context.webdav,
        library: context.library,
        /** A Bunko server on the cloud: its lease timing (`{ttl_s}`, the store tune). */
        lease: context.lease,
        /**
         * An archive the rig made for the spec to import and upload (`uploads.files`:
         * `<Series>/<Volume>.cbz`), as the driver's `importVolume` takes it.
         */
        upload(rel) {
            if (!context.uploads?.files?.includes(rel)) {
                throw new Error(`no rig upload ${rel} (have ${context.uploads?.files?.join(', ') || 'none'})`);
            }
            return { path: rel, base64: readFileSync(path.join(context.uploads.dir, rel)).toString('base64') };
        },
        /** A Bunko account the rig created, by name. */
        account(name) {
            const a = accounts.find((x) => x.username === name);
            if (!a) throw new Error(`no rig account ${name} (have ${accounts.map((x) => x.username).join(', ')})`);
            return a;
        },
        /**
         * Stop the suite's Bunko server and start it again on the same data (the
         * runner's control port, crates/xtask/src/interop/rig/control.rs); resolves
         * once it serves HTTP, its lease still to settle. `{stale: true}` stays down
         * until its lease marker is past its ttl, so it claims the library afresh;
         * `{tune: {lease_settle_ms: n}}` sets store tune keys for the new process.
         */
        restartBunko: (options = {}) => control('restart', options),
        /**
         * Stop the suite's Bunko server and leave it down (SIGINT: it writes no
         * `released`, its marker just stops moving); resolves with `{stopped_at_ms}`.
         * `startBunko()` starts it again on the suite's own tune, once it serves HTTP.
         */
        stopBunko: () => control('stop', {}),
        startBunko: () => control('start', {}),
        /** cloudsim's control API (`/_sim/<path>`). */
        async sim(method, path, body) {
            if (!env.CLOUDSIM_URL) throw new Error('this suite runs no cloudsim');
            const r = await fetch(`${env.CLOUDSIM_URL}/_sim/${path}`, {
                method,
                headers: body ? { 'content-type': 'application/json' } : {},
                body: body ? JSON.stringify(body) : undefined,
            });
            const text = await r.text();
            if (!r.ok) throw new Error(`cloudsim ${method} /_sim/${path}: ${r.status} ${text}`);
            return text ? JSON.parse(text) : null;
        },
    };
}

/** `__mokuroInteropConfig` v1: cloudsim's bases, and a token for the OAuth provider. */
async function interopConfig(rig) {
    const config = { v: 1, gapiShim: true };
    if (!rig.cloudsimUrl) return config;
    const b = rig.cloudsimUrl;
    config.bases = {
        driveApi: `${b}/google`,
        driveUpload: `${b}/google/upload`,
        graph: `${b}/graph/v1.0`,
        megaGateway: `${b}/mega/`,
    };
    if (rig.provider === 'google') {
        const t = await rig.sim('POST', 'token', { provider: 'google', account: rig.simAccount });
        config.tokens = { google: { access_token: t.access_token, expires_in: t.expires_in } };
    } else if (rig.provider === 'microsoft') {
        const t = await rig.sim('POST', 'token', { provider: 'microsoft', account: rig.simAccount });
        config.tokens = { onedrive: t.access_token };
    }
    return config;
}

/**
 * What the `reader` fixture does, apart so the self-test sees its answer: load the
 * reader, wait for its v1 driver, and gate `needs` (caps.mjs). `{ driver }` when the
 * spec runs, `{ skip: [caps] }` when every missing capability is pending; throws when
 * one is missing that the pending list lacks.
 */
export async function openReader(page, { readerUrl, needs, pendingCapsFile }) {
    if (!readerUrl) throw new Error('READER_URL is not set: run the suite through `cargo xtask interop rig`');
    // Read first: a broken pending list fails every spec, never skips one.
    const pending = parsePendingCaps(readFileSync(pendingCapsFile, 'utf8'));
    await page.goto(readerUrl);
    try {
        await page.waitForFunction(() => globalThis.__mokuroInterop !== undefined, null, { timeout: 60_000 });
    } catch {
        throw new Error(
            `no __mokuroInterop at ${readerUrl}: the reader must be a \`vite build --mode interop\` build with the rig driver`,
        );
    }
    const api = await page.evaluate(() => globalThis.__mokuroInterop.apiVersion);
    if (api !== 1) throw new Error(`the reader's rig driver speaks API v${api}; this rig speaks v1`);
    // Every v1 method there before a spec starts: a reader that lost one fails here,
    // naming it, not halfway through a spec as "is not a function".
    const lacking = await page.evaluate(
        ([methods, hooks]) => [
            ...methods.filter((m) => typeof globalThis.__mokuroInterop[m] !== 'function'),
            ...hooks
                .filter((h) => typeof globalThis.__mokuroCloudTestHooks?.[h] !== 'function')
                .map((h) => `__mokuroCloudTestHooks.${h}`),
        ],
        [V1_METHODS, CLOUD_TEST_HOOKS],
    );
    if (lacking.length) throw new Error(`the reader's rig driver v1 lacks ${lacking.join(', ')}`);
    const have = await page.evaluate(() => [...globalThis.__mokuroInterop.capabilities]);
    const gate = gateCapabilities(needs, have, pending);
    if (gate.action === 'run') return { driver: driver(page) };
    if (gate.action === 'skip') return { skip: gate.missing };
    throw new Error(
        `the reader lacks capability ${gate.unlisted.join(', ')} (it has: ${have.join(', ') || 'none'}); ` +
            'list it in interop/pending-reader-caps.txt only while interop/reader.ref predates it',
    );
}

/**
 * Every request of `context` through the rule (egress.mjs), and `readerStub` (when set)
 * served at `readerUrl`.
 */
async function routeContext(context, { readerUrl, readerStub, refusedHosts, stubbedUrls }) {
    await context.route('**/*', (route) => {
        const href = route.request().url();
        const url = new URL(href);
        switch (routeFor(href)) {
            case 'continue':
                return route.continue();
            case 'stub':
                stubbedUrls.push(`${url.origin}${url.pathname}`);
                return route.fulfill({ status: 200, contentType: 'text/javascript', body: '' });
            default:
                refusedHosts.push(url.host);
                return route.abort('blockedbyclient');
        }
    });
    if (readerStub !== null) {
        // Registered last, so it wins over the catch-all above.
        await context.route(`${readerUrl}**`, (route) =>
            route.fulfill({ status: 200, contentType: 'text/html', body: readerStub }),
        );
    }
}

/**
 * The init script that sets `__mokuroInteropConfig` (`c`, interopConfig's) in a page.
 * Playwright runs a context's init scripts and a page's in no set order, so a spec's
 * own page script (a reconnect's new token) may have run first: what it set wins.
 */
export function installInteropConfig(c) {
    const set = globalThis.__mokuroInteropConfig;
    if (!set) {
        globalThis.__mokuroInteropConfig = c;
        return;
    }
    const merged = { ...c, ...set };
    for (const k of ['bases', 'tokens']) if (c[k] || set[k]) merged[k] = { ...c[k], ...set[k] };
    globalThis.__mokuroInteropConfig = merged;
}

/** `__mokuroInteropConfig` set in `context` before any page script runs. */
async function configure(context, rig) {
    await context.addInitScript(installInteropConfig, await interopConfig(rig));
}

export const test = base.extend({
    /** Capabilities the spec needs from the reader: `test.use({ needs: [...] })`. */
    needs: [[], { option: true }],
    /** The pending list (a file URL or path); the self-test points it elsewhere. */
    pendingCapsFile: [DEFAULT_PENDING, { option: true }],
    /** The reader's URL (READER_URL). */
    readerUrl: [process.env.READER_URL ?? '', { option: true }],
    /** Self-test only: HTML served at readerUrl in place of a reader. */
    readerStub: [null, { option: true }],

    rig: [async ({}, use) => use(rigFromEnv()), { scope: 'worker' }],

    /** Self-test only: false lets a spec that was refused a host pass (egress.mjs). */
    refusedFails: [true, { option: true }],

    /**
     * True: watch the requests instead of routing them, so the browser's own CORS
     * preflights reach the servers. While any route is on, Playwright answers every
     * preflight itself, permissively, and a server whose CORS a browser would refuse
     * passes. The rule is the same either way: the browser resolves no host off this
     * machine (playwright.config.mjs), a request for one still fails the spec, and the
     * Google scripts are still answered empty (by a CDP interception of those two URLs
     * alone, on the spec's page; a popup would not get them).
     */
    preflightsReachServers: [false, { option: true }],

    /** Hosts a request was refused for, in order (each spec's own). */
    refusedHosts: async ({}, use, testInfo) => {
        const hosts = [];
        await use(hosts);
        if (hosts.length) {
            testInfo.annotations.push({ type: 'refused-hosts', description: [...new Set(hosts)].join(' ') });
        }
    },

    /**
     * The Google scripts answered empty, in order (each spec's own): the only requests
     * off this machine a spec may make.
     */
    stubbedUrls: async ({}, use, testInfo) => {
        const urls = [];
        await use(urls);
        if (urls.length) {
            testInfo.annotations.push({ type: 'stubbed-urls', description: [...new Set(urls)].join(' ') });
        }
    },

    // Playwright's context is already one per test; this one refuses the network.
    context: async (
        { context, rig, readerUrl, readerStub, refusedHosts, stubbedUrls, refusedFails, preflightsReachServers },
        use,
        testInfo,
    ) => {
        const sim = (method, path) => rig.sim(method, path);
        // What cloudsim recorded since the last spec ended (the server's startup, its
        // polling in between) is no spec's doing: the results fail it on its own
        // ("cloudsim violations before <spec>", playwright.rs).
        if (rig.cloudsimUrl) {
            const before = await takeViolations(sim);
            if (before.length) {
                testInfo.annotations.push({ type: 'cloudsim-violations-between', description: JSON.stringify(before) });
            }
        }
        if (preflightsReachServers) {
            if (readerStub !== null)
                throw new Error('readerStub is served by a route; preflightsReachServers has none');
            // The resolver refuses the host; this records it (the stubs: `page` below).
            context.on('request', (request) => {
                const url = new URL(request.url());
                // blob: and data: name no host (a route never sees them either).
                if (!/^(https?|wss?):$/.test(url.protocol)) return;
                switch (routeFor(request.url())) {
                    case 'stub':
                        return stubbedUrls.push(`${url.origin}${url.pathname}`);
                    case 'refuse':
                        return refusedHosts.push(url.host);
                }
            });
        } else {
            await routeContext(context, { readerUrl, readerStub, refusedHosts, stubbedUrls });
        }
        await configure(context, rig);
        await use(context);
        const errors = [];
        // A request cloudsim could not accept is a defect of whoever sent it. One
        // recorded while the spec ran fails it, the reader's or the server's alike: the
        // server polls cloudsim all along, so the window, not the sender, is what is
        // known.
        if (rig.cloudsimUrl) {
            const during = await takeViolations(sim);
            if (during.length) errors.push(`cloudsim violations while the spec ran: ${JSON.stringify(during)}`);
        }
        const refused = refusedFails ? refusedError(refusedHosts) : null;
        if (refused) errors.push(refused);
        if (errors.length) throw new Error(errors.join('\n'));
    },

    /** Without the route (`preflightsReachServers`), the Google scripts are stubbed here. */
    page: async ({ page, context, preflightsReachServers }, use) => {
        if (preflightsReachServers) {
            const cdp = await context.newCDPSession(page);
            cdp.on('Fetch.requestPaused', (e) =>
                cdp
                    .send('Fetch.fulfillRequest', {
                        requestId: e.requestId,
                        responseCode: 200,
                        responseHeaders: [{ name: 'Content-Type', value: 'text/javascript' }],
                        body: '',
                    })
                    .catch(() => {}),
            );
            await cdp.send('Fetch.enable', {
                patterns: [...STUBBED_SCRIPTS].map((u) => ({ urlPattern: `${u}*`, requestStage: 'Request' })),
            });
        }
        await use(page);
    },

    /**
     * `await anotherDevice()`: the reader on one more device in the same spec, a browser
     * context of its own under the same rule (a host it is refused fails the spec), with
     * its own interop config (its own Drive or OneDrive token) and signed in to nothing.
     * Routed, so never with `preflightsReachServers`.
     */
    anotherDevice: async (
        {
            browser,
            context,
            rig,
            readerUrl,
            readerStub,
            refusedHosts,
            stubbedUrls,
            needs,
            pendingCapsFile,
            preflightsReachServers,
        },
        use,
        testInfo,
    ) => {
        // Set up after `context`, so torn down before it: its check sees what these were refused.
        void context;
        const made = [];
        await use(async () => {
            if (preflightsReachServers) {
                throw new Error('anotherDevice routes its requests; preflightsReachServers has no route');
            }
            const { serviceWorkers, viewport } = testInfo.project.use;
            const other = await browser.newContext({ serviceWorkers, viewport });
            made.push(other);
            await routeContext(other, { readerUrl, readerStub, refusedHosts, stubbedUrls });
            await configure(other, rig);
            const opened = await openReader(await other.newPage(), { readerUrl, needs, pendingCapsFile });
            // The spec's own reader was gated first: it ran, so this one has the same.
            if (opened.skip) throw new Error(`another device lacks ${opened.skip.join(', ')}`);
            return opened.driver;
        });
        for (const other of made) await other.close();
    },

    /** The reader, loaded, its capabilities checked against `needs`. */
    reader: async ({ page, readerUrl, needs, pendingCapsFile }, use, testInfo) => {
        const opened = await openReader(page, { readerUrl, needs, pendingCapsFile });
        if (opened.skip) {
            for (const cap of opened.skip) testInfo.annotations.push({ type: 'pending-reader-cap', description: cap });
            testInfo.skip(true, `pending reader capability: ${opened.skip.join(', ')}`);
        }
        await use(opened.driver);
    },
});
