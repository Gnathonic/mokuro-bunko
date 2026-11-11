// cloudsim's violations, taken for a spec (fixtures.mjs). Kept apart so the rule is
// tested without a browser or a cloudsim (violations.test.mjs).

/**
 * Every violation so far, cleared in the same call: `DELETE /_sim/violations` answers
 * what it cleared. A read then a clear would wipe, unseen, one recorded between them,
 * and Bunko polls cloudsim all along. `sim` is the rig's `sim(method, path)`.
 */
export async function takeViolations(sim) {
    const taken = await sim('DELETE', 'violations');
    // A cloudsim older than this rig clears them and answers nothing.
    if (!Array.isArray(taken)) {
        const got = JSON.stringify(taken);
        throw new Error(`cloudsim's DELETE /_sim/violations answered ${got}, not the list it cleared`);
    }
    return taken;
}
