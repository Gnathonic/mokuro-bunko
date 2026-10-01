//! `admin *`: user and invite management straight on `mokuro.db` (0.5.2 `admin/cli.py`).
//!
//! Deviation: the database is opened with the config's `database.*` knobs (0.5.2's CLI
//! ignored them, spec Q15) and without a read pool, which a one-shot command never needs.

use super::Ctx;
use crate::cfgfile;
use crate::cli::AdminCmd;
use crate::out::{CmdResult, Fail, exit_with};
use crate::prompt;
use bunko_db::{Database, DbOptions, UserStatus, normalize_role};
use std::str::FromStr;

fn open_db(ctx: &Ctx) -> Result<Database, Fail> {
    let config = cfgfile::load_effective(&ctx.config_path)?;
    let options = DbOptions {
        read_connections: 0,
        ..DbOptions::from(&config.database)
    };
    Ok(Database::open_with(
        config.storage.layout().database(),
        &options,
    )?)
}

/// First `n` characters (Python `s[:n]`).
fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn run(ctx: &Ctx, cmd: AdminCmd) -> CmdResult {
    match cmd {
        AdminCmd::AddUser {
            username,
            role,
            password,
        } => {
            let password = prompt::password_option(password, "Password")?;
            let db = open_db(ctx)?;
            let normalized = normalize_role(&role)?;
            db.create_user(&username, &password, normalized, UserStatus::Active, "")?;
            println!("User '{username}' created with role '{role}'");
        }
        AdminCmd::DeleteUser { username, yes } => {
            if !yes {
                prompt::confirm_or_abort(&format!("Delete user '{username}'?"))?;
            }
            if open_db(ctx)?.delete_user(&username)? {
                println!("User '{username}' deleted");
            } else {
                return Err(exit_with(format!("User '{username}' not found")));
            }
        }
        AdminCmd::ListUsers { status } => {
            let filter = status
                .as_deref()
                .map(UserStatus::from_str)
                .transpose()
                .map_err(Fail::msg)?;
            let users = open_db(ctx)?.list_users(filter)?;
            if users.is_empty() {
                match status {
                    Some(s) => println!("No {s} users found"),
                    None => println!("No users found"),
                }
                return Ok(());
            }
            println!(
                "{:<20} {:<12} {:<10} {:<20}",
                "Username", "Role", "Status", "Created"
            );
            println!("{}", "-".repeat(64));
            for u in users {
                println!(
                    "{:<20} {:<12} {:<10} {:<20}",
                    u.username,
                    u.role.as_str(),
                    u.status.as_str(),
                    head(&u.created_at, 19)
                );
            }
        }
        AdminCmd::ChangeRole { username, role } => {
            let db = open_db(ctx)?;
            if db.update_user_role(&username, normalize_role(&role)?)? {
                println!("User '{username}' role changed to '{role}'");
            } else {
                return Err(exit_with(format!("User '{username}' not found")));
            }
        }
        AdminCmd::GenerateInvite { role, expires } => {
            let db = open_db(ctx)?;
            let code = db.create_invite(normalize_role(&role)?, &expires, None)?;
            println!("Invite code: {code}");
            println!("Role: {role}");
            println!("Expires in: {expires}");
        }
        AdminCmd::ListInvites { include_all } => {
            let invites = open_db(ctx)?.list_invites(include_all)?;
            if invites.is_empty() {
                println!("No invites found");
                return Ok(());
            }
            if include_all {
                println!(
                    "{:<24} {:<12} {:<20} {:<15}",
                    "Code", "Role", "Expires", "Used By"
                );
                println!("{}", "-".repeat(73));
                for i in invites {
                    let used_by = i
                        .used_by
                        .as_deref()
                        .filter(|u| !u.is_empty())
                        .unwrap_or("-");
                    println!(
                        "{:<24} {:<12} {:<20} {:<15}",
                        i.code,
                        i.role.as_str(),
                        head(&i.expires_at, 19),
                        used_by
                    );
                }
            } else {
                println!("{:<24} {:<12} {:<20}", "Code", "Role", "Expires");
                println!("{}", "-".repeat(58));
                for i in invites {
                    println!(
                        "{:<24} {:<12} {:<20}",
                        i.code,
                        i.role.as_str(),
                        head(&i.expires_at, 19)
                    );
                }
            }
        }
        AdminCmd::DeleteInvite { code } => {
            if open_db(ctx)?.delete_invite(&code)? {
                println!("Invite '{code}' deleted");
            } else {
                return Err(exit_with(format!("Invite '{code}' not found")));
            }
        }
        AdminCmd::RestoreUser {
            username,
            role,
            password,
        } => {
            let password = prompt::password_option(password, "Password")?;
            let db = open_db(ctx)?;
            let role = role.as_deref().map(normalize_role).transpose()?;
            if db.restore_user(&username, &password, role)? {
                println!("User '{username}' restored");
            } else {
                return Err(exit_with(format!(
                    "Error: '{username}' is not a deleted account"
                )));
            }
        }
        AdminCmd::ApproveUser { username } => {
            if open_db(ctx)?.approve_user(&username)? {
                println!("User '{username}' approved");
            } else {
                return Err(exit_with(format!(
                    "User '{username}' not found or not pending"
                )));
            }
        }
        AdminCmd::DisableUser { username } => {
            if open_db(ctx)?.disable_user(&username)? {
                println!("User '{username}' disabled");
            } else {
                return Err(exit_with(format!("User '{username}' not found")));
            }
        }
        AdminCmd::SetPassword { username, password } => {
            let password = prompt::password_option(password, "Password")?;
            if open_db(ctx)?.update_user_password(&username, &password)? {
                println!("Password updated for '{username}'");
            } else {
                return Err(exit_with(format!("User '{username}' not found")));
            }
        }
    }
    Ok(())
}
