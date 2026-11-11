# Cloud-direct Bunko in Mokuro Reader: current behaviour and required changes

All paths are under `mokuro-reader/src/lib/` unless stated. I read the code and did not run it.

## 1. Current behaviour

**Queue poller (`catalog/server-ocr-queue.ts`)**
- The poller is HTTP-only. `QueuePoller.read` (L295-309) does `fetch(queueUrl, {cache:'no-store', If-None-Match})`, expects 200 or 304, and parses with `parseQueueFile` (L102, v1 schema).
- `queueUrlForWebdav` (L821) builds `<server>/mokuro-reader/.mokuro-queue.json`.
- `connectedBunkoQueueUrl` (L841) returns a URL only when the active provider is `webdav` and `webdavPutVerified === true` (the `X-Mokuro-Put` header was seen). Drive, MEGA and OneDrive return null.
- `startServerOcrQueue` (L878, called from `routes/+layout.svelte:118`) polls only watched servers plus the connected bunko server.
- Entries with a `manifest` URL are handled in `pullCompletedVolume` (L703-757):
  - It loads the manifest through `loadVolumeManifest` using `fetchWithQueueAuth`, which attaches an Authorization header only for the queue's own origin (L589).
  - If `manifest.pending` is non-empty, it returns `PendingOnServer` with `recheck_after`. The poller keeps that in `offList` (L187, `settleDone` L354).
  - Otherwise `pullManifestFiles` (L656) downloads the primary through `upgradeOcrFromSidecarBlob` and layers through `importFetchedLayers`, by URL.
- Interest filter (`evaluate` L311):
  - A volume is "of interest" if it is watched or `isVolumeShown`.
  - Watches are created by `watchUploadedVolume` (L531, from `util/backup-queue.ts:95`, only when `serverOcr` was returned by the PUT) and by deep-link import (`import/deep-link-import.ts:31`). They are stored in localStorage and lapse after 24 h (L433).
  - `VolumeItem.svelte:143` calls `markVolumeShown`.
- Timing:
  - Interval is `max(30 s, next_check_after ?? 300 s)` (L392).
  - Errors back off 60 s up to 600 s (L150).
  - The poller pauses while the tab is hidden.
  - It halts when nothing is of interest.
  - `nudgeForShownVolumes` (L862) allows one look per minute.

**UI (`catalog/server-ocr-pending.ts`)**
- `describePendingOcr` (L206) renders `VolumeQueueStatus {jobs, held}`. It is pure, provider-agnostic and needs no change.
- `heldText` knows the codes `no-processor`, `paused` and `benchmarking` (L125).
- `queueStatusStore` is keyed by `volumeQueueKey` (folded series + volume, L35).

**Manifest (`import/deep-link-manifest.ts`)**
- The manifest is a server-built per-volume file. It lists archive, ocr, layers, cover and `series_file` as `{url,size,modified}`, plus `pending[]` and `recheck_after` (L47-58, parse L141).
- Its URLs are resolved against the manifest URL, so they are always HTTP URLs.

**Hash-driven OCR upgrade (`catalog/ocr-upgrade-pass.ts`, `catalog/cloud-ocr-upgrade.ts`)**
- This is the provider-agnostic path and already works on Drive, MEGA and OneDrive.
- `planSeries` (L167) compares the `series.json` entry's `mokuro_sha256` with the row's. If they differ, `runTask` (L245) calls `provider.downloadFile(sidecar)` and then `applyCloudPrimaryOcr`.
- It is triggered only by `series-index-sync.ts:266` (after a listing refreshed `series.json` copies) and `series-open.ts:99` (series open).
- The image-only upgrade queue is `enqueueCloudOcrUpgrade` (`cloud-ocr-upgrade.ts:252`). The skip rules are in `upgradeSkipReason` (L100).
- Layers are pulled by `layer-sync.ts` after listings. `pullLayersForVolume` is also called from `util/download-queue.ts:1077`.

**Listing**
- `util/sync/syncable-file.ts`:
  - `isSyncableFile` is `cbz || sidecar || rootConfig` (L65).
  - `isSidecarFile` matches `.mokuro`, `.mokuro.gz`, `.webp`, `.jpg`, `.jpeg` and the exact name `series.json` (L50).
  - `ROOT_CONFIG_FILENAMES` is `volume-data.json`, `profiles.json`, `goals.json` and `catalog.json` (L28).
  - `.mokuro-queue.json` and `.bunko.json` match none of these. `.mokuro-queue.json` is not a sidecar because `endsWith('.mokuro')` is false on `.json`. So every provider ignores them silently. That is safe for existing readers and means the new files need direct, non-listing access.
- Drive (`google-drive-provider.ts:255-290`, `drive-files-cache.ts:170-207`): one paged `files.list` of `'me' in owners and trashed=false`, filtered by the shared helpers.
- MEGA (`mega-provider.ts:625-700`): `reinitialize()` does a full tree reload (`storage.reload(true)`, L445-484), then filters the in-memory `storage.files`.
- OneDrive (`onedrive-provider.ts:195-250`): recursive `listChildren` walk of `mokuro-reader/`. `graph-client.ts` has no delta call.
- WebDAV (`webdav-provider.ts:1160-1290`): PROPFIND with Depth infinity, or a manual recursive fallback.
- Listings happen only at startup (`init-providers.ts:145`), in CloudView actions, after backup runs (`backup-queue.ts:452`), on series delete and on Drive token refresh. There is no periodic or visibility-driven re-list.

**`serverCompilesMetadata` today**
- Interface: `provider-interface.ts:125`.
- Only `webdav-provider.ts` sets it:
  - `true` on a bunko identity-endpoint hit or an anonymous bunko connection (L718, L739).
  - Otherwise `isKnownBunkoServer(url)`, which reads the localStorage PUT_VERIFIED key (L767, L1938).
  - Reset at L874. Exposed at L605.
- Drive, MEGA and OneDrive never set it, so it is undefined and therefore false. `filesystem-provider.ts:57` hard-codes false.
- Flow: `providerManager.status` → `unified-provider-state.ts:99`. The status is a snapshot. Consumers read it synchronously via `getStatus()` or the store.
- Consumers that gate on it:
  - `metadata/catalog-file-sync.ts:47` (`canProduceCatalog`)
  - `metadata/series-backfill.ts:115` (`hasWritableNonServerProvider`, also used by `util/sync/sidecar-backfill.ts:260`)
  - `util/sync/unified-cloud-manager.ts:1862` (the `series.json` write skips unless it carries a facts/offsets request) and `:1979` (`writeCatalogFile` returns `'server-compiled'`)
  - `util/backup-queue.ts:599` → worker `serverCompilesMetadata` (`workers/unified-file-worker.ts:147,1005`) → `util/compress-volume.ts:246-277`
  - `metadata/layer-sync.ts:992,957` (`layerNeedsPush`)
  - `catalog/mokuro-hash.ts:111-122` (`layerStaysLocal`)
- `isBestEffortMetadataPath` (`syncable-file.ts:90`) makes `series.json` and `catalog.json` write failures non-fatal.

**Findings that shape the design**
1. **Drive scope is `drive.file`** (`providers/google-drive/constants.ts:16`). The reader sees only files its own OAuth client created, or files the user picked (`showFilePicker`, `google-drive-provider.ts:810`). A file written by Bunko's own client or API key is invisible to `files.list` and to a `files.get` by ID, unless Bunko writes through the same OAuth client ID or the user picks it. This blocks the Drive case entirely, including for the existing `.mokuro` sidecars and covers Bunko writes. It needs an owner decision. Options: Bunko uses the reader's client ID, the reader moves to the `drive` scope, or the Drive picker flow is extended.
2. OneDrive uses `Files.ReadWrite` (all files), so it has no such problem. MEGA and WebDAV have no scoping.
3. Nothing refreshes listings periodically. Even with a queue file, requirement (2) needs a refresh trigger: either the poller notices a `generated_at` or per-volume change and requests a relist, or there is a cheap root-change check.

## 2. Proposed file formats

**`mokuro-reader/.mokuro-queue.json`**: the same v1 schema, extended compatibly. `parseQueueFile` ignores unknown fields, so a bunko server and the reader stay compatible.
- `manifest` becomes optional. A cloud writer omits it or sets it to `""`; `parseQueueFile` already tolerates both (L112-113).
- Add `volumes[].pending_recheck_after` (seconds, optional) and an optional inline `done: [{series, volume, ocr?, layers?[{id,path,size,modified}]}]`, a bounded list of recently finished volumes (say the last 200, or 24 h). These carry the manifest equivalent. The "`ocr`/`layers` present for done volumes" point matters because absence from the file is the only completion signal today (L333-334).
- `held` already exists. Add `held.reason = 'bunko-offline'` text support in `heldText` (L125-129). Unknown codes already render as "held", so this is optional.

**`mokuro-reader/.bunko.json`** (marker):
```json
{"version":1,"instance":"...","instance_id":"uuid","heartbeat_at":"ISO","ttl_s":900,"compiles":["series.json","catalog.json"],"ocr":true,"queue":".mokuro-queue.json"}
```
Alternative: carry the marker as a top-level `bunko` object inside the queue file. That saves one file, one request per poll and one race. The queue file is already rewritten on every state change, so the heartbeat can ride on `generated_at`. I recommend one file. If you want the marker usable on idle libraries, have Bunko rewrite the queue file at least every `ttl_s/3` even when empty. Cost is one small write per heartbeat. Pick `ttl_s` ≥ 15 min so that heartbeats on Drive and MEGA stay cheap.

## 3. Carrying the manifest info without a server endpoint

Inline it, as above. What each manifest field maps to:
- `pending[]` is already `volumes[].jobs[]` (with `state` and `progress`, richer than the manifest's).
- `recheck_after` becomes the top-level `next_check_after`. The `offList` / `settleDone` machinery (L134-143, L354-377) exists only because the file lists the first 100 waiting volumes. The cloud writer should list all of them, or set `pending_volumes: null`, to avoid this path.
- Sidecar locations are known by convention (`<Series>/<Volume>.mokuro` and `<Volume>.<layer>.mokuro`), so no URLs are needed. They are resolved from the provider listing or cache via `groupSeriesSidecarFiles` (as `ocr-upgrade-pass.ts:195` does). Do NOT use `pullCompletedVolume` for the cloud case. Instead, on completion, call `requestOcrUpgradePass(providerType, [{title}])` and `pullLayersForVolume`, after forcing a relist.

## 4. Change list per file

**New `catalog/cloud-queue-source.ts`** (or generalize the poller's transport):
- Define `interface QueueSource { id: string; read(prev: {etag?:string}): Promise<{file?: QueueFile; etag?: string; notModified?: boolean}>; }`.
- Make `QueuePoller` take `read` instead of `queueUrl` and `requestInit`. This touches L156-176, L295-309 and the `pollerFor` constructor (L797-818).
- `queueUrl` becomes an opaque key (e.g. `cloud:<provider>:<rootId>`) for `statusByQueue` and `WatchEntry.queue_url`.
- Add `WatchEntry.auth: 'cloud'`. Extend `isWatch` (L451-465), where `auth` is currently `'webdav' | 'none'`.
- `onDone` for a cloud source: relist, then upgrade pass and layer pull. It does not call `pullCompletedVolume`.

**`catalog/server-ocr-queue.ts`**
- Add `connectedCloudQueueSource()`: for the active provider of type drive, mega, onedrive or a non-bunko webdav, return a source. Call it in `startServerOcrQueue` (L883-885) beside `connectedBunkoQueueUrl`.
- Cloud sources should always poll while the library is visible, not only for watched volumes. A headless-Bunko library has no uploads from this device, and `isShown` covers the volumes on screen.
- Also fix `watchUploadedVolume` (L531): there is no `serverOcr` response on cloud PUTs, so after a cloud upload to a library with a fresh marker, watch the volume by `(series, volume)` with `manifestUrl: ''`.

**`util/sync/provider-interface.ts`**
- Add optional `readControlFile?(name: string, opts?: {ifNoneMatch?: string}): Promise<{blob: Blob; etag?: string} | 'not-modified' | null>`.
- Add `ProviderStatus.cloudBunko?: {instance: string; heartbeatAt: number; fresh: boolean}`. Keep `serverCompilesMetadata` as the boolean consumers read.

**`util/sync/providers/webdav/webdav-provider.ts`**
- Implement `readControlFile` as a GET (with `If-None-Match`) of `/mokuro-reader/<name>`, through the existing auth (`workerAuthCredentials`).
- Today a plain WebDAV is only `serverCompilesMetadata` via the identity endpoint or PUT_VERIFIED (L718-767, L1938). Add a fourth source: the marker. For a non-bunko-protocol server, the existing `connectedBunkoQueueUrl` returns null (no PUT_VERIFIED), so the marker route is the only way in.

**`util/sync/providers/google-drive/google-drive-provider.ts`** (+ `api-client.ts`)
- Implement `readControlFile` as `files.list q="name='.mokuro-queue.json' and '<readerFolderId>' in parents and trashed=false" fields=files(id,md5Checksum,modifiedTime)`, then `files.get?alt=media`. Cache the file ID.
- See finding 1 above: with `drive.file` this returns nothing for a Bunko-created file. Do not start this until that is resolved.
- `listCloudVolumes` (L255) and `drive-files-cache.ts:200` need no change; the dotfiles are not matched.

**`util/sync/providers/mega/mega-provider.ts`**
- Implement `readControlFile` against the in-memory `storage.files`, finding the node by name under a `mokuro-reader` folder (as L649-690 does) and calling `.downloadBuffer()`.
- The in-memory tree is stale unless `reinitialize()` runs, and that is a full tree reload (L445-484). Polling it is the expensive part. See section 5.

**`util/sync/providers/onedrive/onedrive-provider.ts`** (+ `graph-client.ts`)
- Implement `readControlFile` as `getItemByPath(token, 'mokuro-reader/.mokuro-queue.json')`. That returns `eTag`/`cTag` metadata, which `DriveItem` already declares (L11). If it is unchanged, skip the download. Otherwise `GET .../content` (via `getWorkerDownloadCredentials`).
- Add `getDriveDelta` to `graph-client.ts`. It is not needed for v1.

**`util/sync/provider-manager.ts` / `unified-provider-state.ts`**
- Compute `serverCompilesMetadata` as `provider-reported || cloudBunkoFresh`. See section 6.

**`util/sync/syncable-file.ts`**
- Add and export `QUEUE_FILE_NAME` / `BUNKO_MARKER_NAME` and `isBunkoControlFile(basename)`. Do NOT add them to `isSyncableFile`. Adding them would put them into the listing caches, the `unified-sync-service.ts` diff and managed-file matching, and some paths would try to download or delete them. Extend `syncable-file.test.ts` to assert they stay unlisted.

**`catalog/server-ocr-pending.ts`**
- Optional only: a `bunko-offline` held code in `HELD_TEXT`. `describePendingOcr` and `VolumeItem` are unchanged.

**`metadata/series-file-sync` / `catalog-file-sync.ts:47` / `series-backfill.ts:115` / `unified-cloud-manager.ts:1862,1979`**
- No code change, because they read `serverCompilesMetadata`. They get the new behaviour from the derivation in section 6. `canProduceCatalog` and `hasWritableNonServerProvider` are the two gates that stop the reader from writing `catalog.json` and `series.json`.

**`util/backup-queue.ts:599`**
- No change needed, but note: `serverCompilesMetadata` is evaluated once per upload; a mid-run marker expiry changes what later uploads send.

**`routes/+layout.svelte`**
- Nothing, except that the poller needs a relist hook. Expose `unifiedCloudManager.fetchAllCloudVolumes({refreshIndexes:true})` as the "pick up new sidecars" action. It already ends with `refreshSeriesIndexesInBackground()` (`unified-cloud-manager.ts:313-319`), which triggers `requestOcrUpgradePass` and the layer pull.

## 5. Polling cost per provider, and recommended interval

- **WebDAV:** one GET with `If-None-Match`, usually a 304. Cheap. The server's own `next_check_after`, floored at 30 s, is fine. Recommend 30-60 s while jobs are running and 5 min when idle (the existing `UNPRICED_INTERVAL_S`).
- **OneDrive:** one `GET /me/drive/root:/mokuro-reader/.mokuro-queue.json` for metadata and `eTag`, plus a content GET only on change. Graph throttling is generous for this. Recommend 60 s while jobs are running and 5 min idle.
- **Google Drive:** one `files.list` for the ID (once; cache it), then `files.get?fields=modifiedTime,md5Checksum` per poll, and `alt=media` only on change. The quota is 12,000 requests/min/user, so this is fine. Recommend 60 s running and 5 min idle. Drive's `modifiedTime` has no sub-second precision, so use `md5Checksum` for the change test.
- **MEGA:** the expensive one. A tree reload (`reinitialize`) costs a full `fetchnodes`, which is far too heavy to poll. Options: (a) use megajs's `sc` action-packet feed, noting `reinitialize` currently removes the listener (L473) and notes stacked-handler bugs; (b) poll the single node by handle with a `g` request, which megajs exposes via `File.loadAttributes`. Recommend 120 s while running and 10 min idle, and do not relist the whole tree more than once per ~5 min.
- Relist on change only. A relist is the existing `fetchAllCloudVolumes` cost (Drive 13+ paged calls on a big library, MEGA a full reload, OneDrive a recursive walk), so gate it on a changed `generated_at` where `done` is non-empty.
- All of this pauses while the tab is hidden and resumes on `visibilitychange` (`QueuePoller.onVisibilityChange`, L223).

## 6. Deriving `serverCompilesMetadata` from the marker

- A new `cloudBunkoMarker` store holds `{instance_id, instance, heartbeat_at, ttl_s, seenAt}`, updated by the poller every time it reads the file.
- `fresh = Date.now() - heartbeat_at < ttl_s*1000`, using the Bunko-written `heartbeat_at`, not the local read time. Allow ±5 min clock skew, and prefer `now - max(heartbeat_at, seenAt-skew)`.
- In `providerManager.updateStatus` / `getStatus` merge: `serverCompilesMetadata = status.serverCompilesMetadata === true || marker.fresh`. Recompute on every marker update and on a timer at expiry. Unit-test the pure function.
- Hysteresis: once fresh, require 2× TTL of staleness before flipping back to false. Flapping would make the reader start writing `series.json` and `catalog.json` and then race Bunko when it returns.
- When `serverCompilesMetadata` flips true mid-session, pending debounced `series.json`/`catalog.json` writes must be cancelled. `catalog-file-sync.ts` already re-checks `canProduceCatalog()` at `runWrite` (L50), but `series-file-sync` debounce timers need the same re-check at fire time (`unified-cloud-manager.ts:1862` does).

## 7. Risks

- **Stale marker (Bunko gone):** heartbeat expiry as above, with hysteresis. If the marker is stale, the reader falls back to being the producer; Bunko also needs a "yield on return" rule, i.e. it must not treat reader-written `series.json` and `catalog.json` as authoritative, only as update requests. Queue statuses from a stale file should be dropped, not shown. `generated_at` older than `ttl_s` should render as `held: no processor connected` (`heldText`) or nothing.
- **Offline:** the poller already backs off and keeps the last file (L274-281). Last-known `serverCompilesMetadata` is persisted in localStorage so a cold offline start does not flip it. Persist the marker with `seenAt`.
- **Multiple devices:** each reader polls independently, so cost multiplies with devices and tabs. Poller instances are per-tab (module-level `pollers`, L761); there is no cross-tab leader. Consider a `BroadcastChannel` or Web Locks leader. Watches live in localStorage and are shared across tabs.
- **Two Bunko instances on one folder:** `instance_id` mismatch or flapping heartbeats. Accept the newest and expose a warning.
- **Write races on Drive/MEGA/OneDrive:** a half-written queue file (no atomic rename). Mitigate with write-then-rename on WebDAV, a parse failure treated as a keep-last-file result (the existing `read` throw leads to back-off, L303-305), and a `generated_at` monotonic check.
- **Reader `series.json` PUT on Bunko-compiled libraries:** `seriesFileCarriesServerRequest` (`series-file.ts:645`) assumes the server takes the PUT as a request. With a cloud Bunko, nothing processes the PUT until its next scan, and a reader overwrite could clobber Bunko's compiled file, whose `mokuro_sha256` the upgrade pass relies on. Define the protocol: the reader writes only facts and offsets, and Bunko merges them on its next scan and rewrites the whole file.
- **Layer push:** `layerStaysLocal` (`mokuro-hash.ts:111`) keeps the editor's `original` snapshot local when `serverCompilesMetadata` is true, which is the desired behaviour here too.
- **Drive visibility:** finding 1.
- **Case-only renames and NFC folding:** the queue file's `series`/`volume` values go through `volumeQueueKey` (`normalizeSeriesKey`, `normalizeVolumeTitleKey`), which folds, so they match.

## 8. Tests

Existing:
- `catalog/server-ocr-queue.test.ts` covers `parseQueueFile` (L61), `QueuePoller` (L100-372; backoff L244, visibility L269, 304 handling L151), pulling (L385-570), `startServerOcrQueue` (L612-702) and bearer auth (L704-790).
- `catalog/server-ocr-pending.test.ts` covers `relativeEta`, `volumeQueueKey`, `heldText` and `describePendingOcr`.
- `import/deep-link-manifest.test.ts`.
- `catalog/ocr-upgrade-pass.test.ts` and `catalog/cloud-ocr-upgrade.test.ts`.
- `util/sync/syncable-file.test.ts`.
- `util/sync/unified-provider-state.test.ts` (serverCompilesMetadata passthrough, L26-46).
- `util/sync/providers/webdav/webdav-provider.test.ts`, with `serverCompilesMetadata` asserts at L90, L182, L193, L203 and L235.
- Per provider: `google-drive-provider.test.ts`, `mega-provider.test.ts`, `onedrive/__tests__/`.
- Gate tests: `unified-cloud-manager.test.ts` L2484, L3419, L3468; `backup-queue.test.ts` L603; `cover-service*.test.ts` fixtures.
- Not found: dedicated `catalog-file-sync` or `series-backfill` tests asserting the `serverCompilesMetadata` gate; I did not check whether another file covers them.

Extend or add:
- `server-ocr-queue.test.ts`: poller driven by an injected `QueueSource`, not just `fetch` (the existing `deps.fetch` seam at L175 makes this easy); `parseQueueFile` with `done[]` and a missing `manifest`; a cloud completion calls `requestOcrUpgradePass` and a relist (mock both); a stale `generated_at` publishes nothing.
- `syncable-file.test.ts`: control files are not `isSyncableFile`, `isRootConfigFile`, or sidecars.
- New marker-store tests: heartbeat freshness, hysteresis, clock skew, expiry timer.
- `unified-provider-state.test.ts` and `provider-manager` tests: `serverCompilesMetadata` is `true` when the marker is fresh and falls back to `false` after expiry.
- One `readControlFile` test file per provider: 304/not-modified, 404 → null, auth-expiry, and MEGA staleness.
- `catalog-file-sync`: `runWrite` is skipped when the marker is fresh, and a pending debounced write is dropped when it flips.
- `server-ocr-pending.test.ts`: only if a `bunko-offline` held code is added.