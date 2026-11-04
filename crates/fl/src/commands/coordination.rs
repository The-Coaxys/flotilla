//! `fl coordination-state …` — durable versioned state and advisory locks.
use std::time::Duration;

use fl_model::coordination::{
    CoordinationScope, CoordinationSnapshot, CoordinationValue, MAX_COORDINATION_KEY_BYTES,
    MAX_COORDINATION_VALUE_BYTES, MAX_LOCK_TTL_MS,
};
use fl_model::workspace::DaemonEvent;
use fl_proto::{Request, Response};

use super::{caller, done, read_message, resolve_worktree, unexpected};
use crate::cli::{Cli, CoordinationCommand, CoordinationScopeArg};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

/// `--timeout 0` waits without a deadline; the CLI still yields on Ctrl-C.
const FOREVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &CoordinationCommand,
) -> Result<(), CliError> {
    match command {
        CoordinationCommand::Get { key, scope } => get(client, reporter, cli, key, *scope).await,
        CoordinationCommand::Set {
            key,
            value,
            value_file,
            if_version,
            scope,
        } => {
            set(
                client,
                reporter,
                cli,
                key,
                value.as_deref(),
                value_file.as_deref(),
                *if_version,
                *scope,
            )
            .await
        }
        CoordinationCommand::Delete {
            key,
            if_version,
            scope,
        } => delete(client, reporter, cli, key, *if_version, *scope).await,
        CoordinationCommand::Watch {
            key,
            scope,
            timeout,
        } => watch(client, reporter, cli, key, *scope, *timeout).await,
        CoordinationCommand::Lock {
            key,
            owner,
            ttl_seconds,
            scope,
        } => lock(client, reporter, cli, key, owner, *ttl_seconds, *scope).await,
        CoordinationCommand::Unlock { key, owner, scope } => {
            unlock(client, reporter, cli, key, owner, *scope).await
        }
    }
}

async fn scope(
    client: &mut DaemonClient,
    cli: &Cli,
    arg: CoordinationScopeArg,
) -> Result<CoordinationScope, CliError> {
    match arg {
        CoordinationScopeArg::Global => Ok(CoordinationScope::Global),
        CoordinationScopeArg::Worktree => Ok(CoordinationScope::Worktree(
            resolve_worktree(client, cli).await?.0.id,
        )),
        CoordinationScopeArg::Workspace => {
            let caller = caller(client, cli).await?;
            if let Some(project) = caller.project {
                return Ok(CoordinationScope::Workspace(project.workspace));
            }
            match client.rpc(Request::GetSelectedWorkspace).await? {
                Response::SelectedWorkspace(Some(workspace)) => {
                    Ok(CoordinationScope::Workspace(workspace.id))
                }
                Response::SelectedWorkspace(None) => Err(CliError::invalid(
                    "no workspace is selected; run `fl workspace select <id>` or use --scope worktree",
                )),
                other => Err(unexpected("selected workspace", &other)),
            }
        }
    }
}

fn check_key(key: &str) -> Result<(), CliError> {
    if key.trim().is_empty() || key.len() > MAX_COORDINATION_KEY_BYTES || key.contains('\0') {
        return Err(CliError::invalid(format!(
            "a coordination key is 1-{MAX_COORDINATION_KEY_BYTES} bytes without NUL"
        )));
    }
    Ok(())
}

async fn get(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    key: &str,
    scope_arg: CoordinationScopeArg,
) -> Result<(), CliError> {
    check_key(key)?;
    let scope = scope(client, cli, scope_arg).await?;
    let snapshot = match client
        .rpc(Request::CoordinationGet {
            scope,
            key: key.to_owned(),
        })
        .await?
    {
        Response::CoordinationSnapshot(snapshot) => snapshot,
        other => return Err(unexpected("coordination entry", &other)),
    };
    if reporter.json {
        return reporter.value(&snapshot);
    }
    print_snapshot(reporter, &snapshot);
    Ok(())
}

fn print_snapshot(reporter: &Reporter, snapshot: &CoordinationSnapshot) {
    match &snapshot.value {
        Some(entry) => reporter.text(format!(
            "v{} {} = {}",
            entry.version,
            entry.key,
            entry.value.replace('\n', "\\n")
        )),
        None => reporter.text("(missing)"),
    }
    if let Some(lock) = &snapshot.lock {
        reporter.text(format!(
            "locked by {} until {} ({}s remaining)",
            lock.owner,
            lock.expires_ms,
            lock.expires_ms.saturating_sub(now_ms()) / 1000
        ));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn set(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    key: &str,
    value: Option<&str>,
    value_file: Option<&str>,
    if_version: Option<u64>,
    scope_arg: CoordinationScopeArg,
) -> Result<(), CliError> {
    check_key(key)?;
    let value = match (value, value_file) {
        (Some(value), _) => read_message(value)?,
        (None, Some("-")) => read_message("-")?,
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|error| CliError::invalid(format!("reading `{path}`: {error}")))?,
        (None, None) => read_message("-")?,
    };
    if value.len() > MAX_COORDINATION_VALUE_BYTES {
        return Err(CliError::invalid(format!(
            "a coordination value is at most {MAX_COORDINATION_VALUE_BYTES} bytes"
        )));
    }
    let scope = scope(client, cli, scope_arg).await?;
    match client
        .rpc(Request::CoordinationSet {
            scope,
            key: key.to_owned(),
            value,
            if_version,
        })
        .await?
    {
        Response::CoordinationValue(entry) => {
            if reporter.json {
                return reporter.value(&serde_json::json!({ "entry": entry }));
            }
            reporter.text(format!("v{} {} written", entry.version, entry.key));
            Ok(())
        }
        other => Err(unexpected("coordination value", &other)),
    }
}

async fn delete(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    key: &str,
    if_version: Option<u64>,
    scope_arg: CoordinationScopeArg,
) -> Result<(), CliError> {
    check_key(key)?;
    let scope = scope(client, cli, scope_arg).await?;
    done(
        client
            .rpc(Request::CoordinationDelete {
                scope,
                key: key.to_owned(),
                if_version,
            })
            .await?,
        "coordination delete",
    )?;
    if reporter.json {
        return reporter.value(&serde_json::json!({ "key": key, "deleted": true }));
    }
    reporter.text(format!("deleted {key}"));
    Ok(())
}

async fn lock(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    key: &str,
    owner: &str,
    ttl_seconds: u64,
    scope_arg: CoordinationScopeArg,
) -> Result<(), CliError> {
    check_key(key)?;
    if owner.trim().is_empty() {
        return Err(CliError::invalid("--owner must not be empty"));
    }
    let ttl_ms = ttl_seconds.saturating_mul(1_000);
    if !(1_000..=MAX_LOCK_TTL_MS).contains(&ttl_ms) {
        return Err(CliError::invalid(format!(
            "--ttl-seconds must be 1-{} seconds",
            MAX_LOCK_TTL_MS / 1_000
        )));
    }
    let scope = scope(client, cli, scope_arg).await?;
    match client
        .rpc(Request::LockCoordination {
            scope,
            key: key.to_owned(),
            owner: owner.to_owned(),
            ttl_ms,
        })
        .await?
    {
        Response::CoordinationLock(lock) => {
            if reporter.json {
                return reporter.value(&serde_json::json!({ "lock": lock }));
            }
            reporter.text(format!(
                "locked {key} for {owner} until {}",
                lock.expires_ms
            ));
            Ok(())
        }
        other => Err(unexpected("coordination lock", &other)),
    }
}

async fn unlock(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    key: &str,
    owner: &str,
    scope_arg: CoordinationScopeArg,
) -> Result<(), CliError> {
    check_key(key)?;
    let scope = scope(client, cli, scope_arg).await?;
    done(
        client
            .rpc(Request::UnlockCoordination {
                scope,
                key: key.to_owned(),
                owner: owner.to_owned(),
            })
            .await?,
        "coordination unlock",
    )?;
    if reporter.json {
        return reporter.value(&serde_json::json!({ "key": key, "unlocked": true }));
    }
    reporter.text(format!("unlocked {key}"));
    Ok(())
}

async fn watch(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    key: &str,
    scope_arg: CoordinationScopeArg,
    timeout_secs: u64,
) -> Result<(), CliError> {
    check_key(key)?;
    let scope = scope(client, cli, scope_arg).await?;
    let snapshot = match client
        .rpc(Request::CoordinationGet {
            scope,
            key: key.to_owned(),
        })
        .await?
    {
        Response::CoordinationSnapshot(snapshot) => snapshot,
        other => return Err(unexpected("coordination entry", &other)),
    };
    emit(reporter, &snapshot);
    let mut current = snapshot.value.as_ref().map(|entry| entry.version);
    let wait = if timeout_secs == 0 {
        FOREVER
    } else {
        Duration::from_secs(timeout_secs)
    };
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let Some(event) = client.event(remaining).await? else {
            return Ok(());
        };
        let DaemonEvent::CoordinationChanged {
            scope: changed_scope,
            key: changed_key,
            ..
        } = event
        else {
            continue;
        };
        if changed_scope != scope || changed_key != key {
            continue;
        }
        let snapshot = match client
            .rpc(Request::CoordinationGet {
                scope,
                key: key.to_owned(),
            })
            .await?
        {
            Response::CoordinationSnapshot(snapshot) => snapshot,
            other => return Err(unexpected("coordination entry", &other)),
        };
        let version = snapshot.value.as_ref().map(|entry| entry.version);
        if version != current || snapshot.value.is_none() {
            current = version;
            emit(reporter, &snapshot);
        }
    }
}

fn emit(reporter: &Reporter, snapshot: &CoordinationSnapshot) {
    if reporter.json {
        // One compact object per line so a watcher can be consumed as NDJSON.
        let value: &Option<CoordinationValue> = &snapshot.value;
        if let Ok(line) = serde_json::to_string(&serde_json::json!({
            "value": value,
            "lock": snapshot.lock,
        })) {
            println!("{line}");
        }
    } else {
        print_snapshot(reporter, snapshot);
    }
}
