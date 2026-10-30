// The harness end to end on a cloud, the reader alone: it signs in with what the rig set
// up (the Drive or OneDrive token in `__mokuroInteropConfig`, cloudsim's MEGA account,
// rclone's WebDAV) and lists the library the rig seeded (browser.rs LIBRARY_*).
import { test, expect } from '../fixtures.mjs';

function connect(reader, rig) {
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

test('connect and list', async ({ reader, rig }) => {
    await connect(reader, rig);
    const sync = await reader.sync();
    expect(sync.errors).toEqual([]);
    expect(sync.ok).toBe(true);
    const { volumes } = await reader.library();
    const listed = volumes.filter((v) => v.series === rig.library.series);
    expect(listed.map((v) => v.title).sort()).toEqual([...rig.library.volumes].sort());
    // Listed from the cloud, not installed: nothing was downloaded.
    for (const v of listed) {
        expect(v.cloudPath).toBe(`${rig.library.series}/${v.title}.cbz`);
        expect(v.hasOcr).toBe(false);
    }
});
