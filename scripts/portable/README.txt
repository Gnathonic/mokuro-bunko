Mokuro Bunko - Portable Edition
================================

A self-hosted manga library server with OCR, in a folder. Everything lives
inside this folder: the Python runtime, the server, the OCR engine and
models, your manga library, configuration, and logs. Nothing is written to
AppData, Program Files, or the registry.

GETTING STARTED
---------------
1. Double-click run.bat
2. The FIRST run downloads Python and ~2 GB of OCR components into this
   folder. This can take several minutes - the window shows progress.
   If this machine has an NVIDIA GPU (with drivers installed), GPU
   acceleration is set up automatically; otherwise CPU mode is used.
3. Your browser opens http://127.0.0.1:8080 automatically once the server
   is ready. Create your admin account there.
4. Close the server window to stop the server.

ADDING MANGA
------------
Drop .cbz files (or use any WebDAV client / the web UI) into:
    data\library\<Series Name>\<Volume>.cbz
The server detects new volumes automatically and OCRs them in the
background. Progress is shown at http://127.0.0.1:8080/queue

Read your library with Mokuro Reader: https://reader.mokuro.app
(point it at http://127.0.0.1:8080)

FOLDER LAYOUT
-------------
    run.bat        start the server
    doctor.bat     diagnose problems (run this if something doesn't work)
    app\           the mokuro-bunko application
    bin\           bundled uv.exe (Python manager)
    runtime\       Python, packages, OCR engine, model caches (created on
                   first run; safe to delete to force a fresh install)
    data\          YOUR DATA: library, config.yaml, database, logs
                   - back this up if you back up anything

MOVING / BACKUP / UNINSTALL
---------------------------
- Move or copy the whole folder anywhere (USB drive, another PC) - it
  keeps working.
- Back up the data\ folder (or the whole folder).
- Uninstall by deleting the folder. That's it.

TROUBLESHOOTING
---------------
- Run doctor.bat - it checks Python, GPU drivers, the OCR engine, disk
  space, and more, with hints for anything wrong.
- Server log:            data\logs\server.log
- Per-volume OCR logs:   data\logs\ocr\<volume>.log
- Volumes failing OCR are listed with their error on the Queue page:
  http://127.0.0.1:8080/queue
- First run needs an internet connection (downloads Python, OCR
  components, and the OCR model). After that it works offline.

Project: https://github.com/Gnathonic/mokuro-bunko
License: MPL-2.0 (bundled uv.exe: MIT/Apache-2.0, see LICENSE-uv.txt)
