// The harness end to end through Bunko on a cloud library: the reader signs in to the
// server (which holds the cloud's lease; only it talks to the cloud) and lists the
// library the rig seeded in the cloud (browser.rs LIBRARY_*).
import { test, expect } from '../fixtures.mjs';

test('connect and list', async ({ reader, rig }) => {
    const { username, password } = rig.account('alice');
    await reader.connectWebDAV({ url: rig.bunkoUrl, username, password });
    // The server may still be indexing the cloud library when the first sync runs.
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
