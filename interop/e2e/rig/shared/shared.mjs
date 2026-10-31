// What the shared specs share: the reader signed in to the cloud Bunko holds (as the
// direct suite signs in, cloud-direct.mjs), Bunko's lease marker as the reader judges
// it, what the cloud stores of a file, and every file the reader itself wrote to the
// cloud. Bunko writes to the same folder all along, so "the reader wrote X" is read off
// the browser's own requests, never off the cloud's files.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { expect } from '../fixtures.mjs';
import { LIBRARY_FOLDER, cloudFile, signIn } from '../cloud-direct.mjs';

export { recordReaderWrites } from '../reader-writes.mjs';

export async function connect(reader, rig) {
    const { sync } = await signIn(reader, rig);
    expect(sync).toEqual({ ok: true, errors: [] });
}

/**
 * The tab hidden and shown again, as switching away and back does: the reader's cloud
 * queue poller takes one look then (its marker read included), where it would
 * otherwise look every 60 s (30 s on WebDAV).
 */
export async function tabAwayAndBack(page) {
    await page.evaluate(() => {
        const set = (hidden) => {
            Object.defineProperty(document, 'visibilityState', {
                configurable: true,
                get: () => (hidden ? 'hidden' : 'visible'),
            });
            Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
            document.dispatchEvent(new Event('visibilitychange'));
        };
        set(true);
        set(false);
    });
}

/**
 * Wait until the reader finds the Bunko marker fresh, then until it has SEEN the marker
 * move (a heartbeat after its first look): before that a reader judges it by the
 * marker's own clock (ttl + 5 min), after it by what it observed (2 x ttl). The holder.
 */
export async function bunkoSeenAlive(reader) {
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                return reader.controlState();
            },
            { timeout: 120_000, intervals: [1000, 2000] },
        )
        .toMatchObject({ serverCompilesMetadata: true, marker: { fresh: true } });
    const first = (await reader.controlState()).marker.holder.heartbeatAt;
    await expect
        .poll(
            async () => {
                await tabAwayAndBack(reader.page);
                return (await reader.controlState()).marker?.holder?.heartbeatAt ?? null;
            },
            { timeout: 120_000, intervals: [1000, 2000] },
        )
        .not.toBe(first);
    return (await reader.controlState()).marker.holder;
}

/** A file's bytes under the library folder as the cloud stores them, or null. */
export async function cloudBytes(rig, rel) {
    if (rig.provider === 'rclone') {
        try {
            return readFileSync(path.join(rig.workdir, 'rclone-root', LIBRARY_FOLDER, rel));
        } catch {
            return null;
        }
    }
    const f = await cloudFile(rig, rel);
    if (!f) return null;
    const r = await fetch(
        `${rig.cloudsimUrl}/_sim/content?provider=${rig.provider}&account=${encodeURIComponent(rig.simAccount)}&id=${encodeURIComponent(f.id)}`,
    );
    if (r.status === 404) return null;
    if (!r.ok) throw new Error(`cloudsim content of ${rel}: ${r.status} ${await r.text()}`);
    return Buffer.from(await r.arrayBuffer());
}

/** A JSON file under the library folder, parsed, or null. */
export async function cloudJson(rig, rel) {
    const bytes = await cloudBytes(rig, rel);
    return bytes ? JSON.parse(bytes.toString('utf8').replace(/^﻿/, '')) : null;
}

/** The volume of the seeded series, or of `series`, titled `title`, as the reader lists it. */
export async function volumeNamed(reader, series, title) {
    const { volumes } = await reader.library();
    const v = volumes.find((x) => x.series === series && x.title === title);
    expect(v, `${series}/${title} in the library`).toBeTruthy();
    return v;
}
