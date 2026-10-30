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
// file exists), and RIG_CONTEXT (JSON: {sim_account, mega?: {email, password},
// webdav?: {url, username, password}, library: {series, volumes}}).
import { test as base, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { gateCapabilities, parsePendingCaps } from './caps.mjs';
import { driver } from './driver.mjs';
import { refusedError, routeFor } from './egress.mjs';
import { takeViolations } from './violations.mjs';

export { expect };

const DEFAULT_PENDING = new URL('../../pending-reader-caps.txt', import.meta.url);

/** The parties, read from the environment once per worker. */
function rigFromEnv() {
    const env = process.env;
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
        /** A Bunko account the rig created, by name. */
        account(name) {
            const a = accounts.find((x) => x.username === name);
            if (!a) throw new Error(`no rig account ${name} (have ${accounts.map((x) => x.username).join(', ')})`);
            return a;
        },
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
    const have = await page.evaluate(() => [...globalThis.__mokuroInterop.capabilities]);
    const gate = gateCapabilities(needs, have, pending);
    if (gate.action === 'run') return { driver: driver(page) };
    if (gate.action === 'skip') return { skip: gate.missing };
    throw new Error(
        `the reader lacks capability ${gate.unlisted.join(', ')} (it has: ${have.join(', ') || 'none'}); ` +
            'list it in interop/pending-reader-caps.txt only while interop/reader.ref predates it',
    );
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
        { context, rig, readerUrl, readerStub, refusedHosts, stubbedUrls, refusedFails },
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
        const config = await interopConfig(rig);
        await context.addInitScript((c) => {
            globalThis.__mokuroInteropConfig = c;
        }, config);
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
