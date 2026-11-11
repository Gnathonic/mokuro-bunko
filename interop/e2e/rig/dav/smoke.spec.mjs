// The harness end to end: the reader connects to Bunko's WebDAV as a seeded account
// and lists the library the rig seeded (browser.rs LIBRARY_*).
import { test, expect } from '../fixtures.mjs';

test('connect and list', async ({ reader, rig }) => {
    const { username, password } = rig.account('alice');
    await reader.connectWebDAV({ url: rig.bunkoUrl, username, password });
    // The server may still be indexing its library when the first sync runs.
    await expect
        .poll(
            async () => {
                const sync = await reader.sync();
                expect(sync.errors).toEqual([]);
                expect(sync.ok).toBe(true);
                const { volumes } = await reader.library();
                return volumes
                    .filter((v) => v.series === rig.library.series)
                    .map((v) => v.title)
                    .sort();
            },
            { timeout: 60_000, intervals: [500, 1000, 2000] },
        )
        .toEqual([...rig.library.volumes].sort());
});
