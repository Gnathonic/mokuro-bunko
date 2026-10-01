"""Time the Python 0.5.2 reader on the golden pages, the way ppocr_golden.rs times
Rust: read_page, and read_lines with the layout's own time excluded. Run it right
before/after the Rust test so both see the same machine load.

    GOLDEN_THREADS=4 ~/.cache/mokuro-bunko-demo/ref052-ocr/bin/python \
        crates/bunko-ocr/tests/golden/bench_ppocr_python.py
"""

import json
import sys
import time

import gen_ppocr_golden as g  # noqa: E402  (sets the env and sys.path)


class TimedLayout:
    def __init__(self, layout):
        self.layout = layout
        self.spent = 0.0

    def __getattr__(self, name):
        fn = getattr(self.layout, name)

        def wrapped(*a, **k):
            t = time.perf_counter()
            try:
                return fn(*a, **k)
            finally:
                self.spent += time.perf_counter() - t

        return wrapped


def no_spin() -> None:
    """Disable ORT thread spinning (GOLDEN_NOSPIN=1), matching BUNKO_GOLDEN_NOSPIN."""
    import os

    if not os.environ.get("GOLDEN_NOSPIN"):
        return
    import onnxruntime as ort

    orig = g.ppocr.PPOcr._session

    def session(self, path):
        options = ort.SessionOptions()
        options.intra_op_num_threads = self.threads
        options.log_severity_level = 3
        options.add_session_config_entry("session.intra_op.allow_spinning", "0")
        options.add_session_config_entry("session.inter_op.allow_spinning", "0")
        return ort.InferenceSession(str(path), options, providers=["CPUExecutionProvider"])

    g.ppocr.PPOcr._session = session if orig else orig


def main() -> int:
    no_spin()
    entries = json.loads((g.HERE / "ppocr_pages.json").read_text(encoding="utf-8"))
    layout = TimedLayout(g.line_layout)
    reader = g.engine_runner.PPOcrPageReader(g.ppocr, layout)
    imgs = [(e["id"], g.load_page(e)[0]) for e in entries]
    reader.engine.read_page(imgs[0][1])  # warm-up: session creation
    tot_p = tot_l = cpu_p = 0.0
    for pid, img in imgs:
        c = time.process_time()
        t = time.perf_counter()
        reader.engine.read_page(img)
        tp = time.perf_counter() - t
        cpu_p += time.process_time() - c
        layout.spent = 0.0
        t = time.perf_counter()
        reader.read_lines(img)
        tl = time.perf_counter() - t - layout.spent
        tot_p += tp
        tot_l += tl
        print(f"{pid:<26} read_page {tp:.3f} read_lines {tl:.3f}", flush=True)
    print(f"TOTAL threads={g.THREADS} pages={len(imgs)} read_page {tot_p:.2f}s read_lines {tot_l:.2f}s; "
          f"read_page CPU {cpu_p:.2f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
