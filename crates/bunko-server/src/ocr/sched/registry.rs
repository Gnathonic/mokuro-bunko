//! Machines: the processor registry (0.5.2 `remote/registry.py`) and this server's own
//! in-process processor, kept by the scheduler so a registration, a drop and the claims
//! it returns are one step. Each connected machine contributes lanes.

use std::collections::{HashMap, VecDeque};

use bunko_proto::{
    Catalog, HostInfo, MAX_ENTRIES_PER_ACCOUNT, MAX_IDENTITY_BYTES, MAX_PROCESSOR_NAME,
    MAX_SESSIONS_PER_PROCESSOR, Op, PROCESSOR_ROOT, PROTOCOL_VERSION, RegisterReply,
};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::{Lane, Scheduler};
use crate::ocr::types::{Job, LOCAL, LOCAL_DISPLAY, token_hex};

/// A registration with no socket for this long is evicted by the next registration.
pub const STALE_REGISTRATION_SECONDS: f64 = 300.0;
pub const FAILED_LOGIN_MEMORY: usize = 20;
pub const TRANSFER_MEMORY: usize = 20;
pub const MAX_RETURN_CLASSES: usize = 16;

/// A refused login on a processor path, for the admin panel.
#[derive(Clone, Debug, PartialEq)]
pub struct FailedLogin {
    pub username: String,
    pub reason: String,
    pub at: f64,
}

/// `TransferStats`: the last deliveries and returns of one registration.
#[derive(Clone, Debug, Default)]
pub struct TransferStats {
    ready: VecDeque<[f64; 6]>,
    returned: indexmap::IndexMap<String, u64>,
    last_returned: Option<Value>,
    pub held_until: Option<f64>,
    pub held_error: String,
}

fn num(detail: &std::collections::BTreeMap<String, Value>, key: &str) -> f64 {
    detail
        .get(key)
        .and_then(|v| if v.is_boolean() { None } else { v.as_f64() })
        .unwrap_or(0.0)
}

impl TransferStats {
    pub fn note_ready(&mut self, detail: &std::collections::BTreeMap<String, Value>) {
        let damaged = detail
            .get("verdict")
            .is_some_and(|v| !v.is_null() && v != &json!("") && v != &json!(false));
        self.ready.push_back([
            num(detail, "bytes"),
            num(detail, "seconds"),
            num(detail, "requests"),
            num(detail, "restarts"),
            num(detail, "repairs"),
            if damaged { 1.0 } else { 0.0 },
        ]);
        while self.ready.len() > TRANSFER_MEMORY {
            self.ready.pop_front();
        }
    }

    pub fn note_returned(&mut self, klass: &str, error: &str, now: f64) {
        let mut klass: String = klass.chars().take(40).collect();
        if klass.is_empty() {
            klass = "local".into();
        }
        if !self.returned.contains_key(&klass) && self.returned.len() >= MAX_RETURN_CLASSES - 1 {
            klass = "other".into();
        }
        *self.returned.entry(klass.clone()).or_insert(0) += 1;
        self.last_returned = Some(
            json!({"class": klass, "error": error.chars().take(300).collect::<String>(), "at": now}),
        );
    }

    pub fn to_value(&self) -> Value {
        let bytes: f64 = self.ready.iter().map(|r| r[0]).sum();
        let seconds: f64 = self.ready.iter().map(|r| r[1]).sum();
        json!({
            "volumes": self.ready.len(),
            "mb_per_s": if seconds > 0.0 { Some(bunko_sched::py::round_to(bytes / 1e6 / seconds, 1)) } else { None },
            "resumed": self.ready.iter().filter(|r| r[2] > 1.0).count(),
            "restarted": self.ready.iter().filter(|r| r[3] > 0.0).count(),
            "repaired": self.ready.iter().filter(|r| r[4] > 0.0).count(),
            "damaged": self.ready.iter().filter(|r| r[5] > 0.0).count(),
            "returned": self.returned.values().sum::<u64>(),
            "returned_by_class": self.returned.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<Map<String, Value>>(),
            "last_returned": self.last_returned,
            "held_until": self.held_until,
            "held_error": if self.held_error.is_empty() { None } else { Some(&self.held_error) },
        })
    }
}

/// What a queue-page visitor calls each machine (`PublicNames`).
#[derive(Clone, Debug, Default)]
pub struct PublicNames {
    aliases: HashMap<String, String>,
    explicit: std::collections::HashSet<String>,
    count: u64,
}

impl PublicNames {
    pub fn assign(&mut self, name: &str, public_name: Option<&str>) -> String {
        if let Some(p) = public_name.filter(|p| !p.is_empty()) {
            self.aliases.insert(name.to_string(), p.to_string());
            self.explicit.insert(name.to_string());
        } else if !self.aliases.contains_key(name) || self.explicit.contains(name) {
            self.explicit.remove(name);
            self.count += 1;
            self.aliases
                .insert(name.to_string(), format!("machine {}", self.count));
        }
        self.aliases[name].clone()
    }

    pub fn name(&mut self, machine: &str) -> String {
        if machine.is_empty() || machine == LOCAL {
            return LOCAL_DISPLAY.to_string();
        }
        match self.aliases.get(machine) {
            Some(a) => a.clone(),
            None => self.assign(machine, None),
        }
    }
}

/// One machine: a processor registration (remote) or this server.
#[derive(Debug)]
pub struct Machine {
    /// The registration id (`"local"` for this server).
    pub pid: String,
    /// The hardware key everything is filed under: the processor's name, or `"local"`.
    pub name: String,
    pub username: Option<String>,
    pub local: bool,
    pub host: HostInfo,
    pub host_value: Value,
    pub catalog: Catalog,
    pub catalog_value: Value,
    pub max_sessions: u32,
    pub public_name: Option<String>,
    pub connected_since: f64,
    pub last_seen: f64,
    /// Monotonic time of the last frame (silence judgement).
    pub last_frame: f64,
    /// The link while it is up; dropping it closes the socket.
    pub ops: Option<mpsc::UnboundedSender<Op>>,
    pub account_stamp: Option<String>,
    pub transfer: TransferStats,
    /// The processor paused itself (`availability`, GUI.md §3): no lanes, no work.
    pub pause: Option<bunko_proto::Availability>,
}

impl Machine {
    /// `label()`: `"<name> (<gpu>)"` or the name; `"this server"` for local.
    pub fn label(&self) -> String {
        if self.local {
            return LOCAL_DISPLAY.to_string();
        }
        match self.host.gpu.as_deref().filter(|g| !g.is_empty()) {
            Some(gpu) => format!("{} ({gpu})", self.name),
            None => self.name.clone(),
        }
    }

    /// A processor still downloading its models matches no row.
    pub fn installing(&self) -> bool {
        !self.local && self.catalog.engines.is_empty()
    }

    pub fn connected(&self) -> bool {
        self.ops.is_some()
    }

    pub fn send(&self, op: Op) -> bool {
        self.ops.as_ref().is_some_and(|tx| tx.send(op).is_ok())
    }

    pub fn has_gpu(&self) -> bool {
        self.catalog
            .devices
            .iter()
            .any(|d| d.id.starts_with("gpu:"))
    }

    /// `ProcessorEntry.to_dict()` (admin panel).
    pub fn to_value(&self, sessions: usize) -> Value {
        json!({
            "processor_id": self.pid,
            "name": if self.local { LOCAL_DISPLAY } else { &self.name },
            "label": self.label(),
            "username": self.username,
            "host": self.host_value,
            "catalog": self.catalog_value,
            "max_sessions": self.max_sessions,
            "sessions": sessions,
            "connected_since": self.connected_since,
            "last_seen": self.last_seen,
            "installing": self.installing(),
            "local": self.local,
            "public_name": self.public_name,
            "transfer": self.transfer.to_value(),
            // 0.7: the processor's own pause, `{paused, until, reason}` or null.
            "pause": self.pause.as_ref().map(|a| json!({"paused": true, "until": a.until, "reason": a.reason})),
        })
    }
}

/// A registration request as the HTTP layer hands it over (already authenticated).
#[derive(Debug, Clone)]
pub struct RegisterInput {
    pub username: String,
    pub body: Value,
    pub account_stamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RegisterOutcome {
    Ok(RegisterReply),
    Refused { status: u16, body: Value },
}

/// Why a socket was refused: status + JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct SocketRefusal {
    pub status: u16,
    pub body: Value,
}

/// `clean_processor_name(raw, fallback)`.
pub fn clean_processor_name(raw: &str, fallback: &str) -> String {
    let cut = |s: &str| -> String {
        s.trim()
            .chars()
            .take(MAX_PROCESSOR_NAME)
            .collect::<String>()
            .trim()
            .to_string()
    };
    let name = cut(raw);
    if name.is_empty() { cut(fallback) } else { name }
}

/// The catalog a processor registered, read leniently: a malformed device row is
/// skipped rather than costing the whole catalog (which would read as "installing").
pub fn lenient_catalog(value: &Value) -> Catalog {
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let devices = value
        .get("devices")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let id = d.get("id").and_then(Value::as_str)?;
                    Some(bunko_proto::Device {
                        id: id.to_string(),
                        label: d
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_string(),
                        formats: d
                            .get("formats")
                            .and_then(Value::as_array)
                            .map(|f| {
                                f.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        provider: d
                            .get("provider")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        arch: d.get("arch").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Catalog {
        engines: strings("engines"),
        detectors: strings("detectors"),
        devices,
    }
}

/// Python `repr()` of a str for the error texts.
fn repr(s: &str) -> String {
    bunko_sched::py::py_repr(&Value::String(s.to_string()))
}

impl Scheduler {
    pub fn is_reserved_name(&self, name: &str) -> bool {
        name.to_lowercase() == LOCAL || (self.settings.local_processing && name == LOCAL_DISPLAY)
    }

    /// `POST /_processor/register` (spec remote-processors §5.2, protocol v3).
    pub fn register(&mut self, input: RegisterInput) -> RegisterOutcome {
        let refuse = |status: u16, error: String| RegisterOutcome::Refused {
            status,
            body: json!({"error": error}),
        };
        let Value::Object(body) = &input.body else {
            return refuse(400, "registration body is not an object".into());
        };
        let protocol = body.get("protocol").cloned().unwrap_or(Value::Null);
        if protocol.as_u64() != Some(u64::from(PROTOCOL_VERSION)) || protocol.is_f64() {
            return RegisterOutcome::Refused {
                status: 400,
                body: json!({
                    "error": format!("this server speaks protocol {PROTOCOL_VERSION}, not {}", bunko_sched::py::py_repr(&protocol)),
                    "protocols": [PROTOCOL_VERSION],
                    "version": self.deps.version,
                }),
            };
        }
        let name = match body.get("name") {
            None | Some(Value::Null) => clean_processor_name(&input.username, &input.username),
            Some(Value::String(s)) => clean_processor_name(s, &input.username),
            Some(_) => return refuse(400, "name must be text".into()),
        };
        if self.is_reserved_name(&name) {
            return refuse(400, format!("{} is a reserved name", repr(&name)));
        }
        let public_name = match body.get("public_name") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                let p = clean_processor_name(s, "");
                if p.is_empty() {
                    None
                } else if self.is_reserved_name(&p) {
                    return refuse(400, format!("{} is a reserved name", repr(&p)));
                } else {
                    Some(p)
                }
            }
            Some(_) => return refuse(400, "public_name must be text".into()),
        };
        let catalog_value = match body.get("catalog") {
            None | Some(Value::Null) => json!({}),
            Some(Value::Object(c)) => {
                for key in ["engines", "detectors", "devices"] {
                    if c.get(key).is_some_and(|v| !v.is_array()) {
                        return refuse(400, format!("catalog.{key} must be a list"));
                    }
                }
                Value::Object(c.clone())
            }
            Some(_) => return refuse(400, "catalog must be an object".into()),
        };
        let max_sessions = match body.get("max_sessions") {
            None | Some(Value::Null) => 1,
            Some(Value::Bool(b)) => u64::from(*b),
            Some(v) => match v.as_f64() {
                Some(f) if f.is_finite() => f.trunc().max(0.0) as u64,
                _ => match v.as_str().and_then(|s| s.trim().parse::<i64>().ok()) {
                    Some(n) => n.max(0) as u64,
                    None => return refuse(400, "max_sessions is not a whole number".into()),
                },
            },
        };
        let max_sessions = if max_sessions == 0 { 1 } else { max_sessions };
        let host_value = match body.get("host") {
            Some(Value::Object(h)) => Value::Object(h.clone()),
            _ => json!({}),
        };
        let identity = crate::ocr::pyjson::dumps(
            &json!({"host": host_value, "catalog": catalog_value}),
            crate::ocr::pyjson::DEFAULT,
        );
        if identity.len() > MAX_IDENTITY_BYTES {
            return refuse(
                413,
                format!("host and catalog take more than {MAX_IDENTITY_BYTES} bytes"),
            );
        }
        let held_by_another = self
            .machines
            .values()
            .any(|m| !m.local && m.name == name && m.username.as_deref() != Some(&input.username));
        if held_by_another || !self.profiles.claim(&name, &input.username) {
            return refuse(
                409,
                format!(
                    "the name {} belongs to another processor account; give this machine its own name",
                    repr(&name)
                ),
            );
        }
        let catalog = lenient_catalog(&catalog_value);
        let host: HostInfo = serde_json::from_value(host_value.clone()).unwrap_or_default();
        // Doomed: the same name (a reconnect), silent socketless registrations, then the
        // oldest of this account beyond the limit (socketless first).
        let now = self.now();
        let mut doomed: Vec<(String, &'static str)> = Vec::new();
        for m in self.machines.values() {
            if m.local || m.username.as_deref() != Some(&input.username) {
                continue;
            }
            if m.name == name {
                doomed.push((m.pid.clone(), "re-registered"));
            } else if !m.connected() && now - m.last_seen > STALE_REGISTRATION_SECONDS {
                doomed.push((m.pid.clone(), "registered but never opened a socket"));
            }
        }
        let mut keep: Vec<(&String, bool, f64)> = self
            .machines
            .values()
            .filter(|m| {
                !m.local
                    && m.username.as_deref() == Some(&input.username)
                    && !doomed.iter().any(|(p, _)| *p == m.pid)
            })
            .map(|m| (&m.pid, m.connected(), m.connected_since))
            .collect();
        keep.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)));
        let excess = (keep.len() + 1).saturating_sub(MAX_ENTRIES_PER_ACCOUNT);
        for (pid, _, _) in keep.into_iter().take(excess) {
            doomed.push((pid.clone(), "too many registrations from this account"));
        }
        for (pid, reason) in doomed {
            self.drop_processor(&pid, reason);
        }
        let pid = token_hex(8);
        let machine = Machine {
            pid: pid.clone(),
            name: name.clone(),
            username: Some(input.username.clone()),
            local: false,
            host,
            host_value: host_value.clone(),
            catalog,
            catalog_value: catalog_value.clone(),
            max_sessions: (max_sessions.min(u64::from(MAX_SESSIONS_PER_PROCESSOR))) as u32,
            public_name: public_name.clone(),
            connected_since: now,
            last_seen: now,
            last_frame: self.mono(),
            ops: None,
            account_stamp: input.account_stamp,
            transfer: TransferStats::default(),
            pause: super::availability::availability_of(body),
        };
        self.public_names.assign(&name, public_name.as_deref());
        self.profiles
            .set_identity(&name, &host_value, &catalog_value);
        self.machines.insert(pid.clone(), machine);
        let paused = self.machines.get(&pid).is_some_and(|m| m.pause.is_some());
        self.log(format!(
            "Processor {name} registered ({pid}){}",
            if paused {
                "; it is paused by its owner"
            } else {
                ""
            }
        ));
        RegisterOutcome::Ok(RegisterReply {
            protocol: PROTOCOL_VERSION,
            processor_id: pid.clone(),
            socket: format!("{PROCESSOR_ROOT}/{pid}/socket"),
            results: format!("{PROCESSOR_ROOT}/{pid}/results/{{sid}}/{{claim}}"),
            archives: bunko_proto::ARCHIVES_ROOT.to_string(),
            version: self.deps.version.clone(),
        })
    }

    /// `GET /_processor/{pid}/socket`: the link of a registration.
    pub fn socket_open(
        &mut self,
        pid: &str,
        username: &str,
        ops: mpsc::UnboundedSender<Op>,
    ) -> Result<(), super::SocketRefusal> {
        let Some(m) = self.machines.get(pid) else {
            return Err(SocketRefusal {
                status: 404,
                body: json!({"error": "No such processor"}),
            });
        };
        if m.local || m.username.as_deref() != Some(username) {
            return Err(SocketRefusal {
                status: 403,
                body: json!({"error": "Not your processor"}),
            });
        }
        if m.connected() {
            self.drop_processor(pid, "a second socket was opened");
            return Err(SocketRefusal {
                status: 409,
                body: json!({"error": "Socket already open; register again"}),
            });
        }
        let now = self.now();
        let mono = self.mono();
        let label;
        if let Some(m) = self.machines.get_mut(pid) {
            m.ops = Some(ops);
            m.last_seen = now;
            m.last_frame = mono;
            label = m.label();
        } else {
            return Err(SocketRefusal {
                status: 404,
                body: json!({"error": "No such processor"}),
            });
        }
        self.rebuild_lanes();
        self.bump();
        self.bump_page();
        let slots = self.lanes.iter().filter(|l| l.pid == pid).count();
        if self.scan_active {
            self.log(format!(
                "{label} joined the running scan with {slots} slot(s)"
            ));
        } else {
            self.log(format!("{label} connected with {slots} slot(s)"));
        }
        self.maybe_start_scan();
        Ok(())
    }

    /// The in-process processor is up: machine `local` with `ocr.concurrency` lanes.
    pub fn local_up(&mut self, ops: mpsc::UnboundedSender<Op>, catalog: Catalog, host: HostInfo) {
        let now = self.now();
        let catalog_value = serde_json::to_value(&catalog).unwrap_or(json!({}));
        let host_value = serde_json::to_value(&host).unwrap_or(json!({}));
        self.profiles.set_identity(
            crate::ocr::profiles::LOCAL_PROFILE,
            &host_value,
            &catalog_value,
        );
        let machine = Machine {
            pid: LOCAL.into(),
            name: LOCAL.into(),
            username: None,
            local: true,
            host,
            host_value,
            catalog,
            catalog_value,
            max_sessions: self.settings.concurrency.max(1),
            public_name: None,
            connected_since: now,
            last_seen: now,
            last_frame: self.mono(),
            ops: Some(ops),
            account_stamp: None,
            transfer: TransferStats::default(),
            pause: None,
        };
        self.machines.shift_insert(0, LOCAL.into(), machine);
        self.rebuild_lanes();
        self.bump();
        self.maybe_start_scan();
    }

    /// How many lanes each machine contributes now.
    fn wanted_lanes(&self) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        for m in self.machines.values() {
            if !m.connected() || m.installing() || m.pause.is_some() {
                continue;
            }
            if m.local {
                if self.settings.local_processing {
                    out.push((m.pid.clone(), self.settings.concurrency.max(1) as usize));
                }
            } else {
                out.push((m.pid.clone(), m.max_sessions.max(1) as usize));
            }
        }
        out
    }

    /// Make the lane list match the machines: keep existing lanes (and their ids), add
    /// lanes for new capacity, retire idle lanes that are no longer wanted.
    pub fn rebuild_lanes(&mut self) {
        let wanted = self.wanted_lanes();
        let mut next: Vec<Lane> = Vec::new();
        let old = std::mem::take(&mut self.lanes);
        for (pid, count) in &wanted {
            let mut mine: Vec<Lane> = old.iter().filter(|l| &l.pid == pid).cloned().collect();
            // Busy lanes stay until their session ends, even beyond the count.
            mine.sort_by_key(|l| (l.session.is_none(), l.id));
            let mut kept: Vec<Lane> = Vec::new();
            for l in mine {
                if kept.len() < *count || l.session.is_some() {
                    kept.push(l);
                }
            }
            while kept.len() < *count {
                kept.push(Lane {
                    id: self.next_lane_id,
                    pid: pid.clone(),
                    session: None,
                    waiting_for_faster: false,
                    idle_at: None,
                });
                self.next_lane_id += 1;
            }
            kept.sort_by_key(|l| l.id);
            next.extend(kept);
        }
        // Lanes of machines no longer wanted keep running their session out.
        for l in old {
            if l.session.is_some() && !next.iter().any(|n| n.id == l.id) {
                next.push(l);
            }
        }
        self.lanes = next;
    }

    pub fn seen(&mut self, pid: &str) {
        let now = self.now();
        let mono = self.mono();
        if let Some(m) = self.machines.get_mut(pid) {
            m.last_seen = now;
            m.last_frame = mono;
        }
    }

    pub fn record_failed_login(&mut self, username: &str, reason: &str) {
        let at = self.now();
        self.failed_logins.push_front(FailedLogin {
            username: username.chars().take(64).collect(),
            reason: reason.to_string(),
            at,
        });
        while self.failed_logins.len() > FAILED_LOGIN_MEMORY {
            self.failed_logins.pop_back();
        }
    }

    pub fn drop_account(&mut self, username: &str, reason: &str) {
        let pids: Vec<String> = self
            .machines
            .values()
            .filter(|m| m.username.as_deref() == Some(username))
            .map(|m| m.pid.clone())
            .collect();
        for pid in pids {
            self.drop_processor(&pid, reason);
        }
    }

    /// `registry.drop` + `processor_disconnected`: the single disconnect path. Every
    /// claim of that machine not being installed goes back unrecorded and is offered
    /// again this scan; its sessions end blaming nobody; its breaker goes.
    pub fn drop_processor(&mut self, pid: &str, reason: &str) {
        let Some(machine) = self.machines.shift_remove(pid) else {
            return;
        };
        let label = machine.label();
        let now = self.now();
        if !machine.local {
            self.last_disconnect = Some((machine.name.clone(), now));
        }
        let mut returned = 0;
        let jobs: Vec<Job> = self
            .claims
            .iter()
            .filter(|(_, c)| c.pid == pid && !c.settling)
            .map(|(j, _)| j.clone())
            .collect();
        for job in jobs {
            self.claims.remove(&job);
            self.attempted.remove(&job);
            self.cancelled.remove(&job);
            self.cards.shift_remove(&job);
            returned += 1;
        }
        let sids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.pid == pid)
            .map(|(k, _)| k.clone())
            .collect();
        for sid in sids {
            self.sessions.remove(&sid);
            self.ended_sessions.insert(sid, now);
        }
        self.results
            .retain(|(sid, _), _| self.sessions.contains_key(sid));
        self.breakers.remove(pid);
        self.lanes.retain(|l| l.pid != pid);
        self.bench_machine_left(&machine.name);
        self.rebuild_lanes();
        self.bump();
        self.bump_page();
        if machine.local {
            self.log(format!(
                "{label} stopped ({reason}); {returned} volume(s) back in the queue"
            ));
        } else {
            self.log(format!(
                "{label} disconnected ({reason}); {returned} volume(s) back in the queue"
            ));
        }
        // Dropping `machine` drops its link: the socket task sees the end and closes.
        drop(machine);
    }

    /// Every registration (local first, then by lowercase name), as the admin reads them.
    pub fn processors(&self) -> Vec<Value> {
        let mut entries: Vec<&Machine> = self.machines.values().collect();
        entries.sort_by(|a, b| {
            b.local
                .cmp(&a.local)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        entries
            .into_iter()
            .map(|m| {
                let sessions = self.sessions.values().filter(|s| s.pid == m.pid).count();
                let mut v = m.to_value(sessions);
                if let (Value::Object(map), Some(b)) = (&mut v, self.breakers.get(&m.pid))
                    && b.open_until > self.now()
                    && let Some(Value::Object(t)) = map.get_mut("transfer")
                {
                    t.insert("held_until".into(), json!(b.open_until));
                    t.insert("held_error".into(), json!(b.last_error));
                }
                v
            })
            .collect()
    }

    pub fn failed_logins(&self) -> Vec<Value> {
        self.failed_logins
            .iter()
            .map(|f| json!({"username": f.username, "reason": f.reason, "at": f.at}))
            .collect()
    }

    pub fn last_disconnect(&self) -> Option<Value> {
        self.last_disconnect
            .as_ref()
            .map(|(n, at)| json!({"name": n, "at": at}))
    }

    /// The machine a hardware name belongs to.
    pub fn machine_by_name(&self, name: &str) -> Option<&Machine> {
        self.machines.values().find(|m| m.name == name)
    }

    pub fn remote_connected(&self) -> Vec<&Machine> {
        self.machines
            .values()
            .filter(|m| !m.local && m.connected())
            .collect()
    }

    /// A `catalog` event: what the processor can run changed (models downloaded).
    pub fn catalog_changed(&mut self, pid: &str, catalog: Catalog) {
        let name;
        let value = serde_json::to_value(&catalog).unwrap_or(json!({}));
        if let Some(m) = self.machines.get_mut(pid) {
            m.catalog = catalog;
            m.catalog_value = value.clone();
            name = m.name.clone();
        } else {
            return;
        }
        let host = self
            .machines
            .get(pid)
            .map(|m| m.host_value.clone())
            .unwrap_or(json!({}));
        self.profiles
            .set_identity(crate::ocr::profiles::profile_key(&name), &host, &value);
        self.rebuild_lanes();
        self.bump();
        self.maybe_start_scan();
    }

    /// The admin's map `processor name → its stored catalog` (machines not connected).
    pub fn remembered_catalogs(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for name in self.profiles.names() {
            if let Some(c) = self.profiles.load(&name).get("catalog") {
                out.insert(name, c.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_processor_name("  tower  ", "u"), "tower");
        assert_eq!(clean_processor_name("   ", " user "), "user");
        assert_eq!(clean_processor_name(&"x".repeat(80), "u").len(), 64);
    }

    #[test]
    fn public_names_number_in_order() {
        let mut p = PublicNames::default();
        assert_eq!(p.assign("a", None), "machine 1");
        assert_eq!(p.assign("b", Some("big box")), "big box");
        assert_eq!(p.name("a"), "machine 1");
        assert_eq!(p.name("c"), "machine 2");
        assert_eq!(p.name("local"), "this server");
    }
}
