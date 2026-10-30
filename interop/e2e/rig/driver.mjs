// The rig driver API v1 (`globalThis.__mokuroInterop`, the reader's
// src/lib/interop/rig-driver.ts): one page.evaluate per method, nothing more. The
// reader calls the same stores and functions its UI calls; a spec never clicks.

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

export function driver(page) {
    const m =
        (name) =>
        (...args) =>
            call(page, name, args);
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
        open: m('open'),
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
        pendingOcr: m('pendingOcr'),
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
