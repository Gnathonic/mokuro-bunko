# Generic WebDAV as a storage backend for Bunko: research notes

Evidence is thin where a server's own docs were silent. Those claims are marked [belief] and need a live probe. The WebFetch summaries covered only the first part of the long RFC pages. I could not read the RFC 4918 text for MOVE, Overwrite and MKCOL, or RFC 9110 §13.

Source URLs used:
- RFC 4918: https://www.rfc-editor.org/rfc/rfc4918
- RFC 6578: https://www.rfc-editor.org/rfc/rfc6578
- Nextcloud chunking: https://docs.nextcloud.com/server/latest/developer_manual/client_apis/WebDAV/chunking.html
- Nextcloud basic API: https://docs.nextcloud.com/server/latest/developer_manual/client_apis/WebDAV/basic.html
- Nextcloud big-file config: https://docs.nextcloud.com/server/latest/admin_manual/configuration_files/big_file_upload_configuration.html

## A. Auth

- **Flows:** WebDAV has no OAuth installed-app flow. Basic auth over HTTPS is the baseline, and reqwest_dav also offers Digest [doc: https://github.com/niuhuan/reqwest_dav].
- **Nextcloud:** Use an app password, not the main password. It also speeds up access [doc: https://docs.nextcloud.com/server/latest/user_manual/en/files/access_webdav.html].
- **Headless and desktop are the same:** The user pastes a URL, username and app password.
- **No client secret to ship:** The stored credential is the user's own app password.
- **Expiry and revocation:** Credentials last until the user revokes the app password on the server. A 401 means re-enter credentials. Nothing publishing-status related applies.
- **Bunko-to-Bunko:** Bunko serves Basic auth, so the same flow works.
- **Security:** Refuse plain HTTP unless the user explicitly overrides. rclone's own docs warn that Basic over HTTP is insecure [doc: https://rclone.org/commands/rclone_serve_webdav/].

## B. Listing and change detection

**Listing**
- RFC 4918 §9.1: servers MUST support Depth 0 and 1 and SHOULD support infinity. A server may refuse infinity with 403 and the `propfind-finite-depth` precondition [doc: RFC 4918 §9.1, §9.1.1]. A request with no Depth header is treated as infinity.
- Apache `mod_dav` defaults to `DavDepthInfinity off` [doc: https://httpd.apache.org/docs/2.4/mod/mod_dav.html].
- SabreDAV (the engine under Nextcloud and ownCloud) disables infinity by default [doc: https://sabre.io/blog/2015/sabre-dav-2.0.8-released, via search result]. ownCloud answers "Depth infinity not supported" and has a `dav.propfind.depth_infinity` key from 10.9. Nextcloud has no such documented key [doc: https://central.owncloud.org/t/propfind-infinity-how-to-check-if-desktop-client-is-using-it/40020].
- nginx `ngx_http_dav_module` implements only PUT, DELETE, MKCOL, COPY and MOVE. It has no PROPFIND, so it cannot be a read/list backend alone [doc: https://nginx.org/en/docs/http/ngx_http_dav_module.html].
- Probe at connect time: try `Depth: infinity` and fall back to recursive Depth:1 on 403, 400 or 501. This is the same per-server fallback the Reader uses. Cache the result per server.

**Change detection without a feed**
- RFC 6578 `sync-collection` REPORT: send `Depth: 0` with `sync-level` 1 or infinite. An empty token means a full listing. The server may truncate with a 507 on the request-URI. Tokens are opaque, and the server may forget old ones [doc: RFC 6578].
- SabreDAV answers an unknown token with 403, and the client must do a full resync [doc: https://sabre.io/dav/building-a-caldav-client/].
- Nextcloud's per-folder support for sync-collection on `/files/` is not confirmed. My search found no doc either way [belief].
- Nextcloud documents `nc:metadata_etag` (metadata-only changes) but its docs do not mention a ctag [doc: Nextcloud basic API].
- Collection-ETag propagation is the main shortcut. Nextcloud and ownCloud bubble child changes up to parent ETags [belief, matches their sync clients]. Plain Apache or nginx do not, because folder ETags there are mtime-based.
- Design: poll the root with Depth:0 and compare the ETag or sync-token. If unchanged, stop. If changed, descend only into children whose ETag changed. If the server does not propagate, fall back to a full Depth:1 walk.
- Interval: 30–60 s for a changed-root probe, with a full reconcile every 10–30 min [belief].
- Push: none in standard WebDAV. Nextcloud has a notify_push app [belief]. Skip it.

## C. Reads

- Range GET is standard HTTP. Apache, nginx and Nextcloud serve it [belief]. Bunko supports Range and If-Range [src: `crates/bunko-dav/src/methods/read.rs:278`].
- Probe by sending `Range: bytes=0-0` and expecting 206. Without 206, spool the whole file.
- There is no download-URL lifetime. Every request re-authenticates, so there are no signed URLs.
- Throughput is bounded by the server's PHP and proxy stack. Nextcloud front proxies may cap downloads at 1 GB unless `proxy_buffering` is off or `X-Accel-Buffering no` is sent [doc: Nextcloud big-file config].

## D. Writes

**Plain PUT**
- Limits depend on the server: nginx `client_max_body_size`, Apache `LimitRequestBody` (default 1 GiB from 2.4.53), and PHP/proxy timeouts [doc: Nextcloud big-file config, nginx docs].
- nginx PUT writes a temp file and renames it into place, which makes it effectively atomic. It sets mtime from the `Date` header [doc: nginx dav module].
- `create_full_put_path` is off by default, so PUT into a missing parent fails. MKCOL parents first [doc: nginx dav module].

**Chunked and large uploads**
- Nextcloud chunking v2 is at `/remote.php/dav/uploads/<user>` [doc: Nextcloud chunking].
  - MKCOL an upload folder, PUT chunks named `00001`..`10000`, then MOVE `.file` to the destination.
  - Chunks must be 5 MB–5 GB, except the last.
  - `OC-Total-Length` enables an early quota check (507). `X-OC-Mtime` sets mtime. Upload folders expire after 24 h.
- The server chunk-size setting defaults to 100 MiB (`files.chunked_upload.max_size`) [doc: Nextcloud big-file config].
- Only Nextcloud and ownCloud have this scheme. Other servers need a plain PUT. Do not rely on tus [belief].
- A 300 MB `.cbz` over plain PUT works on Nextcloud only if the timeouts allow it. Offer chunking for Nextcloud-flavoured servers only.

**Conditional writes**
- RFC 4918 §7.2 recommends If-Match on modifying requests. Weak ETags cannot be used in If-Match [doc: RFC 4918 §7.2, §8.6].
- Which servers honour `If-Match` and `If-None-Match: *` on PUT: Bunko evaluates If-Match on any method and treats weak tags as equal to strong [src: `crates/bunko-dav/src/conditional.rs`]. SabreDAV honours If-Match [belief]. nginx dav does not [belief]. Probe with a deliberately wrong `If-Match` and see whether you get 412. If the server ignores it, treat writes as last-writer-wins and re-PROPFIND after writing.

**Rename, mkdir, delete**
- MOVE with `Overwrite: T/F`, MKCOL and DELETE are standard. The MOVE and MKCOL status-code details are not verified.
- Nextcloud puts deletes in the trash bin by default [belief].
- reqwest_dav's `mv` sets no Overwrite header [src: https://docs.rs/crate/reqwest_dav/latest/source/src/lib.rs].

## E. Naming semantics

- Case sensitivity comes from the host filesystem, not WebDAV. rclone's case fix-up covers existing files only; case handling for new files depends on the remote [doc: rclone serve webdav page]. No server can be assumed case-insensitive.
- Servers on ext4 or ZFS allow `Naruto/` and `naruto/` side by side. NTFS and APFS hosts do not.
- Nextcloud's forbidden characters and path-length limits are not covered in the docs I read [belief that they follow the Windows-safe set].
- For Bunko:
  - Keep the NFC + lowercase fold key in the SQLite index only. Never fold on the wire.
  - Store the provider's exact path next to the folded key.
  - A case-only rename is MOVE old to new. On a case-insensitive remote, MOVE to the same fold may fail or no-op, so fall back to MOVE to a temp name, then MOVE to the target.
  - If two remote entries fold to the same key, the remote is case-sensitive. Raise a conflict instead of silently merging.
- Unicode: a macOS-hosted server may return NFD names. Normalize on ingest, and send the exact bytes the server listed when addressing a resource. rclone has `--no-unicode-normalization` and `--vfs-block-norm-dupes` for this reason [doc: rclone serve webdav page].

## F. Metadata

- ETag: strong or weak. RFC 4918 §8.6 says it should not change when the body is unchanged.
- getlastmodified: HTTP-date, whole seconds.
- Size: `getcontentlength`.
- Nextcloud adds `oc:fileid`, `oc:checksums` (MD5, SHA1, SHA256, SHA3-256, Adler32) and `X-OC-MTime` for settable mtime. The server stores `OC-Checksum` without validating it on plain PUT [doc: Nextcloud basic API].
- rclone serve webdav derives its ETag from mtime and size by default. `--etag-hash` switches it to a content hash [doc: rclone page].
- Bunko's ETag is `"<mtime:.6f>-<size>"` for files, mtime alone for folders [src: `crates/bunko-dav/src/resource.rs:50`].
- Never assume the ETag is a content hash. Treat it as an opaque change token.

## G. Quotas and rate limits

- No standard rate limit exists. Back off on 429, 503 (honour Retry-After), 502 and 504. A 504 during MOVE after chunking is often an nginx `fastcgi_read_timeout` [doc: Nextcloud big-file config].
- 507 means insufficient storage. It is also the RFC 6578 truncation status on a REPORT (see B).
- 423 means locked. 412 means a precondition failed.
- 401 and 403 are fatal and need a user prompt. Retry 5xx with jittered exponential backoff.
- Do not hammer a self-hosted PHP server. Cap concurrency at 2–4 requests.

## H. Rust options

| Option | Version | Last release | License | Notes |
|---|---|---|---|---|
| `reqwest_dav` | 0.3.3 | 2026-03-02 per crates.io API; docs.rs page showed 2026-09-15 [conflict, unresolved] | MIT OR Apache-2.0 [doc: crates.io] | 42 stars, 1.24 M downloads |
| `opendal` webdav | 0.59.4 | 2026-10-05 [doc: crates.io] | Apache-2.0 | Heavy dependency; no PUT conditionals found [src] |

- **reqwest_dav:** Has get, put, mkcol, mv, cp, delete and list with Depth 0/1/infinity. It has no If-Match, no Range helper, no custom headers on PUT, and no sync-collection [src: reqwest_dav lib.rs, docs.rs]. PROPFIND is a fixed `allprop` body. The docs page says it depends on reqwest 0.13, which would collide with our 0.12 [doc: docs.rs page; unverified in Cargo.toml].
- **opendal:** Capabilities include stat, read with suffix range, conditional reads, write, create_dir, delete, copy, rename and list [src: opendal webdav backend.rs]. Recursive list is commented out as unfinished. Rename sets no Overwrite header and presign is unsupported [src: the same file].
- A `dav-client` crate lookup returned 404.
- **Recommendation: hand-roll on reqwest 0.12 + rustls + quick-xml 0.37**, which we already depend on [src: `Cargo.toml` lines 58 and 66]. We need these, and neither crate gives them to us:
  - Conditional headers on PUT and MOVE.
  - Range reads.
  - A targeted PROPFIND body (etag, size, mtime, resourcetype, plus the `oc:` namespace).
  - sync-collection REPORT.
  - Chunking and server-capability probes.
- The surface is about 8 methods plus multistatus parsing.
- Clean-room risk is low. Bunko's own server crate (`crates/bunko-dav`) is a ready reference for how the multistatus responses look.
- Optionally pin `reqwest_dav` only for type reference. Do not adopt it.

## I. Gotchas for our use

1. **Large `.cbz` files.** A 10–300 MB PUT may hit nginx `client_max_body_size`, PHP timeouts or the Apache body limit. Probe the server's limit (e.g. a test PUT of 1 MB, then try a larger ranged upload). Surface the limit in the UI. Prefer a streamed body so memory stays flat.
2. **Mid-write visibility.** A PUT that is still in flight may be listed with a partial size. Apache and nginx behave differently. Bunko's `X-Mokuro-Put: verified` header is on `.cbz` PUT responses and on every OPTIONS reply. It means a failed PUT leaves the old file in place, so a client may skip its delete-before-reupload [doc: `docs/configuration.md`]. A Bunko-to-Bunko client can read that header from OPTIONS and skip the delete. Other servers must keep the delete-first or write-temp-then-MOVE pattern.
3. **Content-Digest.** Bunko checks `Content-Digest` (RFC 9530, sha-256 or sha-512) on PUT. A mismatch returns 422 `corrupted-in-transit` with `retry` set, for a client to resend [src: `crates/bunko-dav/src/upload.rs`; doc: `docs/configuration.md`]. Send it on every PUT to Bunko. Other servers ignore it, and Nextcloud stores `OC-Checksum` unvalidated.
4. **ETag handling.** Bunko returns a quoted ETag on PUT and GET [src: `put.rs:254`, `read.rs:259`]. Capture the PUT response ETag as the new baseline for If-Match.
5. **Concurrent Reader writes.** Reader and Bunko both write `catalog.json`, `volume-data.json`, `profiles.json` and `goals.json`. Always read-modify-write with If-Match where honoured. After a 412, re-fetch, merge and retry.
6. **Locking.** Nextcloud's transactional lock is server-side and does not block two clients from saving [doc: Nextcloud locking]. Apache needs a `DavLockDB`. Do not depend on LOCK; use ETags.
7. **Many small sidecars.** Per-file GETs are the cost. Use the Depth listing's ETags to fetch only changed JSON and sidecars, and cache them locally. Parallelize carefully (see G).
8. **Windows clients.** The Windows WebClient caps file transfers at 4 GB by default [doc: Nextcloud user manual]. This is not a concern for our own client.
9. **Server fingerprinting.** Detect Nextcloud or ownCloud from the `DAV:` and `OC-` headers and enable chunking, `oc:` properties and `X-OC-Mtime` only there. Detect Bunko from `X-Mokuro-Put` on OPTIONS.
10. **Open questions to probe live.** Does Nextcloud's `/files/` serve sync-collection? Does Koofr, Synology or QNAP support Depth infinity? Which servers honour `If-None-Match: *` and If-Match on PUT? The Koofr docs host did not resolve, so Koofr, Fastmail, box.com, Synology and QNAP specifics are unverified.