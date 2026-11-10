// The rig driver API v1 (`globalThis.__mokuroInterop`, the reader's
// src/lib/interop/rig-driver.ts): one page.evaluate per method, nothing more. The
// reader calls the same stores and functions its UI calls; a spec never clicks.

/**
 * Every method of driver API v1, as the reader's `createRigDriver` has them: the
 * contract's first twenty, then what the rig's suites added before any reader was pinned
 * (`interop/reader.ref`). The set is v1 from the first pin on: a method added after it
 * comes with `apiVersion` 2. The fixtures check them all before a spec runs.
 */
export const V1_METHODS = [
    'connectWebDAV',
    'connectMega',
    'connectDrive',
    'connectOneDrive',
    'logout',
    'sync',
    'library',
    'open',
    'setPage',
    'progress',
    'renameVolume',
    'renameSeries',
    'deleteVolume',
    'setSpineOffset',
    'editFirstBlock',
    'publishCurated',
    'selectLayer',
    'providerStatus',
    'controlState',
    'historyStatus',
    // The dav and api suites.
    'showSeries',
    'pendingOcr',
    // The direct suite.
    'idle',
    // The shared suite.
    'importVolume',
    'backupVolume',
    // S8, and every count of metadata writes.
    'settleSeriesWrites',
];

/**
 * The reader's cloud test hooks (`src/lib/util/sync/cloud-test-hooks.ts`) a spec calls
 * beside the driver: a token re-minted after the page loaded (`__mokuroInteropConfig`
 * is read once, at load) is handed over with `setCloudTestToken(provider, token)`.
 */
export const CLOUD_TEST_HOOKS = ['setCloudTestBases', 'setCloudTestToken'];

/** Call `__mokuroInterop[name](...args)` in the page; a method the reader lacks says so. */
function call(page, name, args) {
    return page.evaluate(
        ([n, a]) => {
            const api = globalThis.__mokuroInterop;
            if (typeof api?.[n] !== 'function') throw new Error(`__mokuroInterop.${n} is not a function`);
            return api[n](...a);
        },
        [name, args],
    );
}

/** The reader's driver, of a volume id it has neither a row nor a placeholder for. */
const NOT_ON_DEVICE = /No volume \S+ on this device/;

/**
 * `call` for the two methods that look up a volume which may not be installed (`open`,
 * `pendingOcr`), asked again, once, when the reader answers that it has no such volume.
 *
 * The driver of `interop/reader.ref` makes that lookup in two reads of its `volumes`
 * table: the row by id, then every row, to work out the placeholders. The app makes a
 * listed volume a row by itself about 100 ms after its catalog first shows it (the
 * cover service's batch), about when a spec that has just connected opens it. A row
 * landing between the two reads is missed by the first and hides the placeholder from
 * the second, so the driver has no volume for an id `library()` gave a moment before.
 * Asked again, the first read finds the row. Both methods answer this before they do
 * anything, so the second ask repeats nothing; a volume that is not there fails it too,
 * with the same words. No user can meet it (a card downloads the volume it shows, with
 * no lookup by id). It goes once `reader.ref` pins a driver that reads the table once.
 */
async function lookUp(page, name, args) {
    try {
        return await call(page, name, args);
    } catch (e) {
        if (!NOT_ON_DEVICE.test(e?.message ?? '')) throw e;
        return call(page, name, args);
    }
}

export function driver(page) {
    const m =
        (name) =>
        (...args) =>
            call(page, name, args);
    const lookedUp =
        (name) =>
        (...args) =>
            lookUp(page, name, args);
    return {
        page,
        apiVersion: () => page.evaluate(() => globalThis.__mokuroInterop.apiVersion),
        /** @returns {Promise<string[]>} */
        capabilities: () => page.evaluate(() => [...globalThis.__mokuroInterop.capabilities]),
        /** ({url, username?, password?}) */
        connectWebDAV: m('connectWebDAV'),
        /** ({email, password}) */
        connectMega: m('connectMega'),
        /** Drive with the token from __mokuroInteropConfig. */
        connectDrive: m('connectDrive'),
        /** OneDrive with the token from __mokuroInteropConfig. */
        connectOneDrive: m('connectOneDrive'),
        logout: m('logout'),
        /** @returns {Promise<{ok: boolean, errors: string[]}>} */
        sync: m('sync'),
        /** @returns {Promise<{volumes: {id, series, title, cloudPath, hasOcr, layers: string[]}[]}>} */
        library: m('library'),
        /** (volumeId) → {id}: one not on the device is downloaded first (its card's Download). */
        open: lookedUp('open'),
        /** (volumeId, page) */
        setPage: m('setPage'),
        /** (volumeId) → {page, completed} */
        progress: m('progress'),
        /** (volumeId, title) */
        renameVolume: m('renameVolume'),
        /** (series, title) */
        renameSeries: m('renameSeries'),
        /** (volumeId) */
        deleteVolume: m('deleteVolume'),
        /** (series, n) */
        setSpineOffset: m('setSpineOffset'),
        /** (volumeId, page, text) */
        editFirstBlock: m('editFirstBlock'),
        /** (volumeId) */
        publishCurated: m('publishCurated'),
        /** (volumeId, layerId) */
        selectLayer: m('selectLayer'),
        providerStatus: m('providerStatus'),
        /** → {marker, serverCompilesMetadata, held} */
        controlState: m('controlState'),
        /** → {enabled, devices} */
        historyStatus: m('historyStatus'),
        /** (series) the catalog card's click: the series page, its volume cards shown. */
        showSeries: m('showSeries'),
        /** (volumeId) → what the volume's card shows of the server's OCR queue, or null. */
        pendingOcr: lookedUp('pendingOcr'),
        /** Resolves once no sync and no cloud listing is in flight (the app's own, too). */
        idle: m('idle'),
        /**
         * ({path, base64}) → {id}: the upload modal's import of `<Series>/<Volume>.cbz`
         * (`rig.upload(rel)` gives both), an image-only archive's review answered
         * keeping its names; resolves once the volume is installed.
         */
        importVolume: m('importVolume'),
        /**
         * (volumeId) → {cloudPath, error?}: the card's Backup, to the end of its run
         * (the upload, then the run's listing and its series.json/catalog.json writes).
         */
        backupVolume: m('backupVolume'),
        /**
         * Every series.json write the reader has queued (its 2 s debounce cut short) or
         * in flight, done: what the syncs before it asked to write has been written.
         */
        settleSeriesWrites: m('settleSeriesWrites'),
    };
}
