# Roadmap

Where Mokuro Bunko is headed after 0.7. These are plans, not promises: the order holds, but
details will change as each release is built. Released work is in [CHANGELOG.md](CHANGELOG.md).

| Version | Theme | In one line |
|---|---|---|
| **0.8** | Remote storage | Point Bunko at your own cloud storage; no server to host. |
| **0.9** | Streaming | Read volumes without downloading and installing them first. |
| **1.0** | Books | EPUB, PDF and other book formats, with OCR for text in images. |

## 0.8 — Remote storage

Bunko learns to work on a library that lives in your cloud storage instead of on its own disk:

- **Google Drive**
- **MEGA**
- **WebDAV** (any WebDAV server, not only another Bunko)
- **OneDrive**

In this mode you don't need to host Bunko anywhere. Run it on any computer, point it at your
cloud storage, and it finds new volumes there, OCRs them and writes the results back next to
them, keeping status files up to date as it goes so the reader can show what is queued, in
progress and done. Mokuro Reader keeps connecting to your cloud storage exactly as it does
today; Bunko never has to be reachable from the internet, or running while you read.

Hosting a Bunko server stays fully supported: remote storage is an additional way to use it,
not a replacement.

## 0.9 — Streaming

Volumes can be stored as a folder of page images instead of a CBZ archive. For those
*streamable* volumes the reader saves where each page lives instead of downloading and
installing the whole volume:

- **No install step**: a volume opens as soon as you pick it.
- **Saves space**: pages are fetched as you read instead of kept on the device.
- **Needs a connection**: a streamed volume can't be read offline.

Any streamable volume can still be installed on the device for offline reading, the same way
volumes are installed today.

## 1.0 — Books

Support for books and book formats: **EPUB**, **PDF**, and more to follow.

Images in a book are OCR'd for burned-in text (scanned pages, illustrations with text), and
the text is stored together with its formatting, so the reader can reproduce the book
faithfully rather than as plain text.
