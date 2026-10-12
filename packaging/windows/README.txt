Mokuro Bunko @VERSION@ for Windows (@FLAVOR@)
==========================================

A self-hosted manga library server with OCR. One program, no Python, no
installer: unzip the folder anywhere and run it.

GETTING STARTED
---------------
1. Double-click mokuro-bunko.exe: an icon appears by the clock (Windows 11
   may put it under the ^ arrow), and the first time the setup wizard opens in
   your browser. The tray icon has the status, pause/resume, the dashboard,
   settings and "Start at login".
   Or double-click run.bat for the server in a console window instead.
2. Your browser opens http://127.0.0.1:8080 once the server is up.
   Create your admin account there.
3. Quit in the tray menu (or close the console window) stops the server.

The "full" build runs OCR on this PC. Install the OCR backend once with the
setup wizard, or with
    mokuro-bunko-cli install-ocr
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
    mokuro-bunko.exe      the app (tray icon): status, settings; runs the server
    mokuro-bunko-cli.exe  the same program for a terminal: mokuro-bunko-cli --help
    run.bat               start the server in a console window
    doctor.bat            diagnose problems (run this if something doesn't work)
    PORTABLE.txt      keeps everything in data\ (see the file)
    data\             YOUR DATA: library, config.yaml, database, logs,
                      OCR models - back this up if you back up anything

UPDATES
-------
The admin panel shows when a new release is out and can install it with
one click: it downloads the new release, checks its signature and checksum,
replaces mokuro-bunko.exe and mokuro-bunko-cli.exe, and restarts. Your data\
folder is never touched.

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
