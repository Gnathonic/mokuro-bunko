// The reader alone on a cloud (the direct suite, §7.1): its sign-in on each provider,
// the files the cloud holds under the library folder, and how each cloud answers the
// reader's next write of a file with a fault. cloudsim answers Drive and Graph faults
// with `/_sim/fault` and MEGA's with its error-code knobs; rclone has no fault knobs,
// so its answers are made at the browser's door (`page.route`) for that one request.
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';

/** The folder the reader keeps its library in, at the top of the cloud. */
export const LIBRARY_FOLDER = 'mokuro-reader';

/** The reader's sign-in with what the rig set up (fixtures.mjs `rig`). */
export function signIn(reader, rig) {
    switch (rig.provider) {
        case 'google':
            return reader.connectDrive();
        case 'microsoft':
            return reader.connectOneDrive();
        case 'mega':
            return reader.connectMega(rig.mega);
        case 'rclone':
            return reader.connectWebDAV(rig.webdav);
        default:
            throw new Error(`the direct suite has no sign-in for ${rig.provider}`);
    }
}

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** Every file below `dir` on disk, as `[path relative to it, bytes]`. */
function walk(dir, rel = '') {
    const out = [];
    for (const e of readdirSync(path.join(dir, rel), { withFileTypes: true })) {
        const r = rel ? `${rel}/${e.name}` : e.name;
        if (e.isDirectory()) out.push(...walk(dir, r));
        else out.push([r, readFileSync(path.join(dir, r))]);
    }
    return out;
}

/**
 * The cloud's files under the library folder, as it stores them: `[{path, id, size,
 * sha256, modified}]`, `path` relative to the folder (`Rig Series/Vol 1.cbz`). cloudsim's
 * tree (never in its request log), or rclone's served directory (no `modified`: one
 * file per path on a disk).
 */
export async function cloudFiles(rig) {
    if (rig.provider === 'rclone') {
        const root = path.join(rig.workdir, 'rclone-root', LIBRARY_FOLDER);
        return walk(root).map(([p, bytes]) => ({ path: p, id: p, size: bytes.length, sha256: sha256(bytes) }));
    }
    const rows = await rig.sim('GET', `tree?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}`);
    const byId = new Map(rows.map((r) => [r.id, r]));
    const out = [];
    for (const r of rows) {
        if (r.folder) continue;
        const parts = [];
        for (let n = r; n; n = byId.get(n.parent_id)) parts.unshift(n.name);
        if (parts.length < 2 || parts[0] !== LIBRARY_FOLDER) continue;
        out.push({ path: parts.slice(1).join('/'), id: r.id, size: r.size, sha256: r.sha256, modified: r.modified });
    }
    return out;
}

/**
 * One file under the library folder (`cloudFiles`), or null. Of several at one path,
 * the one written last: a merge of two `mokuro-reader` roots keeps both copies of a
 * control file (`.mokuro-queue.json`, `volume-data.json`), and its writer goes on
 * writing one of them while the other stays as the merge left it.
 */
export async function cloudFile(rig, rel) {
    const at = (f) => Date.parse(f.modified ?? '') || 0;
    const copies = (await cloudFiles(rig)).filter((f) => f.path === rel);
    return copies.reduce((last, f) => (last === null || at(f) > at(last) ? f : last), null);
}

/**
 * How this cloud answers the reader's NEXT write of `rel` (a file under the library
 * folder it already holds), once: `quota` (a full account), `rateLimit` (a rate limit
 * with Retry-After: 1) and `conflict` (412). A method is null where the cloud has no
 * such answer: MEGA has no conditional write to conflict with, and the reader's WebDAV
 * writes have no Retry-After rule to wait by (rclone never rate-limits). On rclone the
 * answer is made at the browser's door for `page`.
 */
export function writeFaults(rig, page) {
    const account = rig.simAccount;
    switch (rig.provider) {
        case 'google': {
            // A file the reader replaces is a PATCH of its id (a resumable session).
            const fault = async (rel, status, reason, extra = {}) => {
                const f = await cloudFile(rig, rel);
                if (!f) throw new Error(`${rel} is not in the cloud to fault its write`);
                await rig.sim('POST', 'fault', {
                    provider: 'google',
                    account,
                    method: 'PATCH',
                    path_has: `/upload/drive/v3/files/${f.id}`,
                    status,
                    reason,
                    ...extra,
                });
            };
            return {
                quota: (rel) => fault(rel, 403, 'storageQuotaExceeded'),
                rateLimit: (rel) => fault(rel, 429, 'rateLimitExceeded', { retry_after: 1 }),
                conflict: (rel) => fault(rel, 412, 'conditionNotMet'),
            };
        }
        case 'microsoft': {
            // Every write is an upload session created at the file's path.
            const fault = (rel, status, reason, extra = {}) =>
                rig.sim('POST', 'fault', {
                    provider: 'microsoft',
                    account,
                    method: 'POST',
                    path_has: `/${LIBRARY_FOLDER}/${rel}:/createUploadSession`,
                    status,
                    reason,
                    ...extra,
                });
            return {
                // Graph answers a full drive 507, never 403 (R2.1).
                quota: (rel) => fault(rel, 507, 'insufficientStorage'),
                rateLimit: (rel) => fault(rel, 429, 'activityLimitReached', { retry_after: 1 }),
                conflict: (rel) => fault(rel, 412, 'preconditionFailed'),
            };
        }
        case 'mega': {
            // Codes are per command, not per file: `u` (an upload's URL) starts every
            // upload, and the progress file is the first a sync uploads.
            const knobs = (k) => rig.sim('POST', 'knobs', { provider: 'mega', account, ...k });
            return {
                quota: () => knobs({ fail_command: { command: 'u', code: -17 } }),
                // EAGAIN: MEGA's "busy, try again" for the next two commands.
                rateLimit: () => knobs({ eagain: 2 }),
                conflict: null,
            };
        }
        case 'rclone': {
            const door = async (rel, status) => {
                const want = `/${LIBRARY_FOLDER}/${rel}`;
                let used = false;
                await page.route(
                    (url) => decodeURIComponent(url.pathname) === want,
                    (route) => {
                        if (used || route.request().method() !== 'PUT') return route.fallback();
                        used = true;
                        return route.fulfill({ status, body: '' });
                    },
                );
            };
            return {
                // WebDAV's full disk (RFC 4918 §11.5).
                quota: (rel) => door(rel, 507),
                rateLimit: null,
                conflict: (rel) => door(rel, 412),
            };
        }
        default:
            throw new Error(`no faults for ${rig.provider}`);
    }
}
