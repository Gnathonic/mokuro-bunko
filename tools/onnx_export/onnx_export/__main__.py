"""``python -m onnx_export <step>``: the whole models-v1 pipeline, or one step of it.

Steps, in the order ``all`` runs them:
  hayai          export hayai-nova (fp32 + fp16) and its tables
  paddle         export paddle-manga (fp32 + fp16) and its tables
  ppocr          copy the PP-OCRv6 manga ONNX files from the pin
  parity-hayai   torch (0.5.2 runner) vs ORT; fails below 220/220 at fp32
  parity-paddle  torch (0.5.2 runner) vs ORT; fails below 100/100 at fp32
  manifest       write models.json (refuses without passing parity reports)
Every step takes ``--out DIR`` (default ~/.cache/mokuro-bunko-demo/models-v1).
"""

from __future__ import annotations

import sys


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    step, rest = argv[0], argv[1:]
    if step == "hayai":
        from .hayai.export import main as m
    elif step == "paddle":
        from .paddle.export import main as m
    elif step == "ppocr":
        from .ppocr import main as m
    elif step == "parity-hayai":
        from .hayai.check_parity import main as m
    elif step == "parity-paddle":
        from .paddle.check_parity import main as m
    elif step == "manifest":
        from .build_manifest import main as m
    elif step == "all":
        for s in ("hayai", "paddle", "ppocr", "parity-hayai", "parity-paddle", "manifest"):
            print(f"== {s}", file=sys.stderr, flush=True)
            rc = main([s, *rest])
            if rc:
                return rc
        return 0
    else:
        print(f"unknown step {step!r}\n{__doc__}", file=sys.stderr)
        return 2
    return m(rest) or 0


if __name__ == "__main__":
    sys.exit(main())
