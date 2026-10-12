"""Generate the bunko-sched golden fixtures from the 0.5.2 Python code.

Run from the repository root with the reference environment:

    ~/.cache/mokuro-bunko-demo/ref052/bin/python crates/bunko-sched/tests/golden/gen_golden.py

Every case is drawn from a seeded RNG, fed to the real 0.5.2 functions, and
written as `{inputs, output}` JSON next to this script. `tests/golden.rs`
replays them against the Rust port. Re-running reproduces the same files.
"""

from __future__ import annotations

import json
import math
import random
import sys
import tempfile
import threading
import types
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
sys.path.insert(0, str(ROOT / "src"))

from mokuro_bunko.ocr import congestion as congestion_mod  # noqa: E402
from mokuro_bunko.ocr import eta  # noqa: E402
from mokuro_bunko.ocr import throughput as throughput_mod  # noqa: E402
from mokuro_bunko.ocr import watcher as watcher_mod  # noqa: E402
from mokuro_bunko.ocr.engine_runner import pipeline_verdict, summarize, widen_target  # noqa: E402
from mokuro_bunko.ocr.job_order import natural_key, order_jobs  # noqa: E402
from mokuro_bunko.ocr.processor import OCRProcessor  # noqa: E402
from mokuro_bunko.ocr.volume_outlook import pending_entries, recheck_after  # noqa: E402
from mokuro_bunko.ocr.watcher import OCRWorker  # noqa: E402

# --- a fake clock for every module that reads one ---------------------------

CLOCK = [1_700_000_000.0]
_fake_time = types.SimpleNamespace(time=lambda: CLOCK[0], monotonic=lambda: CLOCK[0])
eta.time = _fake_time
watcher_mod.time = _fake_time
congestion_mod.time = _fake_time


def dump(name: str, data) -> None:
    text = json.dumps(data, ensure_ascii=False, separators=(",", ":"), allow_nan=False)
    (HERE / name).write_text(text, encoding="utf-8")
    print(f"{name}: {len(text.encode('utf-8')) / 1024:.0f} KiB")


def nice(rng: random.Random, lo: float, hi: float) -> float:
    """A float with a few digits, sometimes a whole number."""
    x = rng.uniform(lo, hi)
    r = rng.random()
    if r < 0.3:
        x = float(round(x))
    elif r < 0.6:
        x = round(x, 2)
    return max(lo, x)


# --- natural keys and order -------------------------------------------------

PIECES = [
    "Vol", "vol", "VOLUME", "Volume ", "Chapter", "第", "巻", "話", "章", "集", " ", "-", "_", ".",
    "(", ")", "[", "]", "1", "2", "10", "08", "007", "2.5", "10.25", "10.5", "3.", "１０", "２",
    "一", "二", "三", "十", "十一", "二十", "百二", "千九百", "一〇", "二五", "十十", "二三十", "〇",
    "一番", "十字架", "三国志", "鬼滅の刃", "ワンピース", "ﾜﾝﾋﾟｰｽ", "ß", "STRASSE", "ﬁ", "é", "é",
    "Σ", "σ", "ς", "٣", "٤٥", "३", "①", "Ⅻ", "ｖｏｌ", "Ａ", "a", "B", "c", "Z", "İ", "ǅ",
    "12345678901234567890123", "0", "00", "第三巻", "十一話", "abc", "x2y", "😀", "　",
]


def random_name(rng: random.Random) -> str:
    return "".join(rng.choice(PIECES) for _ in range(rng.randint(1, 5)))


def encode_key(key) -> list:
    return [[kind, str(value), text] for kind, value, text in key]


def gen_natural(rng: random.Random) -> None:
    names = sorted({random_name(rng) for _ in range(500)})
    keys = [encode_key(natural_key(n)) for n in names]
    lists = []
    for _ in range(60):
        items = [random_name(rng) for _ in range(rng.randint(2, 12))]
        ordered = sorted(items, key=lambda n: (natural_key(n), n))
        lists.append({"input": items, "sorted": ordered})
    dump("natural_key.json", {"names": names, "keys": keys, "sorted": lists})

    cases = []
    gens = ["g-1", "g-2", "g-3", "g-10"]
    for _ in range(150):
        series_pool = [random_name(rng) for _ in range(rng.randint(1, 5))]
        vol_pool = [random_name(rng) for _ in range(rng.randint(1, 6))]
        jobs = [
            [rng.choice(series_pool), rng.choice(vol_pool), rng.choice(gens), i]
            for i in range(rng.randint(0, 25))
        ]
        ranked = rng.sample(gens, rng.randint(0, len(gens)))
        rank = {g: i for i, g in enumerate(ranked)}
        last_served = {}
        for g in gens:
            if rng.random() < 0.4:
                last_served[g] = rng.choice(series_pool + [random_name(rng)])
        out = order_jobs(jobs, rank, key=lambda j: (j[0], j[1], j[2]), last_served=last_served)
        cases.append({"jobs": jobs, "rank": rank, "last_served": last_served, "order": [j[3] for j in out]})
    dump("order_jobs.json", cases)


# --- rate model -------------------------------------------------------------

GENS = ["g-1", "g-2", "g-3"]
MACHINES = ["local", "tower", "rig-c", "box"]


def est(e):
    if e is None:
        return None
    return {
        "pages_per_second": e.pages_per_second,
        "source": e.source,
        "volumes_observed": e.volumes_observed,
        "latency_seconds": e.latency_seconds,
    }


def startup_dict(s):
    return {"seconds": s.seconds, "source": s.source, "rough": s.rough}


def tp_dict(t):
    if t is None:
        return None
    return {"pages": t.pages, "seconds": t.seconds, "volumes": t.volumes, "last_at": t.last_at}


def random_congestion(rng: random.Random) -> dict:
    data = {}
    for g in GENS:
        r = rng.random()
        if r < 0.3:
            continue
        if r < 0.35:
            data[g] = "not a list"
            continue
        runs = []
        for _ in range(rng.randint(0, 6)):
            if rng.random() < 0.08:
                runs.append(rng.choice(["junk", 3, None]))
                continue
            run = {}
            if rng.random() < 0.7:
                run["volume_pages"] = rng.choice([rng.randint(1, 250), 0, nice(rng, 1, 250)])
                run["volume_seconds"] = rng.choice([nice(rng, 1, 400), 0.0, -1.0]) if rng.random() < 0.95 else True
            if rng.random() < 0.8:
                run["pages"] = rng.randint(0, 900)
                run["elapsed"] = nice(rng, 0, 500)
            if rng.random() < 0.3:
                run["volume_first"] = rng.choice([True, False, 1, 0])
            runs.append(run)
        data[g] = runs
    return data


def random_bench(rng: random.Random) -> dict:
    data = {}
    for g in GENS:
        if rng.random() < 0.4:
            continue
        row = {}
        if rng.random() < 0.6:
            row["best"] = {"pages_per_second": rng.choice([nice(rng, 0.2, 30), 0, -1, True, "7"])}
        if rng.random() < 0.6:
            row["baseline"] = rng.choice([{"pages_per_second": nice(rng, 0.2, 30)}, "junk", {}])
        if rng.random() < 0.6:
            row["startup_seconds"] = rng.choice([nice(rng, 0.5, 60), 0, -3, None])
        data[g] = row if rng.random() < 0.95 else "junk"
    return data


def random_ops(rng: random.Random, n: int) -> list:
    ops = []
    for _ in range(n):
        CLOCK[0] += rng.uniform(1, 2000)
        r = rng.random()
        g = rng.choice(GENS)
        m = rng.choice(MACHINES)
        key = g if m == "local" or rng.random() < 0.3 else f"{g}@{m}"
        if r < 0.75:
            pages = rng.choice([rng.randint(1, 250), nice(rng, 1, 250), 0, -2, True, "12"]) if rng.random() < 0.15 else rng.randint(2, 250)
            seconds = nice(rng, 0.5, 400) if rng.random() < 0.95 else rng.choice([0, -1.0, None])
            ops.append({"op": "volume", "key": key, "pages": pages, "seconds": seconds,
                        "first": rng.random() < 0.3, "t": CLOCK[0]})
        elif r < 0.93:
            ops.append({"op": "startup", "key": key, "seconds": rng.choice([nice(rng, 0.5, 60), 0.0, -1.0]), "t": CLOCK[0]})
        else:
            ops.append({"op": "forget", "gen": g, "t": CLOCK[0]})
    return ops


def apply_ops(rm: eta.RateModel, ops: list) -> None:
    for op in ops:
        CLOCK[0] = op["t"]
        if op["op"] == "volume":
            rm.record_volume(op["key"], op["pages"], op["seconds"], first_of_session=op["first"])
        elif op["op"] == "startup":
            rm.record_startup(op["key"], op["seconds"])
        else:
            rm.forget(op["gen"])


def make_world(rng: random.Random, n_ops: int) -> tuple[dict, eta.RateModel]:
    CLOCK[0] = 1_700_000_000.0 + rng.uniform(0, 1e6)
    start = CLOCK[0]
    congestion = random_congestion(rng)
    bench = random_bench(rng)
    ops = random_ops(rng, n_ops)
    rm = eta.RateModel(None)
    rm._congestion = lambda: congestion
    rm._bench_file = lambda: bench
    CLOCK[0] = start
    apply_ops(rm, ops)
    machine_bench = []
    for g in GENS:
        for m in MACHINES:
            if rng.random() < 0.35:
                machine_bench.append([g, m, rng.choice([nice(rng, 0.2, 30), None, 0.0]),
                                      rng.choice([nice(rng, 0.5, 60), None, 0.0])])
    world = {"start": start, "congestion": congestion, "bench": bench, "ops": ops,
             "machine_bench": machine_bench, "now": CLOCK[0]}
    return world, rm


def gen_fit(rng: random.Random) -> None:
    cases = []
    for _ in range(300):
        samples = []
        for _ in range(rng.randint(0, 10)):
            p = rng.choice([rng.randint(1, 300), nice(rng, 0.5, 300), 0, -4])
            s = rng.choice([nice(rng, 0.1, 500), 0.0, -1.0]) if rng.random() < 0.1 else nice(rng, 0.1, 500)
            samples.append([p, s])
        if rng.random() < 0.2 and samples:
            # A near-perfect line.
            lat, spp = rng.uniform(0, 5), rng.uniform(0.01, 2)
            samples = [[p, lat + p * spp] for p, _ in samples]
        alpha = rng.choice([0.5, 0.5, 0.5, 0.3, 0.9])
        fit = eta.fit_volume_cost([tuple(s) for s in samples], alpha=alpha)
        cases.append({"samples": samples, "alpha": alpha,
                      "fit": None if fit is None else [fit.latency_seconds, fit.seconds_per_page]})
    for _ in range(100):
        m = rng.choice([rng.randint(-1, 300), 0, 1, 2])
        t = rng.choice([nice(rng, -1, 100), 0.0])
        cases.append({"emission": [m, t], "rate": eta.emission_rate(m, t)})
    dump("fit.json", cases)


def gen_rate_model(rng: random.Random) -> None:
    worlds = []
    for _ in range(32):
        world, rm = make_world(rng, rng.randint(0, 30))
        queries = []
        for _ in range(30):
            g = rng.choice(GENS + ["g-9"])
            m = rng.choice(MACHINES)
            q = rng.random()
            op = rng.choice([0, 0, 1, 3, 4, rng.randint(4, 200)])
            os_ = rng.choice([0.0, nice(rng, 0.1, 300)])
            if q < 0.2:
                queries.append({"q": "rate", "gen": g, "op": op, "os": os_,
                                "out": est(rm.rate(g, observed_pages=op, observed_seconds=os_))})
            elif q < 0.5:
                key = rng.choice([None, g, f"{g}@{m}", f"{g}@{m}"])
                prior = rng.choice([None, nice(rng, 0.2, 30)])
                prior_est = eta.RateEstimate(prior, "bench", 0, 0.0) if prior else None
                queries.append({"q": "rate_on", "gen": g, "key": key, "prior": prior, "op": op, "os": os_,
                                "out": est(rm.rate_on(g, key, machine_prior=prior_est,
                                                      observed_pages=op, observed_seconds=os_))})
            elif q < 0.6:
                queries.append({"q": "startup", "gen": g, "out": startup_dict(rm.startup(g))})
            elif q < 0.75:
                key = rng.choice([None, g, f"{g}@{m}"])
                prior = rng.choice([None, nice(rng, 0.5, 60), 0.0, -1.0])
                queries.append({"q": "startup_on", "gen": g, "key": key, "prior": prior,
                                "out": startup_dict(rm.startup_on(g, key, machine_prior=prior))})
            elif q < 0.8:
                queries.append({"q": "latency", "gen": g, "out": rm.latency(g)})
            elif q < 0.9:
                key = rng.choice([g, f"{g}@{m}"])
                queries.append({"q": "throughput", "key": key, "out": tp_dict(rm.throughput(key))})
            elif q < 0.97:
                within = rng.choice([3600.0, 6 * 3600.0, 1e9, 10.0])
                queries.append({"q": "evidence", "gen": g, "within": within,
                                "out": rm.machines_with_evidence(g, within=within)})
            else:
                queries.append({"q": "report", "gen": g, "out": rm.report(g)})
        world["queries"] = queries
        worlds.append(world)
    dump("rate_model.json", worlds)


# --- earliest finish --------------------------------------------------------


def gen_eft(rng: random.Random) -> None:
    cases = []
    for _ in range(200):
        n = rng.randint(1, 5)
        machines = [rng.choice(MACHINES) for _ in range(n)]
        lanes = []
        for i in range(n):
            lanes.append(eta.EftLane(
                key=i, machine=machines[i],
                free_in=rng.choice([0.0, 0.0, nice(rng, 0, 600)]),
                warm=rng.choice([None, None] + GENS),
                rows=frozenset(rng.sample(GENS, rng.randint(0, len(GENS)))),
            ))
        rates = {}
        startups = {}
        for g in GENS:
            for m in sorted(set(machines)):
                rates[(g, m)] = None if rng.random() < 0.08 else [nice(rng, 0.2, 20), rng.choice([0.0, nice(rng, 0, 10)])]
                startups[(g, m)] = nice(rng, 0, 40)
        jobs = []
        for j in range(rng.randint(0, 30)):
            pages = rng.choice([rng.randint(1, 300)] * 6 + [None, 0, -3])
            jobs.append((f"j{j}", rng.choice(GENS), pages))
        asking = rng.choice(list(range(n)) + ([99] if rng.random() < 0.05 else []))
        params = {}
        if rng.random() < 0.2:
            params = {"margin": rng.choice([0.0, 0.2]), "margin_cap": rng.choice([1.0, 10.0]),
                      "slack": rng.choice([0.0, 5.0]), "limit": rng.choice([0, 3, 10])}

        def rate_for(g, m):
            r = rates[(g, m)]
            return None if r is None else eta.RateEstimate(r[0], "t", 0, r[1])

        d = eta.earliest_finish_claim(jobs, lanes, asking, rate_for=rate_for,
                                      startup_for=lambda g, m: startups[(g, m)], **params)
        out = None if d is None else {
            "mine": d.mine,
            "left": [[l.job, l.to, l.there, l.here, l.starts, l.busy_first] for l in d.left],
        }
        cases.append({
            "lanes": [[l.key, l.machine, l.free_in, l.warm, sorted(l.rows)] for l in lanes],
            "rates": [[g, m, r] for (g, m), r in rates.items()],
            "startups": [[g, m, s] for (g, m), s in startups.items()],
            "jobs": [list(j) for j in jobs], "asking": asking, "params": params, "out": out,
        })
    dump("eft.json", cases)


# --- queue plan -------------------------------------------------------------


def lane_pricing(rm: eta.RateModel, machine_bench: list):
    table = {(g, m): (p, s) for g, m, p, s in machine_bench}

    def rate_for(generation_id, machine=None, *, observed_pages=0, observed_seconds=0.0):
        bench = table.get((generation_id, machine)) if machine is not None else None
        prior = bench[0] if bench else None
        return rm.rate_on(
            generation_id,
            None if machine is None else OCRWorker._rate_key(generation_id, machine),
            machine_prior=eta.RateEstimate(prior, "bench", 0, 0.0) if prior is not None and prior > 0 else None,
            observed_pages=observed_pages, observed_seconds=observed_seconds,
        )

    def startup_for(generation_id, machine=None):
        bench = table.get((generation_id, machine)) if machine is not None else None
        return rm.startup_on(
            generation_id,
            None if machine is None else OCRWorker._rate_key(generation_id, machine),
            machine_prior=bench[1] if bench else None,
        )

    return rate_for, startup_for


def random_card(rng: random.Random, now: float, i: int) -> dict:
    card = {"series": f"S{rng.randint(0, 3)}", "volume": f"V{i}"}
    g = rng.choice(GENS)
    r = rng.random()
    if r < 0.85:
        card["generation_id"] = g
    elif r < 0.92:
        card["generation_id"] = rng.choice(["", None])
    card["generation"] = f"name-{g}"
    r = rng.random()
    if r < 0.75:
        card["slot"] = rng.randint(0, 4)
    elif r < 0.8:
        card["slot"] = rng.choice(["x", 1.0, True, -1, 12])
    r = rng.random()
    if r < 0.8:
        card["machine"] = rng.choice(MACHINES)
    elif r < 0.85:
        card["machine"] = rng.choice(["", 5])
    card["status"] = rng.choice(["running", "running", "starting", "error", "done", "finalizing", None])
    total = rng.choice([rng.randint(1, 300)] * 5 + [None, 0, 12.7])
    if total is not None or rng.random() < 0.5:
        card["total_pages"] = total
    done = rng.choice([0, 0, rng.randint(0, 300), 1, 2, 5])
    if isinstance(total, int) and total > 0 and rng.random() < 0.7:
        done = rng.randint(0, total)
    card["done_pages"] = done
    if done > 0 and rng.random() < 0.9:
        card["first_page_at"] = now - nice(rng, 0, 300)
    elif rng.random() < 0.2:
        card["first_page_at"] = rng.choice([0, None])
    if rng.random() < 0.7:
        card["session_started_at"] = now - nice(rng, 0, 120)
    card["started_at"] = now - nice(rng, 0, 200)
    r = rng.random()
    if r < 0.4:
        card["session_ready"] = True
    elif r < 0.7:
        card["session_ready"] = False
    elif r < 0.75:
        card["session_ready"] = 1
    if rng.random() < 0.3:
        card["eta_seconds"] = rng.randint(0, 100)
    card["percent"] = rng.randint(0, 99)
    return card


def random_item(rng: random.Random, i: int) -> dict:
    g = rng.choice(GENS + (["g-9"] if rng.random() < 0.05 else []))
    item = {"series": f"S{rng.randint(0, 5)}", "volume": f"V{i}"}
    r = rng.random()
    if r < 0.9:
        item["generation"] = f"name-{g}"
    elif r < 0.95:
        item["generation"] = ""
    item["generation_id"] = g if rng.random() < 0.97 else ""
    item["engine"] = "hayai"
    item["detector"] = None
    item["pages"] = rng.choice([rng.randint(1, 400)] * 6 + [None, 0, -2, 33.9])
    if rng.random() < 0.2:
        item["attempts"] = rng.randint(1, 4)
    return item


def gen_plan(rng: random.Random) -> None:
    cases = []
    for c in range(75):
        world, rm = make_world(rng, rng.randint(0, 18))
        now = world["now"] + rng.uniform(0, 100)
        CLOCK[0] = now
        rate_for, startup_for = lane_pricing(rm, world["machine_bench"])
        if rng.random() < 0.7:
            lane_machines = [rng.choice(MACHINES) for _ in range(rng.randint(1, 5))]
        else:
            lane_machines = None if rng.random() < 0.8 else []
        lane_count = rng.randint(0, 3)
        running = [random_card(rng, now, i) for i in range(rng.randint(0, 5))]
        pending = [random_item(rng, i) for i in range(rng.randint(0, 16))]
        refusals = {}
        if rng.random() < 0.4:
            for g in GENS:
                for m in MACHINES:
                    if rng.random() < 0.3:
                        refusals[f"{g}|{m}"] = f"{m} cannot run {g}"
        holds = {}
        if rng.random() < 0.5:
            for g in GENS:
                if rng.random() < 0.5:
                    holds[g] = rng.choice([f"held {g}", "", None])
        use_refusal = bool(refusals) or rng.random() < 0.2
        use_hold = bool(holds) or rng.random() < 0.3
        through = None if rng.random() < 0.7 else rng.randint(-1, len(pending) + 1)
        plan = eta.plan_queue(
            running, pending, lane_count=lane_count, rate_for=rate_for, startup_for=startup_for,
            now=now, lane_machines=lane_machines, through=through,
            refusal_for=(lambda g, m: refusals.get(f"{g}|{m}")) if use_refusal else None,
            hold_for=(lambda g: holds.get(g)) if use_hold else None,
        )
        cases.append({
            "world": world, "now": now, "lane_machines": lane_machines, "lane_count": lane_count,
            "running": running, "pending": pending,
            "refusals": refusals if use_refusal else None, "holds": holds if use_hold else None,
            "through": through,
            "out": {"running": plan.running, "pending": plan.pending,
                    "done_in": plan.done_in, "done_at": plan.done_at},
        })
    dump("plan.json", cases)


# --- congestion -------------------------------------------------------------

STAGE_KEYS = ["detect", "engine", "post", "ocr"]


def random_raw(rng: random.Random):
    r = rng.random()
    if r < 0.04:
        return rng.choice([None, "x", [], {"stages": []}, {"stages": "x"}, {"stages": [1, "a"]}])
    elapsed = rng.choice([nice(rng, 0.5, 60)] * 8 + [0, None, "3", True])
    raw = {"elapsed_seconds": elapsed, "items": rng.choice([rng.randint(0, 200), None, 7.9])}
    keys = rng.sample(STAGE_KEYS, rng.randint(1, 4))
    stages = []
    for k in keys:
        workers = rng.choice([1, 1, 2, 4, 0, 0, 3.7, "2", None])
        e = elapsed if isinstance(elapsed, (int, float)) and not isinstance(elapsed, bool) else 10.0
        w = max(1, int(workers) if isinstance(workers, (int, float)) and not isinstance(workers, bool) else 1)
        pool = w * (e or 1)
        st = {"key": k, "workers": workers,
              "items": rng.choice([rng.randint(0, 200)] * 5 + [rng.randint(8, 50), None]),
              "busy_seconds": rng.uniform(0, 1.1) * pool,
              "blocked_seconds": rng.choice([0.0, rng.uniform(0, 0.5) * pool, rng.uniform(0, 0.3) * pool]),
              "starved_seconds": rng.choice([0.0, rng.uniform(0, 0.5) * pool, rng.uniform(0, 0.3) * pool])}
        if rng.random() < 0.6:
            st["device"] = rng.choice(["cpu", "cuda:0", "rocm:0", 5])
        if rng.random() < 0.5:
            st["name"] = rng.choice([k.title(), None, 3])
        if rng.random() < 0.4:
            st["device_bound"] = rng.choice([True, False, 1])
        if rng.random() < 0.03:
            st = rng.choice(["junk", {"key": ""}, {"workers": 1}])
        stages.append(st)
    raw["stages"] = stages
    queues = []
    for a, b in zip(keys, keys[1:]):
        if rng.random() < 0.85:
            queues.append({"name": f"{a}->{b}", "capacity": rng.choice([2, 4, 8, 0, None]),
                           "mean_depth": rng.choice([nice(rng, 0, 8), None]),
                           "max_depth": rng.choice([rng.randint(0, 8), 2.5])})
    if queues and rng.random() < 0.1:
        queues.append(dict(queues[0], capacity=99))
    if rng.random() < 0.05:
        queues.append("junk")
    raw["queues"] = queues
    if rng.random() < 0.5:
        raw["bottleneck"] = rng.choice(keys + ["nope", None])
    return raw


def random_summary_rows(rng: random.Random) -> dict:
    """A summary-shaped dict with percentages given directly."""
    stages = []
    for k in rng.sample(STAGE_KEYS, rng.randint(1, 4)):
        st = {"key": k, "workers": rng.choice([1, 2, 0, 4]), "items": rng.choice([rng.randint(0, 100), 8, 7]),
              "busy_pct": rng.choice([rng.uniform(0, 100), 85.0, 84.9, rng.randint(0, 100)]),
              "blocked_pct": rng.choice([rng.uniform(0, 60), 15.0, 14.5, None, 0]),
              "starved_pct": rng.choice([rng.uniform(0, 60), 15.0, 14.5, None, 0]),
              "device": rng.choice(["cpu", "cuda:0"]),
              "fused": rng.random() < 0.2, "device_bound": rng.random() < 0.3}
        if rng.random() < 0.7:
            st["queue"] = {"name": f"{k}->x", "capacity": 4}
        stages.append(st)
    return {"elapsed_seconds": nice(rng, 0, 50), "items": rng.randint(0, 100), "stages": stages}


def gen_congestion(rng: random.Random) -> None:
    summaries = []
    for _ in range(140):
        raw = random_raw(rng)
        summaries.append({"raw": raw, "out": summarize(raw)})
    verdicts = []
    for _ in range(160):
        s = random_summary_rows(rng)
        verdicts.append({"summary": s, "verdict": pipeline_verdict(s), "widen": widen_target(s)})
    events = []
    for _ in range(50):
        r = rng.random()
        if r < 0.4:
            stats = random_raw(rng)
        elif r < 0.8:
            base = summarize(random_raw(rng)) or random_summary_rows(rng)
            stats = dict(base)
            if rng.random() < 0.5:
                stats.pop("verdict", None)
            elif rng.random() < 0.5:
                stats["verdict"] = None
            for k in ("elapsed_seconds", "items"):
                if rng.random() < 0.3:
                    stats.pop(k, None)
        else:
            stats = rng.choice([None, 5, {"stages": []}, {"stages": [{"busy_pct": 3}, {"busy_seconds": 1}]}])
        events.append({"stats": stats, "out": congestion_mod.summarize_event_stats(stats)})
    records = []
    record_pool = []
    for _ in range(70):
        s = summarize(random_raw(rng)) or random_summary_rows(rng)
        vp = rng.choice([None, 0, rng.randint(1, 200)])
        vs = rng.choice([None, 0.0, nice(rng, 0.5, 300)])
        first = rng.random() < 0.3
        at = 1_700_000_000.0 + rng.uniform(0, 1e6)
        rec = congestion_mod.build_record(s, volume=f"S/V{len(records)}.cbz", at=at, volume_pages=vp,
                                          volume_seconds=vs, volume_first=first)
        records.append({"summary": s, "volume": f"S/V{len(records)}.cbz", "at": at, "volume_pages": vp,
                        "volume_seconds": vs, "volume_first": first, "out": rec})
        record_pool.append(rec)
    averages = []
    for _ in range(60):
        runs = [rng.choice(record_pool) for _ in range(rng.randint(0, 5))]
        if rng.random() < 0.1:
            runs.append(rng.choice(["junk", {"stages": []}, {"at": 5}]))
        if rng.random() < 0.1 and runs and isinstance(runs[0], dict):
            runs[0] = dict(runs[0], at=0)
        averages.append({"runs": runs, "out": congestion_mod.average_runs(runs)})
    dump("congestion.json", {"summarize": summaries, "verdict": verdicts, "events": events,
                             "records": records, "average": averages})


# --- small pure helpers -----------------------------------------------------


def random_json_value(rng: random.Random, depth: int = 0):
    r = rng.random()
    if depth < 3 and r < 0.25:
        return {random_name(rng) + rng.choice(["", "\n", "\t", '"', "\\", "\x01", "\x7f", "é"]): random_json_value(rng, depth + 1)
                for _ in range(rng.randint(0, 4))}
    if depth < 3 and r < 0.4:
        return [random_json_value(rng, depth + 1) for _ in range(rng.randint(0, 4))]
    r = rng.random()
    if r < 0.2:
        return rng.choice([None, True, False])
    if r < 0.4:
        return rng.randint(-10**12, 10**12)
    if r < 0.75:
        return rng.choice([
            rng.uniform(-1e6, 1e6), 1e16, 1e15, 1e-5, 0.0001, -0.0, 0.0, 1.0, 3.0,
            1_727_000_000.123456, rng.uniform(0, 1) * 10 ** rng.randint(-30, 30), 5e-324,
            float(rng.randint(0, 10**17)), 2.5, 1e22, 123456789012345680.0,
        ])
    return random_name(rng) + rng.choice(["", "\b\f\r", " ", "\x00"])


def gen_misc(rng: random.Random) -> None:
    progress = []
    for _ in range(300):
        done = rng.choice([0, -1, rng.randint(0, 300), 1, 2])
        total = rng.choice([0, -5, rng.randint(1, 300), done])
        elapsed = rng.choice([0.0, nice(rng, 0, 300)])
        rate = rng.choice([None, 0.0, -1.0, nice(rng, 0.01, 30)])
        progress.append({"in": [done, total, elapsed, rate],
                         "out": list(OCRProcessor._progress_metrics(done, total, elapsed, rate))})
    recheck = []
    for _ in range(200):
        now = 1_700_000_000.0 + rng.uniform(0, 1e6)
        pending = []
        for _ in range(rng.randint(0, 5)):
            r = rng.random()
            if r < 0.6:
                eta_s = eta.iso_utc(now + rng.uniform(-200, 8000))
            elif r < 0.7:
                eta_s = eta.iso_utc(now + rng.uniform(-200, 8000)).replace("Z", "+02:00")
            elif r < 0.8:
                eta_s = rng.choice(["garbage", "2024-13-01T00:00:00Z", "", "2024-02-30"])
            else:
                eta_s = None
            pending.append({"kind": "ocr", "id": "x", "eta": eta_s})
        recheck.append({"pending": pending, "now": now, "out": recheck_after(pending, now)})
    isos = []
    for _ in range(300):
        base = rng.choice([0.0, 1_700_000_000.0, 951_782_400.0, 4_102_444_800.0, 1e10])
        x = base + rng.choice([rng.uniform(0, 1e8), rng.randint(0, 10**8) + 0.9999996,
                               rng.randint(0, 10**8) + 0.9999994, rng.randint(0, 10**8) + 0.0000005])
        isos.append([x, eta.iso_utc(x)])
    outlook = []
    for _ in range(100):
        owed = [types.SimpleNamespace(id=g, name=f"name-{g}", primary=(i == 0 and rng.random() < 0.7))
                for i, g in enumerate(rng.sample(GENS, rng.randint(0, 3)))]
        planned = []
        for _ in range(rng.randint(0, 6)):
            planned.append({"series": rng.choice(["S", "T"]), "volume": rng.choice(["V", "W"]),
                            "generation_id": rng.choice(GENS + [None, 5]),
                            "eta_at": rng.choice([None, "2024-01-01T00:00:00Z", "2025-01-01T00:00:00Z", 7])})
        outlook.append({"owed": [[r.id, r.name, r.primary] for r in owed], "planned": planned,
                        "out": pending_entries(owed, "S", "V", planned)})
    rounds = []
    for _ in range(400):
        x = rng.choice([rng.uniform(-1000, 1000), rng.randint(-100, 100) + 0.5, rng.uniform(0, 1) * 10 ** rng.randint(-8, 12),
                        2.675, 0.125, 84.5, 15.05])
        n = rng.choice([0, 1, 2, 4])
        rounds.append([x, n, round(x, n), round(x), f"{x:.0f}", f"{x:.1f}", f"{x / 100:.0%}", repr(x)])
    sums = []
    for _ in range(150):
        vals = [rng.choice([rng.uniform(-1e3, 1e3), rng.uniform(0, 1) * 10 ** rng.randint(-10, 20), 0.1])
                for _ in range(rng.randint(0, 12))]
        sums.append([vals, sum(vals)])
    medians = []
    for _ in range(100):
        vals = [rng.randint(1, 500) for _ in range(rng.randint(1, 9))]
        import statistics
        medians.append([vals, int(round(statistics.median(vals)))])
    dumps = []
    for _ in range(150):
        v = random_json_value(rng)
        dumps.append([v, json.dumps(v, ensure_ascii=False, indent=2)])
    retry = []
    for _ in range(80):
        poll = rng.choice([30.0, 5.0, 0.5, 600.0, 7.3])
        attempts = rng.choice([-1, 0, 1, 2, 3, 4, 5, 17, 18, 500, 10**6])
        fake = types.SimpleNamespace(poll_interval=poll)
        retry.append([poll, attempts, OCRWorker._retry_delay_seconds(fake, attempts)])
    busy = []
    for _ in range(100):
        ev = {}
        if rng.random() < 0.7:
            ev["cpu_pressure"] = rng.choice([rng.uniform(0, 1), 0.6, 0.595, 0.605, True, "0.9"])
        if rng.random() < 0.7:
            ev["other_cpu"] = rng.choice([rng.uniform(0, 1), 0.5, 0.495, 0.005])
        busy.append([ev, OCRWorker._busy_reason(ev)])
    dump("misc.json", {"progress": progress, "recheck": recheck, "iso": isos, "outlook": outlook,
                       "round": rounds, "sum": sums, "median": medians, "dumps": dumps,
                       "retry": retry, "busy": busy})


def gen_throughput(rng: random.Random) -> None:
    cases = []
    for _ in range(150):
        samples = []
        for _ in range(rng.randint(0, 8)):
            s = [rng.choice([rng.randint(1, 200), 0, -1, None, True]) if rng.random() < 0.1 else rng.randint(1, 200),
                 nice(rng, 0.5, 100)]
            if rng.random() < 0.6:
                s.append(rng.choice([1000.0 + rng.uniform(0, 300), None]))
            samples.append(s)
        last_at = rng.choice([None, 12.5])
        cases.append({"samples": samples, "last_at": last_at,
                      "out": tp_dict(throughput_mod.throughput_of(samples, last_at=last_at))})
    profiles = []
    for _ in range(120):
        runs = {}
        if rng.random() < 0.5:
            runs["recent"] = [
                {"pages": rng.choice([rng.randint(1, 200), 0]), "seconds": nice(rng, 0.5, 100),
                 "at": rng.choice([1000.0 + rng.uniform(0, 500), None])}
                for _ in range(rng.randint(0, 25))
            ]
        if rng.random() < 0.5:
            runs["congestion"] = [
                {"volume_pages": rng.choice([rng.randint(1, 200), None, 0]), "volume_seconds": nice(rng, 0.5, 90),
                 "at": rng.choice([2000.0 + rng.uniform(0, 500), None])}
                for _ in range(rng.randint(0, 25))
            ]
        if rng.random() < 0.6:
            runs["pages"] = rng.choice([rng.randint(1, 5000), 0])
            runs["seconds"] = rng.choice([nice(rng, 1, 5000), 0])
            runs["volumes"] = rng.choice([rng.randint(1, 50), 0, None, 2.7])
        if rng.random() < 0.5:
            runs["last_at"] = rng.choice([None, 0, 3000.5])
        if rng.random() < 0.05:
            runs = rng.choice([None, "x"])
        profiles.append({"runs": runs, "out": tp_dict(throughput_mod.profile_throughput(runs))})
    dump("throughput.json", {"of": cases, "profile": profiles})


# --- worker helpers run on a stand-in worker ---------------------------------


def fake_worker(storage: Path, rows: list, rates=None, poll: float = 30.0):
    fake = types.SimpleNamespace()
    fake.storage_path = storage
    fake.generations = rows
    fake.poll_interval = poll
    fake._lock = threading.RLock()
    fake._failures_path = storage / ".ocr-failures.json"
    fake.queue_state = types.SimpleNamespace(bump=lambda: None)
    fake._rel_paths = {}
    fake._log = lambda *a, **k: None
    fake.rates = rates
    fake.failure_key = OCRWorker.failure_key
    fake._rate_key = OCRWorker._rate_key
    for name in ("_load_failures", "_save_failures", "_save_failures_file", "_rel_library_path",
                 "_retry_delay_seconds", "_prune_failure_records", "_record_ocr_failure"):
        setattr(fake, name, types.MethodType(getattr(OCRWorker, name), fake))
    return fake


def row(gen_id: str, name: str, primary: bool, enabled: bool = True, detector=None):
    return types.SimpleNamespace(id=gen_id, name=name, primary=primary, enabled=enabled,
                                 engine="hayai", reported_detector=detector)


def gen_worker(rng: random.Random) -> None:
    records = []
    for _ in range(30):
        with tempfile.TemporaryDirectory() as tmp:
            storage = Path(tmp)
            rows = [row("g-1", "main", True), row("g-2", "fast ✓", False, detector="rtdetr")]
            fake = fake_worker(storage, rows, poll=rng.choice([30.0, 5.0]))
            steps = []
            for _ in range(rng.randint(1, 6)):
                CLOCK[0] = 1_700_000_000.0 + rng.uniform(0, 1e6)
                gen = rng.choice(rows)
                series = rng.choice(["", "Series A", "シリーズ", "S/Sub"])
                vol = rng.choice(["Vol 1", "第2巻", "x\"y"])
                path = storage / "library" / series / f"{vol}.cbz" if series else storage / "library" / f"{vol}.cbz"
                err = rng.choice([None, "boom", "ünïcödé\nline2", "x" * 50])
                log = rng.choice([None, "/logs/ocr/a.log"])
                failure = types.SimpleNamespace(error=err, log_file=log) if err is not None else None
                if failure is None:
                    fake.processor = types.SimpleNamespace(last_failure=None)
                fake._record_ocr_failure(path, gen, failure)
                steps.append({"series": series, "volume": vol, "gen": gen.id, "error": err, "log": log,
                              "now": CLOCK[0]})
            text = fake._failures_path.read_text(encoding="utf-8")
            records.append({"poll": fake.poll_interval, "steps": steps, "file": text})
    prunes = []
    for _ in range(40):
        with tempfile.TemporaryDirectory() as tmp:
            storage = Path(tmp)
            library = storage / "library"
            library.mkdir()
            files = []
            for _ in range(rng.randint(0, 4)):
                series = rng.choice(["", "A", "B"])
                ext = rng.choice([".cbz", ".cbr", ".zip", ".rar", ".txt"])
                rel = f"{series}/V{rng.randint(0, 3)}{ext}" if series else f"V{rng.randint(0, 3)}{ext}"
                (library / rel).parent.mkdir(parents=True, exist_ok=True)
                (library / rel).write_bytes(b"x")
                files.append(rel)
            failures = {}
            for i in range(rng.randint(0, 6)):
                rec = {"series": rng.choice(["", "A", "B", None, 3]),
                       "volume": rng.choice([f"V{rng.randint(0, 3)}", "", None]),
                       "generation": rng.choice(["main", "fast", "gone", "", None, 4]),
                       "attempts": rng.randint(1, 5), "last_attempt_at": 1.5}
                failures[f"k{i}@x"] = rec
            if rng.random() < 0.1:
                failures["junk"] = "not a record"
            source = json.dumps(failures, ensure_ascii=False, indent=2)
            (storage / ".ocr-failures.json").write_text(source, encoding="utf-8")
            rows = [row("g-1", "main", True), row("g-2", "fast", False)]
            fake = fake_worker(storage, rows)
            fake._prune_failure_records()
            path = storage / ".ocr-failures.json"
            prunes.append({"files": files, "names": [r.name for r in rows], "input": source,
                           "file": path.read_text(encoding="utf-8") if path.exists() else None})
    speeds = []
    for _ in range(30):
        world, rm = make_world(rng, rng.randint(0, 30))
        CLOCK[0] = world["now"] + rng.uniform(0, 3600)
        rows = [row(g, f"name-{g}", i == 0, enabled=rng.random() < 0.85) for i, g in enumerate(GENS)]
        running = []
        for i in range(rng.randint(0, 5)):
            card = {"generation_id": rng.choice(GENS + [None]),
                    "status": rng.choice(["running", "starting", "finalizing", None])}
            if rng.random() < 0.8:
                card["machine"] = rng.choice(MACHINES + [""])
            running.append(card)
        within = rng.choice([6 * 3600.0, 600.0, 1e9])
        fake = fake_worker(Path("/nonexistent"), rows, rates=rm)
        out = OCRWorker.speed_report(fake, running, within=within)
        speeds.append({"world": world, "clock": CLOCK[0],
                       "rows": [[r.id, r.name, r.enabled] for r in rows],
                       "running": running, "within": within, "out": out})
    dump("worker.json", {"records": records, "prunes": prunes, "speed": speeds})


def main() -> None:
    gen_natural(random.Random(1))
    gen_fit(random.Random(2))
    gen_rate_model(random.Random(3))
    gen_eft(random.Random(4))
    gen_plan(random.Random(5))
    gen_congestion(random.Random(6))
    gen_misc(random.Random(7))
    gen_throughput(random.Random(8))
    gen_worker(random.Random(9))
    total = sum(p.stat().st_size for p in HERE.glob("*.json"))
    print(f"total: {total / 1024:.0f} KiB")


if __name__ == "__main__":
    main()
