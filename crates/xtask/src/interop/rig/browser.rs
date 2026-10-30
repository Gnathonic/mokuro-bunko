//! The browser suites (`dav`, `api`, `bridge`, `direct`, `shared`): the parties a suite
//! needs, each its own process, then its specs (`interop/e2e/rig/<suite>/`) in Chromium
//! against the served reader (playwright.rs). One set of parties per suite and
//! provider; every spec gets a fresh browser context (fixtures.mjs).

use super::super::genlib;
use super::super::server::{Backend, CLOUD_FOLDER, SEED_PASSWORD, SeedUser, Server, ServerOptions};
use super::cloud::{self, ACCOUNT};
use super::cloudsim::{self, Cloudsim};
use super::control::{self, Control};
use super::fakeproc::{FakeOptions, FakeProcessor};
use super::playwright::{self, Env};
use super::rclone::{self, Rclone};
use super::results::{Outcome, Report, Scenario};
use super::{Paths, Provider, Suite};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The library every browser suite starts from: one series, a volume with its OCR
/// sidecar and one without (the fake processor's to OCR).
pub const LIBRARY_SERIES: &str = "Rig Series";
pub const LIBRARY_VOLUMES: [&str; 2] = ["Vol 1", "Vol 2"];

/// The Bunko accounts besides the admin (S2's three roles, and the OCR processor).
const USERS: [(&str, &str); 4] = [
    ("alice", "editor"),
    ("bob", "uploader"),
    ("carol", "registered"),
    ("ocr", "processor"),
];
const PROCESSOR: &str = "ocr";

/// Where the suite's Bunko server keeps its library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BunkoOn {
    /// Its own disk.
    Local,
    /// The suite's cloud (cloudsim or rclone).
    Cloud,
}

/// The parties one suite runs besides the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parties {
    pub cloud: bool,
    pub bunko: Option<BunkoOn>,
    /// processor-fake, held (RIG_FAKEPROC_HOLD), beside every Bunko server.
    pub fakeproc: bool,
}

pub fn parties(suite: Suite) -> Parties {
    match suite {
        Suite::Dav | Suite::Api => Parties {
            cloud: false,
            bunko: Some(BunkoOn::Local),
            fakeproc: true,
        },
        Suite::Bridge | Suite::Shared => Parties {
            cloud: true,
            bunko: Some(BunkoOn::Cloud),
            fakeproc: true,
        },
        // The reader alone against the cloud.
        Suite::Direct | Suite::Cloud => Parties {
            cloud: true,
            bunko: None,
            fakeproc: false,
        },
    }
}

/// `RIG_ACCOUNTS`: the admin, then every seeded user, with their passwords.
pub fn accounts(admin_password: Option<&str>) -> Value {
    let mut v = Vec::new();
    if let Some(pw) = admin_password {
        v.push(json!({"username": "admin", "password": pw, "role": "admin"}));
    }
    for (username, role) in USERS {
        v.push(json!({"username": username, "password": SEED_PASSWORD, "role": role}));
    }
    Value::Array(v)
}

/// The library's files, `(path below the library folder, bytes)`.
fn library_files() -> Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    for (i, v) in LIBRARY_VOLUMES.iter().enumerate() {
        files.push((
            format!("{LIBRARY_SERIES}/{v}.cbz"),
            genlib::cbz_bytes(&format!("rig browser {v}"))?,
        ));
        if i == 0 {
            files.push((
                format!("{LIBRARY_SERIES}/{v}.mokuro"),
                genlib::mokuro_doc(LIBRARY_SERIES, v, i),
            ));
        }
    }
    Ok(files)
}

/// The running parties; dropped (stopped) control port and processor first, cloud last.
struct Running {
    /// Holds the server too: dropped (its thread joined) before it.
    _control: Option<Control>,
    fake: Option<FakeProcessor>,
    /// Shared with the control port, which restarts it (control.rs).
    server: Option<Arc<Mutex<Server>>>,
    /// Held only to keep it serving until the suite ends.
    _rclone: Option<Rclone>,
    sim: Option<Cloudsim>,
    env: Env,
}

/// `reader`: the served reader's URL, which is also its origin.
fn setup(paths: &Paths, suite: Suite, p: Provider, work: &Path, reader: &str) -> Result<Running> {
    let want = parties(suite);
    let files = library_files()?;
    let in_cloud: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(rel, b)| (format!("{CLOUD_FOLDER}/{rel}"), b.clone()))
        .collect();
    let mut context = json!({
        "sim_account": ACCOUNT,
        "library": {"series": LIBRARY_SERIES, "volumes": LIBRARY_VOLUMES},
    });
    let (mut sim, mut rc) = (None, None);
    if want.cloud {
        if p == Provider::Rclone {
            let bin = rclone::binary(paths.rclone_bin.as_deref(), &paths.tools)?;
            // The reader's origin: its PROPFINDs carry Authorization, which a `*` does
            // not allow.
            let r = Rclone::start(
                &bin,
                &work.join("rclone-root"),
                &work.join("rclone"),
                Some(reader),
            )?;
            r.preflight(reader)?;
            r.seed(&in_cloud)?;
            context["webdav"] = json!({"url": r.url(), "username": r.user, "password": r.password});
            rc = Some(r);
        } else {
            let s = Cloudsim::start(&paths.cloudsim_bin, &work.join("cloudsim"))?;
            s.seed(p, ACCOUNT, &in_cloud)?;
            if p == Provider::Mega {
                // The account's MEGA sign-in (a server's login makes the same one).
                s.login(p, ACCOUNT)?;
                let (email, password) = cloudsim::mega_credentials(ACCOUNT);
                context["mega"] = json!({"email": email, "password": password});
            } else if suite == Suite::Bridge {
                // Only Bunko talks to this account: hold it to what Bunko's driver must
                // send (as the cloud suite does).
                s.knobs(p, ACCOUNT, json!({"strict_bunko": true}))?;
            }
            sim = Some(s);
        }
    }
    let mut server = None;
    if let Some(on) = want.bunko {
        let dir = work.join("bunko");
        let backend = match on {
            BunkoOn::Local => {
                for (rel, bytes) in &files {
                    let path = dir.join("library").join(rel);
                    std::fs::create_dir_all(path.parent().context("a library path")?)?;
                    std::fs::write(&path, bytes)?;
                }
                Backend::Local
            }
            BunkoOn::Cloud => Backend::Cloud(match (&sim, &rc) {
                (Some(s), _) => s.login(p, ACCOUNT)?,
                (_, Some(r)) => cloud::rclone_login(r),
                _ => unreachable!("a cloud backend runs with a cloud"),
            }),
        };
        let mut env = Vec::new();
        if on == BunkoOn::Cloud {
            let tune = cloud::tune(p);
            // A spec that holds a claim open (`restartBunko`) keeps its settle wait under
            // this: a claimant writes no heartbeat while it settles.
            context["lease"] = json!({"ttl_s": control::tune_value(&tune, "lease_ttl_s")});
            env.push((control::TUNE_VAR.to_string(), tune));
        }
        if let Some(s) = &sim {
            env.extend(s.server_env());
        }
        let mut s = Server::start_with(
            &paths.bin,
            &dir,
            ServerOptions {
                backend,
                readers: true,
                users: USERS
                    .iter()
                    .map(|(u, r)| SeedUser {
                        username: (*u).into(),
                        role: (*r).into(),
                    })
                    .collect(),
                ocr_remote: true,
                env,
            },
        )?;
        if on == BunkoOn::Cloud {
            s.wait_storage("held", Duration::from_secs(120))
                .context("the server never held the cloud library")?;
        }
        server = Some(s);
    }
    let fake = match (&server, want.fakeproc) {
        (Some(s), true) => Some(FakeProcessor::start(
            &paths.bin,
            &work.join("processor"),
            &s.base_url(),
            PROCESSOR,
            &FakeOptions {
                held: true,
                ..FakeOptions::default()
            },
        )?),
        _ => None,
    };
    let bunko_url = server.as_ref().map(Server::base_url);
    let server = server.map(|s| Arc::new(Mutex::new(s)));
    let ctl = match &server {
        Some(s) => Some(Control::start(control::bunko_handler(s.clone()))?),
        None => None,
    };
    let admin = std::fs::read_to_string(work.join("bunko/admin-password.txt")).ok();
    let accounts_file = work.join("accounts.json");
    std::fs::write(
        &accounts_file,
        serde_json::to_vec_pretty(&accounts(admin.as_deref()))?,
    )?;
    let preauth = match &sim {
        Some(s) => s.get("health")?["preauth"].as_str().map(str::to_string),
        None => None,
    };
    let env = Env {
        reader_url: reader.to_string(),
        bunko_url,
        cloudsim_url: sim.as_ref().map(|s| s.base.clone()),
        preauth_url: preauth,
        rclone_url: rc.as_ref().map(Rclone::url),
        provider: p.name().to_string(),
        accounts: accounts_file,
        workdir: work.to_path_buf(),
        fakeproc_hold: fake.as_ref().map(|f| f.hold_file().to_path_buf()),
        context,
        control_url: ctl.as_ref().map(Control::url),
    };
    Ok(Running {
        _control: ctl,
        fake,
        server,
        _rclone: rc,
        sim,
        env,
    })
}

fn failed(name: &str, ms: u64, detail: String) -> Scenario {
    Scenario {
        name: name.into(),
        outcome: Outcome::Failed,
        ms,
        detail: Some(detail),
        capability: None,
    }
}

/// Run one suite on one provider into `report`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    paths: &Paths,
    interop: &Path,
    suite: Suite,
    p: Provider,
    work: &Path,
    reader: &str,
    grep: Option<&str>,
    headed: bool,
    report: &mut Report,
) {
    let start = Instant::now();
    let rel = format!("e2e/rig/{}", suite.name());
    let junit = paths
        .out
        .join(format!("{}-{}.junit.xml", suite.name(), p.name()));
    // Never an earlier run's, beside this run's results.
    let _ = std::fs::remove_file(&junit);
    if playwright::spec_files(&interop.join(&rel)).is_empty() {
        report.add(Scenario {
            name: suite.name().into(),
            outcome: Outcome::Skipped,
            ms: 0,
            detail: Some(format!("no specs in interop/{rel}/")),
            capability: None,
        });
        return;
    }
    let mut running = match setup(paths, suite, p, work, reader) {
        Ok(r) => r,
        Err(e) => {
            report.add(failed(
                "setup",
                start.elapsed().as_millis() as u64,
                format!("{e:#}"),
            ));
            return;
        }
    };
    let parsed = playwright::run(&playwright::Run {
        interop,
        suite: suite.name(),
        env: &running.env,
        junit: &junit,
        grep,
        headed,
    });
    match parsed {
        Ok(parsed) => {
            for s in parsed.scenarios {
                report.add(s);
            }
            report.refused_hosts = parsed.refused_hosts;
            report.stubbed_urls = parsed.stubbed_urls;
        }
        Err(e) => report.add(failed("playwright", 0, format!("{e:#}"))),
    }
    // A party that died mid-suite fails it, whatever the specs saw.
    if let Some(s) = &running.server
        && let Err(e) = s
            .lock()
            .map_err(|_| anyhow::anyhow!("the server lock"))
            .and_then(|mut s| s.check_alive())
    {
        report.add(failed("bunko", 0, format!("{e:#}")));
    }
    if let Some(f) = &mut running.fake
        && let Err(e) = f.check_alive()
    {
        report.add(failed("processor-fake", 0, format!("{e:#}")));
    }
    // The fixtures fail a spec on violations it caused; anything left came after.
    if let Some(sim) = &running.sim {
        match sim.violations() {
            Ok(v) if v.is_empty() => {}
            Ok(v) => report.add(failed(
                "cloudsim violations",
                0,
                Value::Array(v).to_string(),
            )),
            Err(e) => report.add(failed("cloudsim violations", 0, format!("{e:#}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_suite_runs_the_parties_its_checks_need() {
        let local = Parties {
            cloud: false,
            bunko: Some(BunkoOn::Local),
            fakeproc: true,
        };
        assert_eq!(parties(Suite::Dav), local);
        assert_eq!(parties(Suite::Api), local);
        let through_bunko = Parties {
            cloud: true,
            bunko: Some(BunkoOn::Cloud),
            fakeproc: true,
        };
        assert_eq!(parties(Suite::Bridge), through_bunko);
        assert_eq!(parties(Suite::Shared), through_bunko);
        assert_eq!(
            parties(Suite::Direct),
            Parties {
                cloud: true,
                bunko: None,
                fakeproc: false
            }
        );
    }

    #[test]
    fn the_accounts_file_names_every_role_with_its_password() {
        let v = accounts(Some("admin-pw"));
        let rows = v.as_array().unwrap();
        assert_eq!(
            rows[0],
            json!({"username": "admin", "password": "admin-pw", "role": "admin"})
        );
        let roles: Vec<(&str, &str)> = rows[1..]
            .iter()
            .map(|r| (r["username"].as_str().unwrap(), r["role"].as_str().unwrap()))
            .collect();
        assert_eq!(roles, USERS);
        assert!(rows[1..].iter().all(|r| r["password"] == SEED_PASSWORD));
        // No server (direct): no admin.
        assert_eq!(accounts(None).as_array().unwrap().len(), USERS.len());
    }

    #[test]
    fn the_library_has_one_volume_with_ocr_and_one_without() {
        let files = library_files().unwrap();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "Rig Series/Vol 1.cbz",
                "Rig Series/Vol 1.mokuro",
                "Rig Series/Vol 2.cbz"
            ]
        );
        // Each archive has its own bytes.
        assert_ne!(files[0].1, files[2].1);
    }
}
