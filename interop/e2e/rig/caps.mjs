// Reader capabilities and the pending list (interop/pending-reader-caps.txt), kept
// pure so the gate is tested without a browser (caps.test.mjs).
//
// A rig spec names the capabilities it needs. One the reader's driver does not list
// FAILS the spec, so a reader that lost a feature cannot pass quietly. A Bunko change
// that needs a newer reader than interop/reader.ref lists the capability in the pending
// file until reader.ref is bumped; a spec needing only listed capabilities is skipped,
// and the results JSON reports the skip with its capability.

/** A capability name as the driver lists them (`edit-layers-personal`). */
export const CAPABILITY = /^[a-z0-9][a-z0-9-]*$/;

/** The pending file's capabilities: one per line, `#` starts a comment. */
export function parsePendingCaps(text) {
    const caps = [];
    text.split('\n').forEach((raw, i) => {
        const line = raw.replace(/#.*/, '').trim();
        if (!line) return;
        if (!CAPABILITY.test(line)) {
            throw new Error(`pending-reader-caps.txt line ${i + 1}: ${JSON.stringify(line)} is not a capability name`);
        }
        if (caps.includes(line)) {
            throw new Error(`pending-reader-caps.txt line ${i + 1}: ${line} is listed twice`);
        }
        caps.push(line);
    });
    return caps;
}

/**
 * `run` when the reader has everything in `needs`; `skip` when every missing one is
 * pending; otherwise `fail`, naming the missing capabilities the pending list lacks.
 */
export function gateCapabilities(needs, have, pending) {
    const missing = needs.filter((c) => !have.includes(c));
    const unlisted = missing.filter((c) => !pending.includes(c));
    const action = missing.length === 0 ? 'run' : unlisted.length === 0 ? 'skip' : 'fail';
    return { action, missing, unlisted };
}
