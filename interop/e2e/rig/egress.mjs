// What the rig's browser may reach (fixtures.mjs routes every request through here).
// Kept apart so the rule is tested without a browser (egress.test.mjs).
//
// Nothing leaves this machine: a request to a loopback host goes through, the two
// Google scripts src/app.html loads even in an interop build are answered with an empty
// script (the reader installs its gapi REST shim and takes its Google token from
// `__mokuroInteropConfig` instead), and every other request is refused. A refused
// request is still a defect of the reader's: an interop build that reaches for a host
// off the machine would need the network on CI, or talks to a real service in place of
// the simulated one. So a spec that was refused one fails (refusedError).

const LOCAL_HOSTS = new Set(['127.0.0.1', 'localhost', '[::1]']);

export const STUBBED_SCRIPTS = new Set(['https://apis.google.com/js/api.js', 'https://accounts.google.com/gsi/client']);

/** `continue`, `stub` (an empty 200 script) or `refuse`, for a request's URL. */
export function routeFor(href) {
    const url = new URL(href);
    if (LOCAL_HOSTS.has(url.hostname)) return 'continue';
    if (STUBBED_SCRIPTS.has(`${url.origin}${url.pathname}`)) return 'stub';
    return 'refuse';
}

/** The error a spec fails with when it was refused `hosts`, or null when it was not. */
export function refusedError(hosts) {
    if (!hosts.length) return null;
    return `the reader asked for hosts off this machine (refused): ${[...new Set(hosts)].join(', ')}`;
}
