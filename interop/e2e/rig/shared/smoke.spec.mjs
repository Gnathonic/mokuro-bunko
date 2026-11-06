// The harness end to end with Bunko and the reader on one cloud folder: the reader signs
// in to the cloud directly (cloud-direct.mjs), lists the library the rig seeded, and
// finds the lease marker of the Bunko that holds it fresh.
import { test, expect } from '../fixtures.mjs';
import { signIn } from '../cloud-direct.mjs';
import { bunkoSeenAlive } from './shared.mjs';

test('connect, list, and see the Bunko that holds the library', async ({ reader, rig }) => {
    const { sync } = await signIn(reader, rig);
    expect(sync).toEqual({ ok: true, errors: [] });
    const { volumes } = await reader.library();
    const listed = volumes.filter((v) => v.series === rig.library.series);
    // The seeded volumes (an earlier spec may have uploaded more).
    expect(listed.map((v) => v.title)).toEqual(expect.arrayContaining(rig.library.volumes));
    const holder = await bunkoSeenAlive(reader);
    expect(holder).toMatchObject({ bunkoVersion: expect.stringMatching(/^\d+\.\d+\.\d+/) });
});
