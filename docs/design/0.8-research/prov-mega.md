# MEGA provider research for Bunko 0.8

**Scope.** The task's first paragraph names MEGA. Sections A–I are phrased for OAuth and Drive. I answered A–I for MEGA and marked the OAuth-only questions as not applicable.

**Source gaps.** MEGA's official developer pages (mega.io/developers) would not load for me. The raw `cs`/`sc` command reference is not in what I fetched, so protocol facts come from library source. [src] means I read it in code.

Sources used:
- mega-rs 0.8.0 source: https://crates.io/api/v1/crates/mega/0.8.0/download
- mega-rs README and repo: https://github.com/Hirevo/mega-rs
- mega-rs issues: https://github.com/Hirevo/mega-rs/issues
- megajs `file.mjs`: https://raw.githubusercontent.com/qgustavor/mega/master/lib/file.mjs
- megajs `mutable-file.mjs`: https://raw.githubusercontent.com/qgustavor/mega/master/lib/mutable-file.mjs
- megajs `api.mjs`: https://raw.githubusercontent.com/qgustavor/mega/master/lib/api.mjs
- megajs `storage.mjs`: https://raw.githubusercontent.com/qgustavor/mega/master/lib/storage.mjs
- rclone MEGA docs: https://raw.githubusercontent.com/rclone/rclone/master/docs/content/mega.md
- MEGA SDK README: https://raw.githubusercontent.com/meganz/sdk/master/README.md
- MEGA SDK license: https://raw.githubusercontent.com/meganz/sdk/master/LICENSE

## Verdict

- Use `mega` 0.8.0 as a vendored base, not as an untouched dependency. It has the login, session resume, listing, upload, rename, move, delete and event-polling pieces. It has three gaps that matter: no ranged download, no hashcash handling, and no incremental tree maintenance.
- Hand-rolling from scratch is not worth it. The crypto and protocol code is already written in that crate under MIT OR Apache-2.0.
- Avoid the C++ SDK.

## A. Auth

OAuth, client types, client secrets, refresh tokens and publishing status do not apply. MEGA login is email and password, plus an optional 2FA code.

- **Login flow.** The flow is:
  1. A pre-login call returns the account version.
  2. The client derives a key. Version 1 uses the legacy AES key-prepare; version 2 uses PBKDF2 with a salt.
  3. A login call carries the optional `mfa` field and a random `sek`.
  4. The client decrypts the master key and RSA private key, then RSA-decrypts `csid` into the session ID `sid`.
  - mega-rs implements this: `Client::login(email, password, mfa)`. [src] lib.rs L160-L285
  - megajs does the same; the 2FA code goes in `request.mfa`, and the password is deleted from options after the key is derived. [src] storage.mjs
- **Session resume without the password.** mega-rs `serialize_session()` writes `[1] + master_key encrypted with sek + sid` as base64. `resume_session()` restores it with no network call to log in. [src] lib.rs L287-L378
  - This blob is not the same format as megajs's `{key, sid, name, user}`, so a Reader-exported blob cannot be loaded directly. It would need a small converter. The key in megajs is the master key and the sid is the same, so the pieces exist. [belief] that a converter is simple.
  - Treat the blob as a password-equivalent secret and encrypt it at rest.
- **Headless.** No browser is needed. The 2FA code is typed once at setup and is not needed on resume. [src]
- **Session lifetime and revocation.** Error -15 (ESID) means the session is invalid or expired. [src] megajs api.mjs.
  - mega-rs has `list_sessions`, `kill_sessions` and `kill_all_sessions`, so the user can see and revoke Bunko's session. [src]
  - Re-login triggers (password change, the user killing sessions) and the idle expiry period are not documented in what I read. [belief]
  - Plan on a "re-authenticate" state in the UI.
- **Dedicated account.** I recommend asking users to use the same account the Reader uses, with a separate session for Bunko. Concurrent sessions are supported. [belief]

## B. Listing and change detection

- **Listing.** The tree comes from `fetch_nodes` with `c: 1`. The request has no pagination and returns the whole account tree in one JSON response. mega-rs holds it all in memory in a `Nodes` map and cannot scope the fetch to a subtree. [src] lib.rs L519
  - Every node name is decrypted client-side, because names live in encrypted attributes.
  - rclone warns that memory "can rise sharply" on large accounts. [doc] rclone mega.md
  - To find the root folder, `get_node_by_path` compares names with exact case and takes the first match. [src]
  - For "mokuro-reader", search all roots (Cloud Drive) yourself, case-folded, and handle duplicates.
- **Change feed.** The `sc` endpoint takes `sn` (cursor) and `sid`. The reply either carries action packets plus a new `sn`, or a `w` wait URL; POSTing to the wait URL blocks until something changes, then you poll `sc` again. [src] megajs api.mjs, mega-rs L1821-L1960
  - This is a real long-poll push channel, and no fixed polling interval is needed.
  - mega-rs `wait_events` does exactly this. EAGAIN (-3) is retried with doubling backoff.
- **What mega-rs reports.** Only created files (`t`), updated attributes (`u`) and deleted nodes (`d`). All other action packets are silently dropped. [src]
  - Folder creation inside `t` packets is filtered by `.files`, so new folders may not be reported. [belief] Verify with a test.
  - Move events are a delete plus a create. megajs defers deletes until the end of the batch because it cannot tell a move from a delete. [src] storage.mjs
  - Updated events for unknown nodes are skipped. [src]
  - **Rename and move changes need your own handling.** Plan to re-fetch the subtree on any ambiguity.
- **Cursor validity.** The cursor `sn` is in memory only in mega-rs. After a long outage the server rejects a stale `sn`. [belief] The recovery is a full `fetch_nodes`.
  - Persist `sn` alongside the SQLite index. On any error, do a full resync and diff.
- **Files the app cannot see.** Not applicable. A MEGA login sees the whole account.

## C. Reads

- **Range support in the protocol.** The download URL takes `/<start>-<end>`. megajs aligns the start down to a 16-byte AES block, requests that range, and discards leading bytes. MAC verification is turned off for ranged reads. [src] file.mjs
  - Ranged decrypt works in principle. The CTR counter is `aligned_start / 16`. [belief] The counter code is in a file I did not read.
- **mega-rs does not expose it.** `download_node` always requests `0-size` and streams the entire file through decrypt plus a condensed-MAC check. [src] lib.rs L1134-L1243
  - It can only write to an `AsyncWrite`, so a Range bridge needs a custom download. That is about 60 lines on top of its HTTP client: fetch the URL, set the CTR offset, skip the leading bytes.
  - Without it, a reader app that seeks into a .cbz forces whole-file fetches.
- **Integrity gate.** The condensed-MAC check runs only on a full download. Ranged reads cannot be verified individually, so verify whole files on the OCR spool path.
- **URL lifetime and throughput.** Not documented in what I read. [belief] URLs are short-lived (minutes to hours); request a fresh one per fetch.

## D. Writes

- **Upload.** mega-rs `upload_node` makes one request for an upload URL, then a single POST of the whole encrypted stream to `<url>/0`. It then completes with a `p` command. [src] L1245
  - The size must be known up front.
  - It computes the condensed MAC and sparse checksum while streaming.
  - It is not resumable. A failure mid-upload of a 300 MB file restarts from zero.
  - megajs uses chunks growing from 128 KB to 1 MB, over up to 4 connections, POSTed to `<url>/<position>`. [src] mutable-file.mjs
  - Chunking is optional, but I would add it for resume and parallelism. [belief]
- **Overwrite.** There is no overwrite path in either library. Upload creates a new node with a new handle. [src]
  - MEGA has a file-versioning concept in its official clients. [belief] I could not confirm the API parameter.
  - **The only safe replace** is: upload under a temporary name, rename the old node, rename the new one, then trash the old one. That is not atomic, and a concurrent Reader can see a duplicate or a missing file in between.
- **Conditional writes.** None. There is no ETag or revision precondition. Lost updates must be handled by re-reading and comparing before the write. [belief]
- **mkdir, rename, move.** The commands are `p` with `t: 1` (mkdir), `a` (rename, by re-encrypting attributes) and `m` (move). All are implemented in mega-rs. [src]
- **Delete.** mega-rs `delete_node` sends `d`, a permanent delete. Trash is a move to the rubbish bin, which `Nodes::rubbish_bin()` exposes, so `move_node(node, rubbish)` works. [src]
  - Use the move by default.
  - rclone defaults to trash too, with `--mega-hard-delete` as the opt-in. [doc]
  - Mokuro Reader's own delete behaviour is unknown to me. Check it.
- **Idempotence.** mega-rs sends a random `i` id on complete and delete, which guards retries. [src]

## E. Naming

- **Duplicates.** MEGA can hold two files with the same name in one folder. [doc] rclone mega.md
  - Names live in encrypted attributes, so the server cannot enforce uniqueness at all. [src] attributes are packed and encrypted with the node key.
  - Our index must treat duplicates as a normal state. Pick a deterministic winner, such as the newest, and surface the rest as conflicts.
- **Case.** Nothing enforces or folds case. mega-rs compares names exactly. [src]
  - For the case-insensitive, case-preserving rule, keep the display name from MEGA and add an NFC plus lowercase fold key in SQLite.
  - A case-only rename is a plain `a` rename. There is no collision with the old name on the server side.
  - Collisions between differently-cased siblings are only detectable in our index.
- **Forbidden characters, path length, Unicode form.** Not documented in what I read. [belief] Names are free-form UTF-8 in attributes, so the limits are probably client-side only. Do not rely on this; test with NFD and long names.

## F. Metadata

- **Per node.** You get a handle (8 characters), parent, owner, size, kind, created time (from the packet timestamp) and `modified_at`. [src]
- **mtime.** It is stored in the encrypted fingerprint. `upload_node` takes a `LastModified` argument, so the client can set it. [src] lib.rs L2749
- **Hash.** There is no content hash. There is an 8-byte condensed MAC and a 16-byte sparse checksum, neither of which is a standard digest. [src] rclone says "Mega does not support modification times or hashes yet." [doc]
  - Use size plus mtime plus handle as the freshness key.
- **ETag or revision.** None. A new handle on every overwrite is the change signal.

## G. Quotas and rate limits

- **Transfer quota.** Free accounts have a download bandwidth quota that is not officially published. Third-party sources say it is tracked per IP over a rolling window. [belief] (cloudwards, rcloneview). Uploads do not count. [belief]
- **Error codes.** -3 EAGAIN (retry with backoff), -6 ETOOMANY, -15 ESID (re-login), -16 EBLOCKED, -17 EOVERQUOTA, -19 ETOOMANYCONNECTIONS. [src] error.rs; the full ERRORS table runs -1 to -29 in megajs. [src]
- **Server busy and hashcash.** megajs answers an `X-Hashcash` challenge header by computing a token and retrying. [src] api.mjs
  - mega-rs has no hashcash handling. [src] I grepped `src` and found nothing.
  - Open issue #25, "Max retries reached error when logging in" (Nov 2025), may be this. [belief] I did not confirm it.
  - Budget for implementing hashcash.
- **Blocking.** rclone reports that heavy rapid use can get a login blocked for about a week, and recommends spacing commands by 3 seconds. [doc]
  - Rate-limit our own `cs` calls and never loop on login.
- **Over-quota corruption.** mega-rs issue #16 reports `CondensedMacMismatch` when over quota. [src] issues page
  - The error body is likely decrypted as file data. Check the status and the byte count before trusting a download.

## H. Rust options

| Option | Licence | Coverage | Cost |
|---|---|---|---|
| `mega` 0.8.0 (latest release 2024-10-09, 14 versions, 7 open issues) | MIT OR Apache-2.0 [src] | Login, 2FA, resume, list, whole-file download, whole-body upload, mkdir, rename, move, delete, sc events | About 3.4k lines. Lacks ranges, hashcash and incremental index. Maintenance looks slow: the newest open issue is Dec 2025. |
| Hand-roll on reqwest 0.12 with rustls | ours | Whatever we write | The same RustCrypto dependencies (aes, ctr, cbc, pbkdf2, rsa 0.9, sha2). I estimate 1.5 to 2 weeks to reach parity. [belief] |
| MEGA SDK via FFI | BSD-2-Clause [src] | Everything, including sync | CMake 3.19 plus vcpkg, per-OS toolchains, C++ API (`megaapi.h`) with SWIG Java and Objective-C bindings only. [src] No Rust binding exists in what I read. |

- **Recommendation.** Vendor `mega` 0.8.0 into the workspace (MIT or Apache lets us). Add four things:
  - ranged download;
  - hashcash;
  - chunked and resumable upload;
  - a full-resync path for the event cursor.
  - Wrap it behind the `LibraryStore` trait so a later swap stays cheap.
- **Why not the SDK.** The vcpkg build cost across Windows, macOS and Linux (including musl and Docker) is high. A C++ FFI boundary also fits badly with on-device distribution.
- **One check on `rsa` 0.9.** It carries a known timing advisory, RUSTSEC-2023-0071. [belief] It matters little here because the key is used only for the one-time `csid` decrypt, but note it.

## I. Gotchas for our use

1. **Large .cbz files, 10 to 300 MB.**
   - A whole-body POST with no resume means one dropped connection restarts the file. Add chunking.
   - Full downloads for OCR are fine because the MAC check runs there.
   - Streaming to readers needs the custom Range path.
2. **Many small sidecars.**
   - Each write is a separate upload plus complete call, and rapid calls risk blocking. Batch and throttle.
   - Cache small files locally and update them from `sc` events.
3. **Concurrent Reader writes.**
   - There are no preconditions, so last write wins. Reader replaces a JSON file by uploading a new node, probably deleting the old one.
   - The index must tolerate seeing two nodes with the same name briefly, and pick the newest.
   - Re-read the file before overwriting `catalog.json`, `profiles.json` and similar shared files.
4. **Case-only renames.** They are just attribute edits on MEGA, so they work server-side. Our fold key needs updating on the matching `u` event.
5. **Memory.** The full-tree fetch is O(account size). Cap it and drop nodes outside "mokuro-reader" right after the fetch.
6. **Transfer quota.** A free account can hit EOVERQUOTA mid-OCR. Treat -17 as a pause-and-retry state, not a failure, and verify downloaded size.

## Open items I could not confirm

- Exact quota numbers and the quota window.
- Whether the API has a versioning parameter for overwrite.
- Whether stale `sn` returns a specific error code.
- The CTR counter derivation for ranged downloads.
- Whether Reader deletes by trash or permanently.

Each needs a small experiment against a throwaway account.