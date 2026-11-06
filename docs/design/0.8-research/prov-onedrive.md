# OneDrive (Microsoft Graph v1.0) as a Bunko LibraryStore backend

Date: 2026-10-12. Tags: [doc] = stated in docs, [src] = read in library source or the crates.io API, [belief] = unverified.

## Short answers to your confirm list
- **Delta scope:** the v1.0 delta page lists only `/drive/root/delta` (https://learn.microsoft.com/en-us/graph/api/driveitem-delta?view=graph-rest-1.0) [doc].
  - rclone says the delta API "only works at the root of the drive" and otherwise recurses from the root and filters (https://raw.githubusercontent.com/rclone/rclone/master/docs/content/onedrive.md) [src].
  - Whether personal accounts accept a folder-scoped `/items/{id}/delta` is [belief] and untested. A web search found conflicting community answers, with no doc settling it.
  - **Design on root delta plus client-side filtering to the `mokuro-reader` subtree. Verify folder delta by experiment before depending on it.**
- **Delta tokens:** a `410 Gone` carries a `Location` header with a fresh enumeration and the code `resyncChangesApplyDifferences` or `resyncChangesUploadDifferences` [doc]. The docs give no fixed token lifetime ("reuse an old token after being disconnected for a long time").
- **ETag vs cTag:** `cTag` changes only on content change. `eTag` covers metadata plus content [doc: https://learn.microsoft.com/en-us/graph/api/resources/driveitem?view=graph-rest-1.0].
  - `If-Match` takes an eTag or a cTag and returns 412 on mismatch for `createUploadSession`, PATCH (rename/move) and DELETE [doc: the createUploadSession, driveitem-update and driveitem-delete pages].
  - The simple `PUT .../content` page documents no `If-Match` [doc]. Whether it is honored there is [belief], so don't rely on it.
- **Hashes:** `quickXorHash` is "the only value that is guaranteed to be available for both OneDrive for work or school and OneDrive for home" [doc: https://learn.microsoft.com/en-us/graph/api/resources/hashes?view=graph-rest-1.0]. `sha1Hash` and `crc32Hash` are "if available". rclone says SHA1 is being phased out on Personal [src].
- **Fragments:** each fragment must be a multiple of 320 KiB (327,680 B) and under 60 MiB per request. 5-10 MiB is recommended, and resumable upload is recommended above 10 MiB [doc: https://learn.microsoft.com/en-us/graph/api/driveitem-createuploadsession?view=graph-rest-1.0].
- **Throttling:** 429 or 503 with `Retry-After`. Honor it and pause all requests [doc: https://learn.microsoft.com/en-us/graph/throttling].
- **Device code with `/common`:** supported. Tenant may be `/common`, `/consumers` or `/organizations` [doc: https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-device-code].
- **SPA + desktop on one registration:** this is plausible but not stated in so many words.
  - The reply-URL page says the login server picks the type (web, native or spa) from the matched redirect URI [doc: https://learn.microsoft.com/en-us/entra/identity-platform/reply-url].
  - It says to separate localhost URIs by path rather than port, and "Allow public client flows" is the switch for the public flows.
  - Combined with the 24-hour vs 90-day refresh behavior, one registration holding both kinds of redirect is workable [belief on the combination].
  - **Make Bunko's loopback path differ from the Reader's SPA redirect.**

## A. Auth for an installed app
- **Flows:**
  - Authorization code with PKCE on a loopback redirect: `http://localhost` is allowed for Mobile and desktop, and the port is ignored when matching [doc: reply-url page].
  - Device code for headless, Docker and NAS: 15 minutes default `expires_in`, poll at `interval`, and the errors are `authorization_pending`, `authorization_declined`, `bad_verification_code`, `expired_token` [doc: device-code page].
  - Personal accounts get asked to sign in again during device code, and `verification_uri_complete` is not supported [doc].
- **Prefer `127.0.0.1` to `localhost`.** The portal rejects an `http` loopback URI, so it must be set via the manifest `replyUrlsWithType`. IPv6 `[::1]` is unsupported [doc: reply-url page].
- **Client type and secrets:** desktop and headless apps are public clients that "can't have client secrets" [doc: https://learn.microsoft.com/en-us/entra/identity-platform/msal-client-applications]. **Ship no secret. Use PKCE or device code with only the client ID.**
- **Refresh-token lifetime:** 24 hours for tokens issued to a `spa` redirect, 90 days for all other scenarios [doc: https://learn.microsoft.com/en-us/entra/identity-platform/refresh-tokens].
  - Every use returns a new refresh token, so persist the newest and delete the old one.
  - rclone reports a 90-day idle expiry [src].
  - Personal and work accounts are not distinguished in the docs. Whether the 90 days is sliding on every use is implied by the rotation text but not stated [belief].
  - **Bunko must use a native or device-code token, not the Reader's SPA one, or it re-auths every 24 hours.**
- **Revocation:** a password change revokes password-based tokens. Admin reset, a user revoking sessions or an admin revoking tokens revokes them too, and the sign-in service may revoke at any time [doc: refresh-tokens page]. **Handle `invalid_grant` by prompting a re-sign-in.**
- **Publishing status:** publisher verification needs a work or school account registration, and "Apps that are registered by using a Microsoft account can't be publisher verified" [doc: https://learn.microsoft.com/en-us/entra/identity-platform/publisher-verification-overview].
  - The unverified-publisher consent block targets multitenant apps in other organizations, which would hit work accounts. Impact on consumer-only use is [belief] and untested.
  - Re-consent triggers: adding scopes, or an admin policy on work tenants [belief].

## B. Listing and change detection
- Initial enumeration: call `delta` with no token, follow `@odata.nextLink`, and keep the final `@odata.deltaLink`. The docs say paging `children` is "not guaranteed" to return everything during writes, so delta is the only guaranteed full read [doc: delta page].
- Delta returns the latest state per item, and an item may appear more than once, so use the last occurrence [doc].
- `parentReference.path` is absent in delta, so **track by item id and rebuild paths yourself** [doc].
- Deleted items carry a `deleted` facet. Delete a folder locally only once it is empty [doc].
- Delta omits properties by account type [doc]:
  - Business: `ctag` on create and modify; `ctag` and `name` on delete.
  - Consumer: `ctag` and `size` on delete.
  - **A delete event may therefore carry no name, so the SQLite index must be keyed by id.**
- `?token=latest` returns an empty page plus the current link. A timestamp token works on Business and SharePoint only [doc].
- Delta supports `$select`, `$expand` and `$top` [doc].
- Cost: delta with a token is 1 resource unit and delta without a token is 2 [doc: https://learn.microsoft.com/en-us/sharepoint/dev/general-development/how-to-avoid-getting-throttled-or-blocked-in-sharepoint-online].
- **Polling interval:** the docs give no fixed interval, only "polling repeatedly or at high rates causes throttling". A periodic poll of 1-5 minutes is [belief]. The scan guide's "no more than once per day" applies to the safety poll when webhooks are used [doc: https://learn.microsoft.com/en-us/onedrive/developer/rest-api/concepts/scan-guidance].
- **Push:** `GET /me/drive/root/subscriptions/socketIo` returns a socket.io `notificationUrl`. It supports delegated permissions on personal and work accounts, uses websocket transport only, and needs no public endpoint [doc: https://learn.microsoft.com/en-us/graph/api/subscriptions-socketio?view=graph-rest-1.0].
  - After a notification, run delta. Notifications carry no payload I verified [belief].
  - Classic webhooks need a public HTTPS URL and periodic renewal [doc: scan guide].
  - Rust has no first-party socket.io client. A hand-rolled websocket is [belief].
  - **Plan: delta polling as the baseline, socket.io as an optional low-latency add-on.**
- Files the app cannot see: with `Files.ReadWrite` the whole drive is visible, so nothing is hidden. Mokuro Reader's scope already covers it. Behavior for items outside the `mokuro-reader` subtree is just noise to filter [belief].

## C. Reads
- `GET /items/{id}/content` returns a 302 to a pre-authenticated URL, and no `Authorization` header is needed on it [doc: https://learn.microsoft.com/en-us/graph/api/driveitem-get-content?view=graph-rest-1.0].
- Range requests go to the download URL, not to `/content`. The reply is 206, but the server "may" ignore the range and return 200 with the full body [doc]. **Treat a 200 response to a Range request as a possible case.**
- `@microsoft.graph.downloadUrl` is short-lived. The driveItem page says about 1 hour and the content page says "might expire within minutes" [doc]. Fetch fresh on each use, never cache.
- `If-None-Match` with an eTag or cTag gives 304 [doc].
- Resource cost of a download is 1 unit [doc: throttling page]. There is no per-file bandwidth figure in the docs. Throughput measurement is [belief].

## D. Writes
- **Simple PUT:** documented limit is 250 MB per call [doc: https://learn.microsoft.com/en-us/graph/api/driveitem-put-content?view=graph-rest-1.0].
  - The onedrive-api crate hard-codes `UPLOAD_SMALL_MAX_SIZE` at 4,000,000 bytes [src].
  - **Use the simple PUT for sidecars and JSON under 4 MB, and upload sessions for .cbz.**
- **Sessions:**
  - `POST .../createUploadSession` with body `{"item":{"@microsoft.graph.conflictBehavior":"replace"}, "deferCommit":true}`. Values are fail (default), replace, rename [doc].
  - PUT fragments sequentially with `Content-Range`, with no `Authorization` header on the `uploadUrl`. Every fragment must declare the same total size [doc].
  - A 202 returns `nextExpectedRanges`, a 416 means a fragment was already received, and a GET on the `uploadUrl` returns status [doc].
  - A 404 means start over, and retry 5xx with backoff [doc].
  - The session expires if there is no activity, and each fragment extends it [doc].
  - `deferCommit` is committed by an empty POST on Business and SharePoint, or by a final PUT on Personal [doc]. Without it, the final byte commits automatically.
  - `fileSize` in the item body is documented as Personal-only [doc].
- **Overwrite in place:** use `POST /items/{itemId}/createUploadSession` ("an update only requires the ID") [doc]. Whether the item id is preserved on replace is [belief] and probably yes. Verify.
- **Conditional writes:** send `If-Match: <eTag>` on createUploadSession (412 on mismatch), and on the PATCH and DELETE calls [doc]. For atomic replace, first read the item's eTag from the index and send it. On 412, re-run delta and reconcile.
  - Server-side race: if another writer creates the same name during upload, the final fragment returns `409 nameAlreadyExists` [doc].
  - The upload is not visible until commit, so a reader sees the old or new file, never half [doc, by design of session; exact atomicity statement is [belief]].
- **mkdir:** `POST /items/{parent}/children` [doc: driveitem-post-children, listed in the methods table but body shape not fetched; use `{"name":..., "folder":{}, "@microsoft.graph.conflictBehavior":"fail"}` [belief]].
- **Rename/move:** `PATCH /items/{id}` with `name` and/or `parentReference` and optional `If-Match` [doc].
- **Delete:** moves the item to the recycle bin, not a permanent delete, and returns 204 [doc]. A separate permanentDelete exists [doc: driveItem methods table].
- **mtime:** `fileSystemInfo` is Read-Write [doc]. rclone sets times to 1-second accuracy [src].

## E. Naming
- **Case:** "OneDrive is case insensitive so you can't have a file called Hello.doc and one called hello.doc" [src: rclone doc]. So no duplicates differing only by case; a name collision is the 409 or conflictBehavior path. The official Graph docs fetched do not state this [belief for the doc claim].
- Case-preserving: the stored name keeps its case [belief]. This matches the owner rule. **A case-only rename via PATCH `name` should work, but verify.**
- **Forbidden characters:** `/ \ * < > ? : |`. Business also bans `# %`. Folder names can't end with `.`. Business names can't start with `~` [doc: https://learn.microsoft.com/en-us/onedrive/developer/rest-api/concepts/addressing-driveitems]. The support page also bans `"`, leading and trailing spaces, reserved names (CON, PRN, AUX, NUL, COM0-9, LPT0-9, `.lock`, `_vti_`, `desktop.ini`) and names starting with `~$` [doc: https://support.microsoft.com/en-us/office/restrictions-and-limitations-in-onedrive-and-sharepoint-64883a5d-228e-48f5-b3d2-eb39e07630fa].
- **Path length:** 400 characters is [belief]. The support page I fetched defers to another article.
- **Unicode normalization (NFC/NFD):** not documented in anything I read. Assume the service stores what you send [belief]. **Your NFC+lowercase fold must be applied in Bunko's index, and Bunko must send the original-case name unchanged.**
- **Path encoding:** percent-encode per segment, and `#` and `%` need encoding [doc: addressing page]. **Prefer id addressing over path addressing** [doc: ids persist across rename and move].
- **Fit with your rule:** the index key is `NFC(name).lowercase`, and the displayed name is the stored one. Any Bunko-side name that case-folds to an existing sibling is the same file. A Reader client that creates `Vol.CBZ` while Bunko holds `vol.cbz` is a conflict on the OneDrive side, so rely on `conflictBehavior`.

## F. Metadata
- `id`: stable across rename and move [doc]. `size`: Int64 [doc].
- `lastModifiedDateTime`: read-only. `fileSystemInfo.lastModifiedDateTime`: read-write, and the right place for the client mtime [doc].
- Hash: `quickXorHash` only (base64), reliably. `sha1Hash` and `crc32Hash` are optional (hex) [doc]. A reference QuickXorHash snippet exists on the Microsoft docs site; implementing it for integrity checks is feasible [doc pointer].
- Revision: `eTag` for the item and `cTag` for content [doc]. A folder's cTag changes when any descendant changes, but is not returned for Business folders [doc]. Per-file version history exists via versions [doc: methods table].

## G. Quotas and rate limits
- Graph's global limit is 130,000 requests per 10 seconds per app across tenants [doc: https://learn.microsoft.com/en-us/graph/throttling-limits].
- The SharePoint-published numbers (per user 3,000 requests per 5 minutes, 50 GB ingress per hour, 100 GB egress per hour; per app per tenant 1,250 to 6,250 resource units per minute) are for work tenants [doc: SharePoint throttling page]. **Personal OneDrive limits are not published.** That page also warns that unlicensed usage limits may be lowered.
- Resource unit costs per request: 1 for get item, delta with token and download; 2 for list children, create, update, delete and upload [doc].
- Back off on 429 and 503 using `Retry-After`. Also retry 500, 502, 504 on uploads. A 507 means quota (when `fileSize` is declared) and a 412 means an eTag mismatch [doc].
- Set a descriptive User-Agent in the `ISV|Company|App/Version` form, since undecorated traffic is prioritized lower [doc: SharePoint throttling page].
- Do not create multiple app IDs to evade limits [doc]. File size limit is 250 GB per file [doc: support page].

## H. Rust implementation options
| Crate | Version / last release | License | Coverage |
|---|---|---|---|
| `onedrive-api` | 0.11.0, 2026-01-21 [src crates.io API] | MIT | root-only delta, upload sessions, `if_match`, `conflict_behavior`, download URL, list, move, delete, auth utilities [src docs.rs]. |
| `graph-rs-sdk` | 3.0.1, 2025-04-20 [src] | MIT | broad generated Graph surface, includes OAuth. Large dependency tree [belief]. |
| `oauth2` | 5.0.0, 2025-01-21 [src] | MIT OR Apache-2.0 | OAuth2 and PKCE and device flow only. |

- `onedrive-api` does not auto-retry 429, and the docs.rs page lists no throttling handler [src]. Its `beta` feature is excluded from semver [src].
- None of these provides socket.io push, and I verified no crate that handles `Retry-After` for you.
- **Recommendation:** hand-roll a thin Graph client on reqwest 0.12 + rustls (about 10 endpoints: delta, get, children, content/Range, createUploadSession plus fragment PUT, simple PUT, PATCH, DELETE, mkdir). Add the `oauth2` crate for PKCE and device code, or write the 3 token calls directly.
  - Reasons: the core calls are few; you need custom throttle handling, the 4 MB vs 250 MB split and resume logic anyway; and `onedrive-api` is a single-maintainer crate that is root-only.
  - If you want a faster start, `onedrive-api` (MIT) is license-clean and could be wrapped, but expect to bypass it for retry, `deferCommit`, and `Range`.
  - All three are Apache/MIT-compatible. No GPL found.

## I. Gotchas for our use
- **Reader writes the same folder.** Reader runs as an SPA with Files.ReadWrite and `conflictBehavior=replace`. A write Bunko makes while the Reader replaces the same file is last-writer-wins unless Bunko sends `If-Match`.
  - **Always send `If-Match` with the indexed eTag on every overwrite of `catalog.json`, `volume-data.json`, `profiles.json`, `goals.json` and `series.json`, then merge on 412.**
  - Do the same for sidecars: read-modify-write with eTag.
  - The Reader doesn't know about eTags, so its replace can still silently clobber. That is a risk to accept and document [belief].
- **Echo suppression.** Our own writes come back in delta. Compare the returned eTag/cTag with the one the write returned and skip the match.
- **Many small files.** One list or delta call returns many items. Cache small JSON by eTag, check the cTag, and use `If-None-Match` (304) on download.
- **Large .cbz (10-300 MB).** Upload via sessions in 5-10 MiB fragments that are multiples of 320 KiB, persist the `uploadUrl` for resume, and re-create on 404. Download via pre-authenticated URL with Range, and refetch the URL when it expires mid-transfer.
- **Download URL expiry.** Don't hold download URLs in the index. Ask for a fresh one per fetch.
- **Throttling.** Bunko's bulk OCR scan of hundreds of volumes shares the user's quota with the Reader. Serialize requests, honor `Retry-After` globally across all tasks, and avoid parallel fan-out on full listing.
- **Case-folding conflicts.** If two remote items fold to the same key (e.g. created by two clients), OneDrive itself prevents duplicates in one folder [src], so the index never sees both. A case-only rename arrives in delta as the same id with a new name.
- **Token revocation or expiry.** A 401 or `invalid_grant` must put the store into a "needs re-auth" state, not crash. Headless installs need a visible device-code prompt path.
- **Delta resync.** On 410, re-enumerate, then diff against the SQLite index and apply deletes. Don't delete a folder row until its children are processed [doc].
- **Recycle bin.** Deleting through the API doesn't free quota until the bin empties [belief].

## Open items to verify by experiment
1. Folder-scoped `/items/{id}/delta` on a personal account.
2. `If-Match` on simple PUT `/content`.
3. Item id preservation on session-replace.
4. Case-only rename via PATCH.
5. Delta cursor lifetime in practice.
6. A SPA and a native redirect URI coexisting on registration `50f6e3ce-aa4f-4a0e-8357-7e51a6aad04d`.