// The fixtures' pure parts: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { installInteropConfig } from './fixtures.mjs';

const CONFIG = {
    v: 1,
    gapiShim: true,
    bases: { graph: 'http://127.0.0.1:9/graph/v1.0' },
    tokens: { onedrive: 'revoked' },
};

/** The context's init script, handed its own copy as Playwright hands one to each page. */
const contextScript = () => installInteropConfig(structuredClone(CONFIG));

/** What a spec's page init script does to hand the next load a new token. */
function pageScript(t) {
    const config = (globalThis.__mokuroInteropConfig ??= { v: 1 });
    (config.tokens ??= {}).onedrive = t;
}

test("a page's own token wins whichever init script runs first", () => {
    for (const order of [
        [contextScript, () => pageScript('fresh')],
        [() => pageScript('fresh'), contextScript],
    ]) {
        delete globalThis.__mokuroInteropConfig;
        for (const run of order) run();
        assert.deepEqual(globalThis.__mokuroInteropConfig, { ...CONFIG, tokens: { onedrive: 'fresh' } });
    }
    delete globalThis.__mokuroInteropConfig;
});

test('alone, the config is the one the rig made', () => {
    delete globalThis.__mokuroInteropConfig;
    contextScript();
    assert.deepEqual(globalThis.__mokuroInteropConfig, CONFIG);
    delete globalThis.__mokuroInteropConfig;
});
