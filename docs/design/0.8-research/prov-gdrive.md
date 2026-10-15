# Google Drive backend research for Mokuro Bunko 0.8

**Headline.** No Google source states whether `drive.file` access is keyed to the OAuth client or to the Cloud project. I read the scope guide, Picker docs, OAuth docs and rclone docs. Third-party evidence points toward "per client" in some cases but is weak. Treat this as unproven and run a 15-minute test before building (see the test at the end of section A).

## A. Auth for an installed app

- **Desktop with a browser.** Create a "Desktop app" client. The app listens on a random port on `127.0.0.1` or `[::1]` (`localhost` also works but may hit firewalls) and uses PKCE S256 with a 43-128 character verifier. [doc] https://developers.google.com/identity/protocols/oauth2/native-app
- **Headless, Docker or NAS.** Use the device flow with a "TVs and Limited Input devices" client. `https://www.googleapis.com/auth/drive.file` is on the allowed-scope list, together with `drive.appdata`, openid/email/profile and YouTube scopes. [doc] https://developers.google.com/identity/protocols/oauth2/limited-input-device
  - Using a different client type for the device flow gives `invalid_client`.
  - Poll `https://oauth2.googleapis.com/token` at the returned `interval`. A 428 response means `authorization_pending`, a 403 `slow_down` means back off, and a 403 `access_denied` means the user refused. [doc]
  - The example `expires_in` for the device code is 1800 seconds. [doc]
  - Incremental authorization is not supported for devices. [doc]
- **Client secret.** Google says installed and device apps "cannot keep secrets", so the secret is not confidential and can ship in a binary. [doc, both pages above]
  - The device-flow polling request requires `client_secret`. Refresh requests list it as optional. [doc]
  - The native-app token exchange and refresh list it as optional. [doc]
  - Whether Google actually accepts a Desktop-client exchange without the secret is [belief]. In practice Google desktop clients are issued a secret, and it is commonly sent.
- **Refresh token lifetime.** Tokens are always returned for installed and device flows, and are long-lived. They stop working in these cases: [doc] https://developers.google.com/identity/protocols/oauth2#expiration
  - **Testing status.** External consent screen in "Testing" gives a 7-day refresh token (basic-profile-only scopes are exempt).
  - **Inactivity.** The token has not been used for 6 months.
  - **Password change.** Only applies to tokens with Gmail scopes.
  - **User revocation.** The user removes the app's access.
  - **Per-client limit.** 100 refresh tokens per account per client ID; issuing a new one silently invalidates the oldest.
  - **Cross-client limit.** The installed-app docs describe a second limit, one per user across all clients; older tokens stop working when it is exceeded. [doc]
- **Publishing status.** The consent screen must be set to "In production" or the 7-day expiry applies. `drive.file` is classified non-sensitive on the scope page. [doc] https://developers.google.com/workspace/drive/api/guides/api-specific-auth
  - The policy page says only sensitive and restricted scopes need review. It says production apps need verified brand information. [doc] https://developers.google.com/identity/protocols/oauth2/production-readiness/policy-compliance
  - I could not read the 100-user unverified cap for non-sensitive scopes. [belief] It does not apply to non-sensitive scopes.
- **Revocation.** POST `https://oauth2.googleapis.com/revoke`. The docs warn it "removes all scopes granted to the project, which affects every client in that project". [doc] This would break the reader's grant too.
- **Re-consent triggers.** `invalid_grant` on refresh means re-run the flow. Also re-consent after any of the expiry causes above. [doc]
- **The drive.file client-vs-project question.**
  - [doc] The scope covers files the app creates, plus files the user opens with the app or shares via the Picker. https://developers.google.com/workspace/drive/api/guides/api-specific-auth
  - [doc] Picker `setAppId` takes the Cloud project number (`APP_ID` is described as "Your Cloud project number"). That implies grants are attached to the project, which is the app identity, and not to a client. https://developers.google.com/workspace/drive/picker/guides/web-picker
  - [doc] OAuth docs say a client ID "identifies a single app", which is no help.
  - [doc, negative] No Google page says whether a second client in the same project shares the grant.
  - [belief] Client-keyed grants are plausible too. A GitHub PR (https://github.com/nelaka141/personnel-finance-manager/pull/4) reports that localhost and production client IDs saw different files. These may have been different projects, so it is not conclusive. A Google Apps Script thread (https://groups.google.com/g/google-apps-script-community/c/_W-NKbttfbo) only says Select in the Picker grants access, and that a picked folder does not grant its children.
  - Test: use the reader's web client to create a folder, then create a Desktop or device client in project 868461116957 and run `files.list` with `q="name='mokuro-reader' and trashed=false"`. A hit means the grant is per project. No hit means per client.
  - Fallbacks if the test fails:
    - Have the user pick the folder in a Picker in Bunko's own web UI. This grants the folder only, not its children.
    - Move to the restricted `drive` scope, which needs security assessment and verification.
    - Run Bunko's own web OAuth client in the same project with a hosted redirect.
    - Reverse option: Bunko creates the library and the reader opens it through a Picker.

## B. Listing and change detection

- **Listing.** `files.list` costs 100 quota units per call. The maximum `pageSize` is not stated on the search page, but samples use 1000. [doc] https://developers.google.com/workspace/drive/api/guides/limits
  - `q` supports `'<id>' in parents`, `trashed=false` and `name = '…'`; quotes are escaped with a backslash. There is no recursive query, so walking is breadth-first by parent. [doc] https://developers.google.com/workspace/drive/api/guides/search-files
  - An alternative is to list everything the app can see in one stream and rebuild paths from `parents`, which is what the reader does. [belief: efficient under `drive.file`, since only app-visible files return]
- **Change feed.**
  - `changes.getStartPageToken` returns the cursor. `changes.list` returns changes oldest first. [doc] https://developers.google.com/workspace/drive/api/guides/manage-changes
  - `pageSize` defaults to 100, with a maximum of 1000. [doc]
  - "The page token doesn't expire", and neither does `newStartPageToken`. [doc] https://developers.google.com/workspace/drive/api/reference/rest/v3/changes/list
  - Use `includeRemoved=true` to see deletions and loss of access. [doc]
  - `drive.file` is listed as an accepted scope, but the docs do not say which changes are reported under it. [doc, negative] Presumably only visible files appear, but that is [belief] and needs to be tested.
  - Trashed files appear as changes with `file.trashed=true`. [belief]
- **Polling interval.** Not stated in the docs. A 30-60 second `changes.list` is cheap, because a page with no changes still counts as one list-class call. I recommend 30-60 seconds when serving readers, and 5 minutes when headless. [belief]
- **Push.** `changes.watch` delivers a notification with no change details, after which you call `changes.list`. It needs a public HTTPS endpoint, which a NAS rarely has, and the watch call itself counts against quota. [doc] Polling is the sensible default.

## C. Reads

- **Range.** `files.get?alt=media` supports `Range: bytes=500-999`. [doc] https://developers.google.com/workspace/drive/api/guides/manage-downloads
- **URL lifetime.** Not documented. With OAuth you call `alt=media` with a bearer token and get no signed URL. A short-lived redirect may be issued. [belief]
- **Throughput.**
  - Download costs 200 quota units, per the limits page. [doc]
  - Egress above 1 TB per day is subject to charges. [doc]
  - rclone says Drive is limited to about 2 transactions per second and has a default shared client rate of 10 per second. [doc] https://rclone.org/drive/
  - Large-file download speed is not documented. [belief]
- **Abuse flag.** Files flagged as malware can be downloaded only by the owner with `acknowledgeAbuse=true`. [doc] Unlikely for .cbz files.

## D. Writes

- **Simple.** `uploadType=media`, 5 MB or less. Multipart also covers 5 MB or less. [doc] https://developers.google.com/workspace/drive/api/guides/manage-uploads
- **Resumable.** `uploadType=resumable` for anything over 5 MB; it works for small files at the cost of one extra request. [doc]
  - Chunks must be multiples of 256 KB, except the last one. rclone defaults to 8 MiB and notes each chunk is buffered in memory. [doc]
  - The session URI expires after one week. [doc]
  - A failure with another 4xx means restart with a new session URI; 5xx means resume. [doc]
- **Overwrite in place.** `PATCH /upload/drive/v3/files/{fileId}` with media. The URI addresses the existing file by ID, so the ID is unchanged. The page does not state it outright. [doc inference]
  - Maximum size is 5,120 GB. [doc] https://developers.google.com/workspace/drive/api/reference/rest/v3/files/update
- **Conditional writes.** The `files.update` reference lists no ETag or `If-Match` and no precondition parameters. [doc, negative] I could not find any in v3. [belief] Treat Drive as having no compare-and-swap, so:
  - Re-read the `version` or `md5Checksum` just before writing and accept a small race window.
  - Keep sidecars single-writer where possible.
  - Pre-generated IDs (`files.generateIds`) make creates idempotent: a repeated create returns 409 instead of a duplicate. [doc] (manage-uploads page)
- **Atomic replace.** No documented atomic rename-over. The content swap on PATCH is atomic per upload (a new revision is created only when the upload finishes). [belief] Each update also creates a revision; use `keepRevisionForever=false`. A file can have at most 200 pinned revisions. [doc]
- **mkdir.** `files.create` with mime type `application/vnd.google-apps.folder` and `parents`. [belief]
- **Rename and move.** `files.update` for `name`, and `addParents`/`removeParents` for moves. [doc]
- **Delete.** `trashed=true` is the trash route, and only the owner can trash. `files.delete` is permanent. [doc] The reader finds folders by `'me' in owners and trashed=false`, so trashing hides a file from the reader. [per the task text]

## E. Naming

- **Duplicates.** `name` is "not necessarily unique within a folder", and rclone says Drive "unlike all the other remotes" allows duplicates and recommends dedupe. [doc] https://developers.google.com/workspace/drive/api/reference/rest/v3/files and https://rclone.org/drive/
- **Case.** Not documented. [doc, negative] My belief is that `name =` matching is case-insensitive and stored names are case-preserving; treat it as untested.
- **Forbidden characters and length limits.** Not documented in the pages I read. Windows-hostile characters like `:` and `?` are probably accepted by Drive. [belief] The name length limit is also undocumented.
- **Unicode normalization.** Not documented; [belief] Drive stores names as sent without normalizing.
- **What this means for the library.**
  - Do not use `name =` queries as the authority. Index the remote tree in SQLite and resolve paths locally with the NFC + lowercase fold key.
  - Keep a many-to-one index: it must tolerate two Drive files folding to the same path (duplicates, or "A" vs "a").
  - Case-only rename is one `files.update` with a new `name`. It is by ID, so no collision occurs.
  - Pick a deterministic winner for duplicates (oldest `createdTime`, say) and surface a warning.

## F. Metadata per file

| Field | Notes |
|---|---|
| `id` | Stable across content updates (inferred). |
| `size` | Output only. |
| `modifiedTime` | Writable, millisecond accuracy (rclone). Setting it also updates `modifiedByMeTime`. |
| `md5Checksum` | Binary content only. |
| `sha1Checksum`, `sha256Checksum` | Content stored in Drive only; rclone notes older files may lack them. |
| `version` | Monotonic, changes on every server-side change. Use as the ETag substitute. |
| `headRevisionId` | Binary content only. |
| `parents` | One parent only. |
| `appProperties` | Private to the requesting app; `properties` is visible to all apps. |

Sources: https://developers.google.com/workspace/drive/api/reference/rest/v3/files and https://rclone.org/drive/

## G. Quotas and errors

- **Quota.** 1,000,000 units per minute per project and 325,000 per minute per user per project. Costs per call: `files.get` 5, `files.list` 100, download 200, update 50. [doc] https://developers.google.com/workspace/drive/api/guides/limits
- **Upload cap.** 750 GB per user per day, then a 24-hour wait. [doc]
- **Billing.** The limits page says Google plans to start charging for overages later in 2026, with 90 days' notice. [doc]
- **Back off on.** 403 `rateLimitExceeded`, 403 `userRateLimitExceeded`, 429, and 500/502/503/504, using exponential backoff of roughly 2^n seconds plus jitter, capped at 32-64 seconds. [doc]
- **Hard stops.** 403 `storageQuotaExceeded` (do not retry) and 403 `dailyLimitExceeded` (a project cap). [doc] https://developers.google.com/workspace/drive/api/guides/handle-errors
- **Other errors.**
  - 404 `notFound` means the file does not exist or the user lacks access.
  - The error page documents no 409 or 412.
  - rclone says "User rate limit exceeded" can need a wait of 24 hours or more. [doc, rclone]

## H. Rust options

- **`google-drive3` 7.0.0+20251218.**
  - Released 2026-01-01, licensed MIT. [src] https://crates.io/api/v1/crates/google-drive3
  - It is generated code, with 3.15M downloads. It depends on `yup-oauth2`, `hyper-rustls` and `google-apis-common`, so it runs on hyper and not on reqwest.
  - It covers everything above: resumable uploads, `changes.list`, Range via the download builder.
- **`yup-oauth2` 12.1.2.** Updated 2026-01-07, with 26.6M downloads. [src] https://crates.io/api/v1/crates/yup-oauth2 It provides device, installed and service-account flows. I did not confirm its license; I have seen "MIT" listed for it. [belief]
- **`oauth2` 5.0.0.** Updated 2025-01-21, licensed "MIT OR Apache-2.0". [src] https://crates.io/api/v1/crates/oauth2 Generic and reqwest-friendly.
- **Recommendation: hand-roll on reqwest 0.12 + rustls**, with `oauth2` 5.0 for the device and loopback + PKCE flows.
  - Reason: the Drive surface we need is about ten endpoints. A hand-rolled client keeps one HTTP stack and lets us stream, set Range, and control retry and chunk buffering.
  - The generated crate is the fallback if hand-rolling stalls.
  - `google-drive3` pulling in hyper alongside reqwest adds about a second HTTP stack; I did not measure the binary-size cost.

## I. Gotchas for our use

1. **Lost updates.** There are no conditional writes, and the reader writes `catalog.json`, `volume-data.json`, `profiles.json` and `goals.json` concurrently. Bunko should not write these files unless it must. When it does, re-fetch, merge, and write back with the `version` check, accepting a small race.
2. **Visibility.** Files the reader creates may be invisible to a different client or project (the section A question). The reverse also applies: files Bunko writes may be invisible to the reader. Test both directions.
3. **Folders.** Under `drive.file`, a Picker-granted folder does not expose its children. [doc-ish: Google Apps Script thread] This kills the "pick the folder" fallback for a whole library.
4. **Duplicates.** The reader may create duplicate folders or files from racing tabs. Handle several matches in the SQLite fold index.
5. **Large files.**
   - Use resumable upload with 8-32 MiB chunks, which is a multiple of 256 KB.
   - For .cbz reads, Range requests work.
   - A 10-300 MB download costs 200 units, so it is fine on quota.
   - Egress above 1 TB per day is charged, so cache archives on disk.
6. **Many small files.** Listing 100 units per call and about 2 transactions per second means a first sync of thousands of sidecars takes minutes. Use `changes.list` after that, and batch listings by parent.
7. **Revisions.** Rewriting a sidecar creates a revision each time. Revisions of binary files count against storage, so avoid rewriting unchanged sidecars.
8. **Trash.** A trashed file leaves the reader's `trashed=false` listing but remains in storage and counts against quota.
9. **7-day token expiry.** Ship with the consent screen "In production", or Testing-mode users re-authorize every week. The headless device flow also needs a way to show the code, such as the tray or log.
10. **Shared client secret.** All Bunko installs would share one client ID, and therefore one project rate limit (1M units per minute per project, 325k per user). Fine for now, but a heavy install could affect others. Revoking tokens at the project level also hits the reader's grant.

**Unverified items to resolve with the test:** the per-client or per-project grant, which changes `drive.file` reports under the limited scope, and case-insensitive `name` matching.