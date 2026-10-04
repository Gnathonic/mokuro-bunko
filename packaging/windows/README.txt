Mokuro Bunko @VERSION@ for Windows (@FLAVOR@)
==========================================

A self-hosted manga library server with OCR. One program, no Python, no
installer: unzip the folder anywhere and run it.

GETTING STARTED
---------------
1. Double-click run.bat
2. Your browser opens http://127.0.0.1:8080 once the server is up.
   Create your admin account there.
3. Close the server window to stop the server.

The "full" build runs OCR on this PC. Install the OCR backend once with
    mokuro-bunko.exe install-ocr
(in this folder): it uses an NVIDIA GPU through CUDA (driver 580 or newer,
about 2 GB to download) or the CPU, and goes into data\backends. OCR models
(a few hundred MB) are downloaded into data\models the first time a volume
is OCR'd.

ADDING MANGA
------------
Drop .cbz files (or use any WebDAV client / the web UI) into:
    data\library\<Series Name>\<Volume>.cbz
New volumes are detected automatically and OCR'd in the background.
Progress: http://127.0.0.1:8080/queue

Read your library with Mokuro Reader: https://reader.mokuro.app
(point it at http://127.0.0.1:8080)

FOLDER LAYOUT
-------------
    run.bat           start the server
    doctor.bat        diagnose problems (run this if something doesn't work)
    mokuro-bunko.exe  the server (also: mokuro-bunko.exe --help)
    PORTABLE.txt      keeps everything in data\ (see the file)
    data\             YOUR DATA: library, config.yaml, database, logs,
                      OCR models - back this up if you back up anything

UPDATES
-------
The admin panel shows when a new release is out and can install it with
one click: it downloads the new mokuro-bunko.exe, checks its signature and
checksum, and restarts. Your data\ folder is never touched.

MOVING / BACKUP / UNINSTALL
---------------------------
- Move or copy the whole folder anywhere (USB drive, another PC).
- Back up the data\ folder.
- Uninstall by deleting the folder.

TROUBLESHOOTING
---------------
- Run doctor.bat.
- Server log:          data\logs\server.log
- Volumes failing OCR are listed with their error on the Queue page.

Project: https://github.com/Gnathonic/mokuro-bunko
Licence: MPL-2.0 (LICENSE.txt). Third-party components and their licences:
THIRD-PARTY-LICENSES.md.
