//! Durable Fast/Thorough routes and prompt templates (schema v5 → v6).
//!
//! One bounded snapshot with a revision; every edit is compare-and-set so two
//! clients cannot silently overwrite each other's routing configuration.
use super::*;
use fl_model::routing::{MAX_ROUTES, MAX_TEMPLATES, RoutingMutation, RoutingState, Scope};

const MAX_ROUTING_BYTES: usize = 512 * 1024;
pub(super) const SCHEMA_V6: &str = "
    CREATE TABLE routing (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 524288)
    ) STRICT;
";

impl Database {
    pub fn routing_state(&self) -> Result<RoutingState> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_routing(&transaction)?;
        transaction.commit()?;
        Ok(state)
    }

    pub fn mutate_routing(
        &mut self,
        expected_rev: u64,
        mutation: RoutingMutation,
    ) -> Result<RoutingState> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = load_routing(&transaction)?;
        if state.rev != expected_rev {
            return Err(StoreError::RevisionConflict {
                expected: state.rev,
                got: expected_rev,
            });
        }
        check_scope(&transaction, mutation_scope(&mutation))?;
        state
            .apply(mutation)
            .map_err(|e| StoreError::Invalid(e.into()))?;
        check_references(&transaction, &state)?;
        let bytes = postcard::to_stdvec(&state)?;
        if bytes.len() > MAX_ROUTING_BYTES {
            return Err(StoreError::Invalid(
                "routing state exceeds byte limit".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO routing(singleton,state) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET state=excluded.state",
            [bytes],
        )?;
        transaction.commit()?;
        Ok(state)
    }
}

fn mutation_scope(mutation: &RoutingMutation) -> Scope {
    match mutation {
        RoutingMutation::SetRoute(route) => route.scope,
        RoutingMutation::RemoveRoute { scope, .. } => *scope,
        RoutingMutation::SetTemplate(template) => template.scope,
        RoutingMutation::RemoveTemplate { scope, .. } => *scope,
    }
}

fn check_scope(connection: &Connection, scope: Scope) -> Result<()> {
    match scope {
        Scope::Global => Ok(()),
        Scope::Workspace(id) => {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if exists {
                Ok(())
            } else {
                Err(StoreError::Missing("routing workspace not found".into()))
            }
        }
        Scope::Project(id) => {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if exists {
                Ok(())
            } else {
                Err(StoreError::Missing("routing project not found".into()))
            }
        }
    }
}

fn check_references(connection: &Connection, state: &RoutingState) -> Result<()> {
    for scope in state
        .routes
        .iter()
        .map(|route| route.scope)
        .chain(state.templates.iter().map(|template| template.scope))
    {
        check_scope(connection, scope)?;
    }
    Ok(())
}

fn load_routing(connection: &Connection) -> Result<RoutingState> {
    let bytes: Option<Vec<u8>> = connection
        .query_row("SELECT state FROM routing WHERE singleton=1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(bytes) = bytes else {
        return Ok(RoutingState::default());
    };
    if bytes.len() > MAX_ROUTING_BYTES {
        return Err(corrupt("oversized routing snapshot"));
    }
    let state: RoutingState = postcard::from_bytes(&bytes)?;
    state.validate().map_err(corrupt)?;
    if state.routes.len() > MAX_ROUTES || state.templates.len() > MAX_TEMPLATES {
        return Err(corrupt("routing snapshot exceeds entry limits"));
    }
    Ok(state)
}

pub(super) fn initial_routing() -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(&RoutingState::default())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::routing::{GitTask, PromptTemplate, RouteKey, RouteOverride, RouteSpec};

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn routes_and_templates_survive_reopen_and_reject_stale_revisions() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let workspace = db.workspace_tree().unwrap().workspaces[0].id;
        let mut route = RouteSpec::new("claude");
        route.model = Some("sonnet".into());
        let state = db
            .mutate_routing(
                0,
                RoutingMutation::SetRoute(RouteOverride {
                    scope: Scope::Workspace(workspace),
                    key: RouteKey::Group(fl_model::routing::RouteGroup::Fast),
                    route,
                }),
            )
            .unwrap();
        assert_eq!(state.rev, 1);
        let state = db
            .mutate_routing(
                1,
                RoutingMutation::SetTemplate(PromptTemplate {
                    scope: Scope::Global,
                    action: GitTask::FixCi,
                    body: "Fix {{branch}}".into(),
                }),
            )
            .unwrap();
        assert_eq!(state.rev, 2);
        let stale = db.mutate_routing(
            1,
            RoutingMutation::RemoveTemplate {
                scope: Scope::Global,
                action: GitTask::FixCi,
            },
        );
        assert!(matches!(
            stale,
            Err(StoreError::RevisionConflict {
                expected: 2,
                got: 1
            })
        ));
        drop(db);
        let db = Database::open(dir.path()).unwrap();
        let state = db.routing_state().unwrap();
        assert_eq!(state.rev, 2);
        assert_eq!(
            state
                .route(GitTask::Commit, fl_model::ProjectId::new(), workspace)
                .unwrap()
                .provider,
            "claude"
        );
    }

    #[test]
    fn unknown_scopes_are_refused_without_changing_the_snapshot() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let err = db
            .mutate_routing(
                0,
                RoutingMutation::SetRoute(RouteOverride {
                    scope: Scope::Workspace(fl_model::WorkspaceId::new()),
                    key: RouteKey::Action(GitTask::Commit),
                    route: RouteSpec::new("claude"),
                }),
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::Missing(_)), "{err:?}");
        assert_eq!(db.routing_state().unwrap(), RoutingState::default());
    }
}
