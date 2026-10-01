//! `admin *` — exact strings, tables and exit codes (spec db-auth-admin §19).

mod common;
use common::{Env, stderr, stdout};
use predicates::prelude::*;

fn env() -> Env {
    let env = Env::new();
    env.write_config("");
    env
}

fn add(env: &Env, user: &str, role: &str) {
    env.cmd()
        .args(["admin", "add-user", user, "--role", role, "--password", "password123"])
        .assert()
        .success()
        .stdout(format!("User '{user}' created with role '{role}'\n"));
}

#[test]
fn add_and_list_users() {
    let env = env();
    env.cmd().args(["admin", "list-users"]).assert().success().stdout("No users found\n");
    env.cmd().args(["admin", "list-users", "--status", "pending"]).assert().success().stdout("No pending users found\n");
    add(&env, "alice", "admin");
    add(&env, "bob", "registered");
    assert!(env.storage().join("mokuro.db").exists());

    let out = env.cmd().args(["admin", "list-users"]).output().unwrap();
    assert!(out.status.success());
    let s = stdout(&out);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], format!("{:<20} {:<12} {:<10} {:<20}", "Username", "Role", "Status", "Created"));
    assert_eq!(lines[1], "-".repeat(64));
    assert_eq!(lines.len(), 4);
    let alice = lines.iter().find(|l| l.starts_with("alice")).unwrap();
    assert!(alice.starts_with(&format!("{:<20} {:<12} {:<10} ", "alice", "admin", "active")), "{alice:?}");
    // created_at[:19] = 'YYYY-MM-DD HH:MM:SS', padded to 20.
    assert_eq!(alice.len(), 20 + 1 + 12 + 1 + 10 + 1 + 20);
}

#[test]
fn add_user_errors() {
    let env = env();
    add(&env, "alice", "registered");
    env.cmd()
        .args(["admin", "add-user", "alice", "--password", "password123"])
        .assert()
        .code(1)
        .stderr("Error: Username 'alice' already exists\n");
    env.cmd()
        .args(["admin", "add-user", "carol", "--password", "short"])
        .assert()
        .code(1)
        .stderr("Error: Password must be at least 8 characters\n");
    env.cmd()
        .args(["admin", "add-user", "x", "--password", "password123"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("Error: Username must be 3-32 characters"));
    // Not a role choice: usage error, exit 2.
    env.cmd().args(["admin", "add-user", "dave", "--role", "writer", "--password", "password123"]).assert().code(2);
}

#[test]
fn add_user_prompts_for_password() {
    let env = env();
    env.cmd()
        .args(["admin", "add-user", "erin"])
        .write_stdin("password123\npassword123\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Password: ").and(predicate::str::contains("Repeat for confirmation: ")))
        .stdout(predicate::str::contains("User 'erin' created with role 'registered'"));
    // Mismatch, then EOF: aborted.
    env.cmd()
        .args(["admin", "add-user", "frank"])
        .write_stdin("password123\nother12345\n")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Error: The two entered values do not match."))
        .stderr("Aborted!\n");
}

#[test]
fn delete_restore_cycle() {
    let env = env();
    add(&env, "alice", "editor");
    // Confirmation declined -> Aborted!, exit 1, user untouched.
    env.cmd().args(["admin", "delete-user", "alice"]).write_stdin("n\n").assert().code(1).stderr("Aborted!\n");
    env.cmd()
        .args(["admin", "delete-user", "alice"])
        .write_stdin("y\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Delete user 'alice'? [y/N]: ").and(predicate::str::ends_with("User 'alice' deleted\n")));
    env.cmd().args(["admin", "delete-user", "alice", "-y"]).assert().code(1).stderr("User 'alice' not found\n");
    env.cmd()
        .args(["admin", "add-user", "alice", "--password", "password123"])
        .assert()
        .code(1)
        .stderr("Error: Username 'alice' belongs to a deleted account; bring it back with: mokuro-bunko admin restore-user alice\n");
    env.cmd().args(["admin", "list-users", "--status", "deleted"]).assert().success().stdout(predicate::str::contains("alice"));
    env.cmd()
        .args(["admin", "restore-user", "alice", "--password", "newpassword1"])
        .assert()
        .success()
        .stdout("User 'alice' restored\n");
    env.cmd()
        .args(["admin", "restore-user", "alice", "--password", "newpassword1"])
        .assert()
        .code(1)
        .stderr("Error: 'alice' is not a deleted account\n");
    let out = env.cmd().args(["admin", "list-users"]).output().unwrap();
    assert!(stdout(&out).lines().any(|l| l.starts_with(&format!("{:<20} {:<12} {:<10}", "alice", "editor", "active"))));
}

#[test]
fn role_status_password() {
    let env = env();
    add(&env, "bob", "registered");
    env.cmd().args(["admin", "change-role", "bob", "uploader"]).assert().success().stdout("User 'bob' role changed to 'uploader'\n");
    env.cmd().args(["admin", "change-role", "nobody", "admin"]).assert().code(1).stderr("User 'nobody' not found\n");
    env.cmd().args(["admin", "change-role", "bob", "anonymous"]).assert().code(2);
    env.cmd().args(["admin", "approve-user", "bob"]).assert().code(1).stderr("User 'bob' not found or not pending\n");
    env.cmd().args(["admin", "disable-user", "bob"]).assert().success().stdout("User 'bob' disabled\n");
    env.cmd().args(["admin", "disable-user", "nobody"]).assert().code(1).stderr("User 'nobody' not found\n");
    env.cmd()
        .args(["admin", "set-password", "bob", "--password", "anotherpass"])
        .assert()
        .success()
        .stdout("Password updated for 'bob'\n");
    env.cmd()
        .args(["admin", "set-password", "nobody", "--password", "anotherpass"])
        .assert()
        .code(1)
        .stderr("User 'nobody' not found\n");
    env.cmd()
        .args(["admin", "set-password", "bob", "--password", "short"])
        .assert()
        .code(1)
        .stderr("Error: Password must be at least 8 characters\n");
}

#[test]
fn invites() {
    let env = env();
    env.cmd().args(["admin", "list-invites"]).assert().success().stdout("No invites found\n");
    let out = env.cmd().args(["admin", "generate-invite", "--role", "uploader", "--expires", "2d"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let s = stdout(&out);
    let lines: Vec<&str> = s.lines().collect();
    let code = lines[0].strip_prefix("Invite code: ").unwrap().to_string();
    assert_eq!(code.len(), 22);
    assert_eq!(lines[1], "Role: uploader");
    assert_eq!(lines[2], "Expires in: 2d");

    let s = stdout(&env.cmd().args(["admin", "list-invites"]).output().unwrap());
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], format!("{:<24} {:<12} {:<20}", "Code", "Role", "Expires"));
    assert_eq!(lines[1], "-".repeat(58));
    assert!(lines[2].starts_with(&format!("{code:<24} {:<12} ", "uploader")));

    let s = stdout(&env.cmd().args(["admin", "list-invites", "--all"]).output().unwrap());
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], format!("{:<24} {:<12} {:<20} {:<15}", "Code", "Role", "Expires", "Used By"));
    assert_eq!(lines[1], "-".repeat(73));
    assert!(lines[2].ends_with(&format!(" {:<15}", "-")), "{:?}", lines[2]);

    env.cmd()
        .args(["admin", "generate-invite", "--expires", "5x"])
        .assert()
        .code(1)
        .stderr("Error: Unknown duration unit: x\n");
    env.cmd().args(["admin", "generate-invite", "--role", "admin"]).assert().code(2);

    env.cmd().args(["admin", "delete-invite", &code]).assert().success().stdout(format!("Invite '{code}' deleted\n"));
    env.cmd().args(["admin", "delete-invite", &code]).assert().code(1).stderr(format!("Invite '{code}' not found\n"));
    // Codes may start with '-'.
    env.cmd().args(["admin", "delete-invite", "-abc"]).assert().code(1).stderr("Invite '-abc' not found\n");
}
