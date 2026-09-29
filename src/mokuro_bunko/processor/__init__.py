"""The processor half of mokuro-bunko: OCR hardware, no library.

Not to be confused with :mod:`mokuro_bunko.ocr.processor`, which is the
LIBRARY server's per-slot OCR driver. This package is the standalone
process ``mokuro-bunko processor serve`` runs: it holds no library, no
users and no admin UI, and reaches a library server as a client.
"""
