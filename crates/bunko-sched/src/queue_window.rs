//! Which planned jobs the reader's queue file lists, and in what order: every running
//! job, then the jobs claimed but not started, then the next `limit` waiting jobs by
//! predicted finishing time, grouped by volume.
//!
//! A volume carries only its listed jobs: running first, then soonest first. Volumes
//! come in the order of their soonest job, those with a running job first. A job no
//! connected machine can run (a row hold: `held` on the plan entry) has no finishing
//! time, so it sorts after every timed job; within the `limit` it is listed like any
//! other waiting job, and may bring its volume in. Past the `limit` it is attached
//! only to a volume listed anyway, so that volume's card still says why the job
//! waits; there it adds no volume.
//!
//! Finishing times are compared in [`ETA_BUCKET_SECONDS`] buckets, with plan order as
//! the tie-break. The plan re-prices every job whenever a rate or a running card's
//! progress moves, and two jobs a few seconds apart would swap places on every such
//! move; the buckets keep the file (and its ETag) still until a job's finishing time
//! moves by minutes.
//!
//! While the whole queue is held the window ignores finishing times (`timed: false`)
//! and follows plan order alone. The plan still prices waiting jobs on the held
//! machines' idle lanes, at "now + their run time", so every finishing time moves with
//! the clock. Jobs on two lanes would then cross bucket boundaries in turn, and the
//! window would reorder itself every few minutes while nothing in the queue changed.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::plan::QueuePlan;
use crate::py::{Object, parse_iso_timestamp, truthy};

/// Finishing times closer than this keep plan order.
pub const ETA_BUCKET_SECONDS: i64 = 300;

/// Why a job is listed. The order is the sort order: running jobs first, then the
/// claimed ones about to start, then the waiting ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Listed {
    Running,
    /// Claimed by a machine that has not started it yet (it is fetching the archive or
    /// opening its session). Not timed, and outside the `limit`.
    Claimed,
    Waiting,
}

/// The sort key. The field order is the sort order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Soonest {
    state: Listed,
    /// No finishing time: blocked behind a job the plan cannot time, or held by its row.
    unpriced: bool,
    /// `floor(eta / ETA_BUCKET_SECONDS)` of the absolute finishing time, so a bucket
    /// does not drift as the clock moves.
    bucket: i64,
    position: usize,
}

/// `timed`: compare finishing times (in buckets); otherwise plan position alone.
fn soonest(e: &Object, state: Listed, position: usize, timed: bool) -> Soonest {
    let eta = e
        .get("eta_at")
        .and_then(Value::as_str)
        .and_then(parse_iso_timestamp);
    Soonest {
        state,
        unpriced: eta.is_none(),
        bucket: match eta {
            Some(t) if timed => (t / ETA_BUCKET_SECONDS as f64).floor() as i64,
            _ => 0,
        },
        position,
    }
}

fn identity(e: &Object) -> Option<(&str, &str, &str)> {
    Some((
        e.get("series")?.as_str()?,
        e.get("volume")?.as_str()?,
        e.get("generation_id")?.as_str()?,
    ))
}

/// One listed job: its plan entry (or claim entry) and why it is listed.
#[derive(Debug)]
pub struct ListedJob<'a> {
    pub entry: &'a Object,
    pub state: Listed,
}

#[derive(Debug)]
pub struct ListedVolume<'a> {
    pub series: &'a str,
    pub volume: &'a str,
    /// Running first, then claimed, then soonest first.
    pub jobs: Vec<ListedJob<'a>>,
}

#[derive(Debug)]
pub struct QueueWindow<'a> {
    /// A volume's place is its soonest listed job; volumes with a running job first.
    pub volumes: Vec<ListedVolume<'a>>,
    /// Volumes with a job waiting (planned or claimed) and none running, listed or not.
    pub pending_volumes: usize,
}

/// The queue file's window over `plan` (`plan.running` priced, `plan.pending` in the
/// scheduler's order) and `claimed` (jobs claimed whose card has not begun: objects with
/// `series`, `volume` and `generation_id`), listing at most `limit` waiting jobs.
/// `timed`: order by finishing time; false while the queue is held (plan order alone).
pub fn queue_window<'a>(
    plan: &'a QueuePlan,
    claimed: &'a [Object],
    limit: usize,
    timed: bool,
) -> QueueWindow<'a> {
    let mut seen: HashSet<(&str, &str, &str)> = HashSet::new();
    let mut running_volumes: HashSet<(&str, &str)> = HashSet::new();
    let mut waiting_volumes: HashSet<(&str, &str)> = HashSet::new();
    let mut picked: Vec<(Soonest, (&str, &str), ListedJob<'a>)> = Vec::new();
    for (i, e) in plan.running.iter().enumerate() {
        let Some((s, v, g)) = identity(e) else {
            continue;
        };
        if !seen.insert((s, v, g)) {
            continue;
        }
        running_volumes.insert((s, v));
        let job = ListedJob {
            entry: e,
            state: Listed::Running,
        };
        picked.push((soonest(e, Listed::Running, i, timed), (s, v), job));
    }
    for (i, e) in claimed.iter().enumerate() {
        let Some((s, v, g)) = identity(e) else {
            continue;
        };
        if !seen.insert((s, v, g)) {
            continue;
        }
        waiting_volumes.insert((s, v));
        let job = ListedJob {
            entry: e,
            state: Listed::Claimed,
        };
        picked.push((soonest(e, Listed::Claimed, i, timed), (s, v), job));
    }
    let mut waiting: Vec<(Soonest, (&str, &str), &'a Object)> = Vec::new();
    for (i, e) in plan.pending.iter().enumerate() {
        let Some((s, v, g)) = identity(e) else {
            continue;
        };
        // The first entry in plan order wins.
        if !seen.insert((s, v, g)) {
            continue;
        }
        waiting_volumes.insert((s, v));
        waiting.push((soonest(e, Listed::Waiting, i, timed), (s, v), e));
    }
    let pending_volumes = waiting_volumes
        .iter()
        .filter(|k| !running_volumes.contains(*k))
        .count();
    waiting.sort_by_key(|w| w.0);
    let mut rest = Vec::new();
    for (n, (key, vk, e)) in waiting.into_iter().enumerate() {
        if n < limit {
            let job = ListedJob {
                entry: e,
                state: Listed::Waiting,
            };
            picked.push((key, vk, job));
        } else {
            rest.push((key, vk, e));
        }
    }
    // A row hold's jobs of a volume listed anyway: outside the limit, adding no volume.
    let listed: HashSet<(&str, &str)> = picked.iter().map(|p| p.1).collect();
    for (key, vk, e) in rest {
        if truthy(e.get("held")) && listed.contains(&vk) {
            let job = ListedJob {
                entry: e,
                state: Listed::Waiting,
            };
            picked.push((key, vk, job));
        }
    }
    picked.sort_by_key(|p| p.0);
    let mut at: HashMap<(&str, &str), usize> = HashMap::new();
    let mut volumes: Vec<ListedVolume<'a>> = Vec::new();
    for (_, (s, v), job) in picked {
        let i = *at.entry((s, v)).or_insert_with(|| {
            volumes.push(ListedVolume {
                series: s,
                volume: v,
                jobs: Vec::new(),
            });
            volumes.len() - 1
        });
        volumes[i].jobs.push(job);
    }
    QueueWindow {
        volumes,
        pending_volumes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::py::iso_utc;
    use serde_json::json;

    /// A fixed epoch on a bucket boundary, so offsets below 300 s share a bucket.
    const T: f64 = 1_800_000_000.0;

    fn e(s: &str, v: &str, g: &str, eta: Option<f64>) -> Object {
        let mut o = Object::new();
        o.insert("series".into(), json!(s));
        o.insert("volume".into(), json!(v));
        o.insert("generation_id".into(), json!(g));
        o.insert("eta_at".into(), json!(eta.map(|t| iso_utc(T + t))));
        o
    }

    fn held(mut o: Object) -> Object {
        o.insert("held".into(), json!(true));
        o
    }

    fn plan(running: Vec<Object>, pending: Vec<Object>) -> QueuePlan {
        QueuePlan {
            running,
            pending,
            ..Default::default()
        }
    }

    fn shape(w: &QueueWindow) -> Vec<(String, Vec<String>)> {
        w.volumes
            .iter()
            .map(|v| {
                (
                    v.volume.to_string(),
                    v.jobs
                        .iter()
                        .map(|j| j.entry["generation_id"].as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect()
    }

    fn order(w: &QueueWindow) -> Vec<String> {
        w.volumes.iter().map(|v| v.volume.to_string()).collect()
    }

    fn sv(items: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        items
            .iter()
            .map(|(v, gs)| (v.to_string(), gs.iter().map(|g| g.to_string()).collect()))
            .collect()
    }

    #[test]
    fn lists_the_next_jobs_not_every_job_of_the_next_volumes() {
        let far = 284.0 * 3600.0;
        let p = plan(
            vec![],
            vec![
                e("S", "A", "l-1", Some(120.0)),
                e("S", "B", "l-1", Some(240.0)),
                e("S", "C", "l-1", Some(360.0)),
                e("S", "A", "g-1", Some(far)),
                e("S", "B", "g-1", Some(far + 120.0)),
                e("S", "C", "g-1", Some(far + 240.0)),
            ],
        );
        let w = queue_window(&p, &[], 3, true);
        assert_eq!(
            shape(&w),
            sv(&[("A", &["l-1"]), ("B", &["l-1"]), ("C", &["l-1"])])
        );
        assert_eq!(w.pending_volumes, 3);
    }

    #[test]
    fn soonest_first_not_plan_order() {
        let p = plan(
            vec![],
            vec![
                e("S", "A", "g-1", Some(3600.0)),
                e("S", "B", "g-1", Some(600.0)),
                e("S", "C", "g-1", Some(2400.0)),
                e("S", "D", "g-1", Some(1200.0)),
            ],
        );
        let w = queue_window(&p, &[], 3, true);
        assert_eq!(order(&w), ["B", "D", "C"]);
        assert_eq!(w.pending_volumes, 4);
    }

    #[test]
    fn a_volume_lists_running_then_soonest() {
        let p = plan(
            vec![e("S", "A", "g-1", Some(600.0))],
            vec![
                e("S", "A", "l-1", Some(1500.0)),
                e("S", "A", "l-2", Some(900.0)),
            ],
        );
        let w = queue_window(&p, &[], 10, true);
        assert_eq!(shape(&w), sv(&[("A", &["g-1", "l-2", "l-1"])]));
        let states: Vec<Listed> = w.volumes[0].jobs.iter().map(|j| j.state).collect();
        assert_eq!(states, [Listed::Running, Listed::Waiting, Listed::Waiting]);
    }

    #[test]
    fn running_volumes_first_then_by_soonest_job() {
        let p = plan(
            vec![e("S", "R", "g-1", Some(3000.0))],
            vec![
                e("S", "X", "g-1", Some(1200.0)),
                e("S", "Y", "g-1", Some(600.0)),
                e("S", "X", "l-1", Some(0.0)),
            ],
        );
        let w = queue_window(&p, &[], 10, true);
        assert_eq!(
            shape(&w),
            sv(&[("R", &["g-1"]), ("X", &["l-1", "g-1"]), ("Y", &["g-1"])])
        );
        assert_eq!(w.pending_volumes, 2);
    }

    #[test]
    fn unpriced_follow_every_priced_in_plan_order() {
        let p = plan(
            vec![],
            vec![
                held(e("S", "U1", "g-1", None)),
                e("S", "P1", "g-1", Some(600.0)),
                e("S", "U2", "g-1", None),
                e("S", "P2", "g-1", Some(0.0)),
            ],
        );
        let w = queue_window(&p, &[], 3, true);
        assert_eq!(order(&w), ["P2", "P1", "U1"]);
        assert_eq!(w.pending_volumes, 4);
    }

    #[test]
    fn a_blocked_tail_keeps_plan_order() {
        // `plan_queue` leaves every item after one it cannot time unpriced too.
        let p = plan(
            vec![],
            vec![
                e("S", "A", "g-1", Some(0.0)),
                e("S", "B", "g-1", Some(600.0)),
                e("S", "Z", "g-1", None),
                e("S", "Y", "g-1", None),
                e("S", "X", "g-1", None),
            ],
        );
        let w = queue_window(&p, &[], 4, true);
        assert_eq!(order(&w), ["A", "B", "Z", "Y"]);
    }

    #[test]
    fn equal_etas_keep_plan_order() {
        let p = plan(
            vec![],
            vec![
                e("S", "B", "g-1", Some(60.0)),
                e("S", "A", "g-1", Some(60.0)),
            ],
        );
        assert_eq!(order(&queue_window(&p, &[], 2, true)), ["B", "A"]);
    }

    #[test]
    fn etas_within_one_bucket_keep_plan_order() {
        let p = plan(
            vec![],
            vec![
                e("S", "A", "g-1", Some(250.0)),
                e("S", "B", "g-1", Some(60.0)),
            ],
        );
        assert_eq!(order(&queue_window(&p, &[], 2, true)), ["A", "B"]);
    }

    #[test]
    fn a_small_eta_shift_keeps_the_window() {
        // Lanes re-priced: every finishing time moves by up to 30 s, none crosses a
        // bucket boundary. Same jobs, same order.
        let before = plan(
            vec![e("S", "R", "g-1", Some(100.0))],
            vec![
                e("S", "A", "g-1", Some(400.0)),
                e("S", "B", "g-1", Some(350.0)),
                e("S", "C", "g-1", Some(700.0)),
                e("S", "D", "g-1", Some(650.0)),
            ],
        );
        let after = plan(
            vec![e("S", "R", "g-1", Some(70.0))],
            vec![
                e("S", "A", "g-1", Some(380.0)),
                e("S", "B", "g-1", Some(370.0)),
                e("S", "C", "g-1", Some(670.0)),
                e("S", "D", "g-1", Some(680.0)),
            ],
        );
        let a = queue_window(&before, &[], 3, true);
        let b = queue_window(&after, &[], 3, true);
        assert_eq!(shape(&a), shape(&b));
        assert_eq!(order(&a), ["R", "A", "B", "C"]);
    }

    /// Finding: a held queue still prices its waiting jobs on the held machines' idle
    /// lanes, at "now + run time", so every finishing time moves with the clock. Two
    /// idle lanes, plan order A (200 s) on one, then B (100 s) and C (50 s) on the
    /// other: by finishing time the window cycles [A,B] → [B,C] → [B,A] → [A,B] every
    /// 300 s with nothing in the queue changing. Untimed (held) it stays in plan order.
    #[test]
    fn a_held_queue_keeps_plan_order_as_the_clock_moves() {
        let at = |now: f64| {
            plan(
                vec![],
                vec![
                    e("S", "A", "g-1", Some(now + 200.0)),
                    e("S", "B", "g-1", Some(now + 100.0)),
                    e("S", "C", "g-1", Some(now + 150.0)),
                ],
            )
        };
        let timed: Vec<Vec<String>> = [0.0, 100.0, 150.0, 200.0]
            .iter()
            .map(|now| order(&queue_window(&at(*now), &[], 2, true)))
            .collect();
        assert_eq!(
            timed,
            [["A", "B"], ["B", "C"], ["B", "A"], ["A", "B"]],
            "timed, the window follows the clock"
        );
        for now in [0.0, 100.0, 150.0, 200.0, 250.0, 299.0] {
            assert_eq!(order(&queue_window(&at(now), &[], 2, false)), ["A", "B"]);
        }
    }

    #[test]
    fn claimed_jobs_come_after_running_and_outside_the_limit() {
        let p = plan(
            vec![e("S", "R", "g-1", Some(0.0))],
            vec![
                e("S", "Y", "g-1", Some(10.0)),
                e("S", "Z", "g-1", Some(20.0)),
            ],
        );
        let claimed = [e("S", "X", "g-1", None)];
        let w = queue_window(&p, &claimed, 1, true);
        assert_eq!(order(&w), ["R", "X", "Y"]);
        let states: Vec<Listed> = w.volumes.iter().map(|v| v.jobs[0].state).collect();
        assert_eq!(states, [Listed::Running, Listed::Claimed, Listed::Waiting]);
        // X waits (claimed, not running), and so do Y and Z.
        assert_eq!(w.pending_volumes, 3);
    }

    #[test]
    fn row_held_jobs_of_listed_volumes_are_attached() {
        let p = plan(
            vec![],
            vec![
                e("S", "A", "g-1", Some(0.0)),
                e("S", "B", "g-1", Some(600.0)),
                held(e("S", "A", "l-1", None)),
                held(e("S", "H", "l-1", None)),
            ],
        );
        let w = queue_window(&p, &[], 2, true);
        assert_eq!(shape(&w), sv(&[("A", &["g-1", "l-1"]), ("B", &["g-1"])]));
        assert_eq!(w.pending_volumes, 3);
    }

    #[test]
    fn pending_volumes_counts_waiting_volumes_listed_or_not() {
        let p = plan(
            vec![e("S", "R", "g-1", Some(0.0))],
            vec![
                e("S", "R", "l-1", Some(300.0)),
                e("S", "A", "g-1", Some(600.0)),
                e("S", "B", "g-1", Some(900.0)),
            ],
        );
        let w = queue_window(&p, &[], 1, true);
        assert_eq!(shape(&w), sv(&[("R", &["g-1", "l-1"])]));
        assert_eq!(w.pending_volumes, 2);
    }

    #[test]
    fn a_job_planned_twice_is_listed_once() {
        let p = plan(
            vec![e("S", "A", "g-1", Some(0.0))],
            vec![e("S", "A", "g-1", Some(300.0))],
        );
        let claimed = [e("S", "A", "g-1", None)];
        let w = queue_window(&p, &claimed, 10, true);
        assert_eq!(shape(&w), sv(&[("A", &["g-1"])]));
        assert_eq!(w.volumes[0].jobs[0].state, Listed::Running);
        assert_eq!(w.pending_volumes, 0);
    }
}
