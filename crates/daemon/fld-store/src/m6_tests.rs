//! Schema v9: coordination state/locks and durable team runs.
use super::*;
use fl_model::coordination::CoordinationScope;
use fl_model::team::{TeamRole, TeamRoleSpec, TeamRun, TeamRunSpec, TeamState};
use fl_model::workspace::{Project, ProjectKind};
use fl_model::{ProjectId, WorktreeId};
use tempfile::TempDir;

fn fixture() -> (TempDir, Database, WorktreeId) {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let workspace = database.workspace_tree().unwrap().workspaces[0].id;
    let project = Project {
        id: ProjectId::new(),
        workspace,
        name: "repo".into(),
        path: "/srv/repo".into(),
        kind: ProjectKind::Git,
        default_branch: Some("main".into()),
        worktree_root: "/srv/repo-worktrees".into(),
        order: 0,
    };
    let worktree = fl_model::workspace::Worktree {
        id: WorktreeId::new(),
        project: project.id,
        path: "/srv/repo".into(),
        branch: Some("main".into()),
        target: None,
        label: "main".into(),
        is_primary: true,
        created_ms: 1,
        last_active_ms: 1,
    };
    let id = worktree.id;
    database
        .add_project(project, std::slice::from_ref(&worktree))
        .unwrap();
    (temporary, database, id)
}

#[test]
fn coordination_versions_compare_and_set_and_delete_durably() {
    let (temporary, mut database, worktree) = fixture();
    let scope = CoordinationScope::Worktree(worktree);
    let key = "plan/owner";

    assert!(
        database
            .coordination_get(scope, key, 10)
            .unwrap()
            .value
            .is_none()
    );
    let first = database
        .coordination_set(scope, key, "alice", Some(0), 10)
        .unwrap();
    assert_eq!(first.version, 1);
    let second = database
        .coordination_set(scope, key, "bob", Some(1), 11)
        .unwrap();
    assert_eq!(second.version, 2);
    let stale = database.coordination_set(scope, key, "carol", Some(1), 12);
    assert!(matches!(stale, Err(StoreError::Conflict(_))));
    let create_again = database.coordination_set(scope, key, "carol", Some(0), 12);
    assert!(matches!(create_again, Err(StoreError::Conflict(_))));
    // Unconditional writes still bump the version.
    assert_eq!(
        database
            .coordination_set(scope, key, "dave", None, 13)
            .unwrap()
            .version,
        3
    );
    assert!(matches!(
        database.coordination_delete(scope, key, Some(2)),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        database.coordination_delete(scope, key, Some(3)).unwrap(),
        0
    );
    assert!(matches!(
        database.coordination_delete(scope, key, None),
        Err(StoreError::Missing(_))
    ));
    // Recreating after a delete restarts at version 1.
    assert_eq!(
        database
            .coordination_set(scope, key, "eve", Some(0), 14)
            .unwrap()
            .version,
        1
    );

    // Global and workspace scopes are independent namespaces.
    let global = CoordinationScope::Global;
    database
        .coordination_set(global, key, "global", None, 20)
        .unwrap();
    assert_eq!(
        database
            .coordination_get(global, key, 20)
            .unwrap()
            .value
            .unwrap()
            .value,
        "global"
    );
    let missing = CoordinationScope::Worktree(WorktreeId::from_bytes([9; 16]));
    assert!(matches!(
        database.coordination_set(missing, key, "x", None, 20),
        Err(StoreError::Missing(_))
    ));

    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    assert_eq!(
        database
            .coordination_get(scope, key, 30)
            .unwrap()
            .value
            .unwrap()
            .value,
        "eve"
    );
    assert_eq!(
        database
            .coordination_get(global, key, 30)
            .unwrap()
            .value
            .unwrap()
            .version,
        1
    );
}

#[test]
fn coordination_locks_respect_owner_and_ttl() {
    let (temporary, mut database, worktree) = fixture();
    let scope = CoordinationScope::Workspace(database.workspace_tree().unwrap().workspaces[0].id);
    let _ = worktree;
    let key = "build";
    let lock = database
        .coordination_lock(scope, key, "agent-a", 5_000, 1_000)
        .unwrap();
    assert_eq!(lock.expires_ms, 6_000);
    let held = database.coordination_lock(scope, key, "agent-b", 5_000, 2_000);
    assert!(matches!(held, Err(StoreError::Conflict(_))));
    assert_eq!(
        database
            .coordination_get(scope, key, 2_000)
            .unwrap()
            .lock
            .unwrap()
            .owner,
        "agent-a"
    );
    // The same owner extends the lock.
    assert_eq!(
        database
            .coordination_lock(scope, key, "agent-a", 10_000, 3_000)
            .unwrap()
            .expires_ms,
        13_000
    );
    assert!(matches!(
        database.coordination_unlock(scope, key, "agent-b"),
        Err(StoreError::NotOwner(_))
    ));
    // After expiry another owner may take it.
    assert_eq!(
        database
            .coordination_lock(scope, key, "agent-b", 5_000, 14_000)
            .unwrap()
            .owner,
        "agent-b"
    );
    assert!(
        database
            .coordination_get(scope, key, 19_000)
            .unwrap()
            .lock
            .is_none()
    );
    assert!(matches!(
        database.coordination_lock(scope, key, "agent-c", 100, 20_000),
        Err(StoreError::Invalid(_))
    ));
    database.coordination_unlock(scope, key, "agent-b").unwrap();
    assert!(
        database
            .coordination_get(scope, key, 20_000)
            .unwrap()
            .lock
            .is_none()
    );

    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    assert!(
        database
            .coordination_get(scope, key, 20_000)
            .unwrap()
            .lock
            .is_none()
    );
}

fn run_fixture(worktree: WorktreeId) -> TeamRun {
    let spec = TeamRunSpec {
        lead: Some("architect".into()),
        roles: vec![
            TeamRoleSpec {
                label: "architect".into(),
                provider: "claude".into(),
                model: None,
                prompt: "design".into(),
            },
            TeamRoleSpec {
                label: "builder".into(),
                provider: "codex".into(),
                model: None,
                prompt: "build".into(),
            },
        ],
    };
    spec.validate().unwrap();
    TeamRun {
        id: uuid::Uuid::new_v4(),
        worktree,
        lead: spec.lead_label().into(),
        state: TeamState::Starting,
        roles: spec
            .roles
            .iter()
            .enumerate()
            .map(|(index, role)| TeamRole {
                index: index as u16,
                label: role.label.clone(),
                provider: role.provider.clone(),
                session: None,
                tab: None,
                state: TeamState::Starting,
                report: None,
            })
            .collect(),
        created_ms: 1,
        updated_ms: 1,
        fan_in_sent: false,
    }
}

#[test]
fn team_runs_settle_once_and_keep_reports_across_reopen() {
    let (temporary, mut database, worktree) = fixture();
    let run = run_fixture(worktree);
    assert!(database.workspace_tree().unwrap().worktrees[0].id == worktree);
    database.create_team_run(&run).unwrap();
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    database.team_role_started(run.id, 0, a, None, 2).unwrap();
    database.team_role_started(run.id, 1, b, None, 3).unwrap();
    let (current, fan_in) = database
        .team_role_state(run.id, 0, TeamState::Finished, 4)
        .unwrap();
    assert_eq!(current.state, TeamState::Running);
    assert!(!fan_in);
    database
        .record_team_report(
            run.id,
            "architect",
            "design ready",
            Some("out/design.md"),
            5,
        )
        .unwrap();
    let (current, fan_in) = database
        .team_role_state(run.id, 1, TeamState::Finished, 6)
        .unwrap();
    assert_eq!(current.state, TeamState::Finished);
    assert!(fan_in);
    assert!(current.fan_in_sent);
    let (_, again) = database.settle_team_run(run.id, 7).unwrap();
    assert!(!again, "fan-in must be claimed exactly once");

    // A failure settles the run as failed, never as finished.
    let mut failed = run_fixture(worktree);
    failed.id = uuid::Uuid::new_v4();
    failed.created_ms = 2;
    failed.updated_ms = 2;
    database.create_team_run(&failed).unwrap();
    database
        .team_role_started(failed.id, 0, uuid::Uuid::new_v4(), None, 8)
        .unwrap();
    database
        .team_role_started(failed.id, 1, uuid::Uuid::new_v4(), None, 8)
        .unwrap();
    database
        .team_role_state(failed.id, 0, TeamState::Failed, 9)
        .unwrap();
    let (current, fan_in) = database
        .team_role_state(failed.id, 1, TeamState::Finished, 10)
        .unwrap();
    assert_eq!(current.state, TeamState::Failed);
    assert!(fan_in);

    let listed = database.team_runs(worktree).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, failed.id, "newest first");
    assert_eq!(
        database.latest_team_run(worktree).unwrap().unwrap().id,
        failed.id
    );

    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    let stored = database.team_run(run.id).unwrap();
    assert_eq!(stored.roles[0].session, Some(a));
    assert_eq!(
        stored.roles[0]
            .report
            .as_ref()
            .unwrap()
            .result_file
            .as_deref(),
        Some("out/design.md")
    );
    assert!(stored.fan_in_sent);
}

#[test]
fn team_cancel_marks_non_terminal_roles_and_a_missing_run_is_not_found() {
    let (_temporary, mut database, worktree) = fixture();
    let run = run_fixture(worktree);
    database.create_team_run(&run).unwrap();
    database
        .team_role_state(run.id, 0, TeamState::Finished, 2)
        .unwrap();
    let cancelled = database.cancel_team_run(run.id, 3).unwrap();
    assert_eq!(cancelled.state, TeamState::Cancelled);
    assert_eq!(cancelled.roles[0].state, TeamState::Finished);
    assert_eq!(cancelled.roles[1].state, TeamState::Cancelled);
    assert!(!cancelled.fan_in_sent);
    assert!(matches!(
        database.team_run(uuid::Uuid::from_bytes([9; 16])),
        Err(StoreError::Missing(_))
    ));
    assert!(matches!(
        database.record_team_report(run.id, "nobody", "x", None, 4),
        Err(StoreError::Missing(_))
    ));
}

#[test]
fn schema_nine_backs_up_v8_and_creates_the_new_tables() {
    let (temporary, database, _worktree) = fixture();
    database
        .connection
        .execute_batch(
            "DROP TABLE IF EXISTS coordination_state; DROP TABLE IF EXISTS coordination_locks; \
             DROP TABLE IF EXISTS team_roles; DROP TABLE IF EXISTS team_runs; \
             PRAGMA user_version=8;",
        )
        .unwrap();
    drop(database);
    let mut database = Database::open(temporary.path()).unwrap();
    assert_eq!(schema_version(&database.connection).unwrap(), 9);
    let backups: Vec<_> = std::fs::read_dir(temporary.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("state.db.before-v8-to-v9-")
        })
        .collect();
    assert_eq!(backups.len(), 1, "v9 migration must back up v8 once");
    let backup =
        Connection::open_with_flags(backups[0].path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(schema_version(&backup).unwrap(), 8);
    let mode: String = backup
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete", "the backup is one self-contained file");
    // The migrated database accepts the new state.
    database
        .coordination_set(CoordinationScope::Global, "k", "v", Some(0), 1)
        .unwrap();
    drop(database);
    assert_eq!(
        std::fs::read_dir(temporary.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("state.db.before-v8-to-v9-")
            })
            .count(),
        1,
        "reopening at v9 does not migrate or back up again"
    );
}
