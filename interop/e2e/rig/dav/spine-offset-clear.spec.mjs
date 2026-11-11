// S8 (B8.6, plan X15, B7.1 + R4.1): a spine offset cleared on the reader is cleared on
// Bunko. The reader sets the series' spine offset to 3, so Bunko's series.json says
// `spine_offset: 3`; it resets it to 0, so series.json carries no override at all; and
// the two syncs after that PUT series.json not once (the reset is not re-sent forever
// as a request the server reads as "keep the stored value").
import { test, expect } from '../fixtures.mjs';
import { basic, bunko, davPath, recordRequests } from '../bunko.mjs';

test.use({ preflightsReachServers: true });

const PASSWORD = 'rig-spine-1';

test('a spine offset set, then cleared, leaves series.json without one and is not re-sent', async ({
    reader,
    rig,
    page,
}) => {
    test.setTimeout(180_000);
    const server = bunko(rig);
    const series = rig.library.series;
    const seriesJson = davPath(series, 'series.json');
    // An editor: series metadata (facts and the shelf alignment) is its to change.
    const account = await server.createUser(`spine-${Date.now().toString(36)}`, PASSWORD, 'editor');
    const auth = basic(account.username, PASSWORD);
    const published = async () => {
        const r = await server.get(seriesJson, auth);
        return r.status === 200 ? JSON.parse(r.text) : null;
    };

    const { sync } = await reader.connectWebDAV({ url: rig.bunkoUrl, username: account.username, password: PASSWORD });
    expect(sync).toEqual({ ok: true, errors: [] });
    // Bunko compiled the series' file before anyone asked for an offset.
    await expect.poll(published, { timeout: 60_000 }).not.toBeNull();
    expect(await published()).not.toHaveProperty('spine_offset');
    // A volume of the series on this device, as on the shelf the offset is set from:
    // the reader writes series.json only for a series it holds something of.
    const { volumes } = await reader.library();
    const vol1 = volumes.find((v) => v.series === series && v.title === 'Vol 1');
    expect(vol1, `${series}/Vol 1 in the library`).toBeTruthy();
    await reader.open(vol1.id);

    // recordRequests answers paths decoded.
    const putPath = `/mokuro-reader/${series}/series.json`;
    const requests = recordRequests(page, new URL(rig.bunkoUrl).origin);
    const seriesPuts = async () =>
        (await requests.take()).filter((r) => r.method === 'PUT' && r.path === putPath).length;

    await reader.setSpineOffset(series, 3);
    await expect.poll(async () => (await published())?.spine_offset, { timeout: 60_000 }).toBe(3);
    expect(await seriesPuts(), 'the offset went up as a series.json PUT').toBeGreaterThan(0);

    await reader.setSpineOffset(series, 0);
    await expect
        .poll(async () => Object.hasOwn((await published()) ?? { spine_offset: null }, 'spine_offset'), {
            timeout: 60_000,
        })
        .toBe(false);
    expect(await seriesPuts(), 'the reset went up as a series.json PUT').toBeGreaterThan(0);

    // Two more syncs, each to its end with every series.json write it queued.
    for (let i = 0; i < 2; i++) {
        expect(await reader.sync()).toEqual({ ok: true, errors: [] });
        await reader.idle();
        await reader.settleSeriesWrites();
    }
    expect(await seriesPuts(), 'series.json PUTs on the two syncs after the reset').toBe(0);
    expect(await published()).not.toHaveProperty('spine_offset');
});
