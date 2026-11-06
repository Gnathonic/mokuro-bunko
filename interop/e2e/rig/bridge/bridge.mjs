// What the bridge specs share: a reader signed in to Bunko (which alone talks to the
// cloud), the seeded volume found in its library, and the cloud's copy of a file
// compared with what Bunko serves of it.
import { createHash } from 'node:crypto';
import { expect } from '../fixtures.mjs';
import { basic, bunko, davPath } from '../bunko.mjs';
import { cloudFile } from '../cloud-direct.mjs';

export const PASSWORD = 'rig-bridge-1';

export const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** A new account of `role` on the server, so a spec's sign-ins stay its own. */
export function newAccount(rig, prefix, role = 'editor') {
    return bunko(rig).createUser(`${prefix}-${Date.now().toString(36)}`, PASSWORD, role);
}

export async function connect(reader, rig, account) {
    const { sync } = await reader.connectWebDAV({
        url: rig.bunkoUrl,
        username: account.username,
        password: account.password,
    });
    expect(sync).toEqual({ ok: true, errors: [] });
}

export async function volumeNamed(reader, rig, title) {
    const { volumes } = await reader.library();
    const v = volumes.find((x) => x.series === rig.library.series && x.title === title);
    expect(v, `${rig.library.series}/${title} in the library`).toBeTruthy();
    return v;
}

/** Signed in as `account`, `Vol 1` downloaded and open: its id. */
export async function openVolume(reader, rig, account, title = 'Vol 1') {
    await connect(reader, rig, account);
    const { id } = await reader.open((await volumeNamed(reader, rig, title)).id);
    return id;
}

/**
 * `rel` (below the library folder) as Bunko serves it to `account` and as the cloud
 * stores it: `{served: {status, bytes}, cloud: {sha256, …} | null}`.
 */
export async function servedAndStored(rig, account, rel) {
    const served = await bunko(rig).bytes(davPath(...rel.split('/')), basic(account.username, account.password));
    return { served, cloud: await cloudFile(rig, rel) };
}

/**
 * Wait until the cloud holds `rel` as Bunko serves it, and Bunko serves what `check`
 * accepts (the parsed JSON): the parsed file.
 */
export async function landedInCloud(rig, account, rel, check, timeout = 60_000) {
    let doc = null;
    await expect
        .poll(
            async () => {
                const { served, cloud } = await servedAndStored(rig, account, rel);
                if (served.status !== 200 || !cloud)
                    return `served ${served.status}, cloud ${cloud ? 'has it' : 'none'}`;
                doc = JSON.parse(served.bytes.toString('utf8'));
                if (!check(doc)) return 'served, not the bytes the reader wrote';
                return sha256(served.bytes) === cloud.sha256 ? 'landed' : 'the cloud holds other bytes';
            },
            { timeout, intervals: [250, 500, 1000] },
        )
        .toBe('landed');
    return doc;
}

/** The first block's text of a `.mokuro` document's first page. */
export const firstLines = (doc) => doc?.pages?.[0]?.blocks?.[0]?.lines ?? null;

/** The server's lease as its Storage panel shows it, or null while it does not answer. */
export async function leaseNow(rig) {
    try {
        return (await bunko(rig).storage()).lease ?? null;
    } catch {
        return null;
    }
}

/** The origins the reader must never reach in this suite: the cloud's own. */
export function cloudOrigins(rig) {
    return [rig.cloudsimUrl, rig.preauthUrl, rig.rcloneUrl].filter(Boolean).map((u) => new URL(u).origin);
}
