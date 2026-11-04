//! Workspace-local sidebar sections. Unknown observations never satisfy a filter.
use serde::{Deserialize, Serialize};
use crate::{SectionId, WorkspaceId, WorktreeId};
use crate::forge::{CheckStatus, ForgeState, PrState, ReviewDecision};
use crate::workspace::{GitSummary, ScriptKind, ScriptRunState, ScriptState, Worktree};

pub const MAX_SECTIONS: usize = 256;
pub const MAX_MEMBERSHIPS: usize = 4096;
pub const MAX_SECTION_NAME: usize = 128;
pub const MAX_BRANCH_GLOB: usize = 256;

/// Structured adapter observations only; terminal text is not an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState { Running, Waiting, Idle, Exited, Failed }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState { Idle, Running, Succeeded, Failed, ExitedUnknown }
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionFilter {
    pub pr: Option<PrState>,
    pub review: Option<ReviewDecision>,
    pub checks: Option<CheckStatus>,
    pub agent: Option<AgentState>,
    pub run: Option<RunState>,
    /// Whole-branch match. `*` matches any sequence, `?` one Unicode scalar.
    /// All other characters are literal (including slash and brackets).
    pub branch_glob: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SectionKind { Manual, Filter(SectionFilter) }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub id: SectionId,
    pub workspace: WorkspaceId,
    pub name: String,
    /// Ascending display order, with stable identity as tie breaker.
    pub order: u32,
    /// Higher filter priority wins; ties use display order then identity.
    pub priority: i32,
    pub kind: SectionKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionMembership {
    pub worktree: WorktreeId,
    pub manual: Option<SectionId>,
    pub pinned: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionsState {
    pub rev: u64,
    pub sections: Vec<Section>,
    pub memberships: Vec<SectionMembership>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SectionMutation {
    Upsert(Section),
    Delete(SectionId),
    Assign { worktree: WorktreeId, section: Option<SectionId> },
    Pin { worktree: WorktreeId, pinned: bool },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionPlacement { Pinned, Section(SectionId), Unassigned }

impl Section {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty() || self.name.len() > MAX_SECTION_NAME || self.name.chars().any(char::is_control) {
            return Err("section name must be 1–128 bytes without control characters");
        }
        if let SectionKind::Filter(filter) = &self.kind {
            if let Some(glob) = &filter.branch_glob {
                if glob.is_empty() || glob.len() > MAX_BRANCH_GLOB || glob.chars().any(char::is_control) {
                    return Err("branch glob must be 1–256 bytes without control characters");
                }
            }
        }
        Ok(())
    }
}
impl SectionsState {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.sections.len() > MAX_SECTIONS || self.memberships.len() > MAX_MEMBERSHIPS { return Err("too many sections or memberships"); }
        for (index, section) in self.sections.iter().enumerate() {
            section.validate()?;
            if self.sections[..index].iter().any(|s| s.id == section.id) { return Err("duplicate section identity"); }
        }
        for (index, membership) in self.memberships.iter().enumerate() {
            if self.memberships[..index].iter().any(|m| m.worktree == membership.worktree) { return Err("duplicate section membership"); }
            if let Some(id) = membership.manual {
                if !self.sections.iter().any(|s| s.id == id && s.kind == SectionKind::Manual) { return Err("manual assignment must name a manual section"); }
            }
        }
        Ok(())
    }
}

/// Pure classification: pin, explicit manual assignment, highest priority
/// matching filter in the workspace, then Unassigned. No side effects or time.
pub fn classify(
    state: &SectionsState, workspace: WorkspaceId, worktree: &Worktree,
    git: Option<&GitSummary>, forge: Option<&ForgeState>, scripts: Option<&[ScriptState]>,
    agent: Option<AgentState>,
) -> SectionPlacement {
    if let Some(m) = state.memberships.iter().find(|m| m.worktree == worktree.id) {
        if m.pinned { return SectionPlacement::Pinned; }
        if let Some(id) = m.manual {
            if state.sections.iter().any(|s| s.id == id && s.workspace == workspace && s.kind == SectionKind::Manual) {
                return SectionPlacement::Section(id);
            }
        }
    }
    let branch = match git { Some(git) => git.branch.as_deref(), None => worktree.branch.as_deref() };
    let pr = forge.filter(|f| f.worktree == worktree.id && f.unavailable.is_none()).and_then(|f| f.pr.as_ref());
    let checks = pr.and_then(|p| {
        if p.checks.is_empty() { return None; }
        Some(if p.checks.iter().any(|c| c.status == CheckStatus::Failure) { CheckStatus::Failure }
        else if p.checks.iter().any(|c| c.status == CheckStatus::Pending) { CheckStatus::Pending }
        else if p.checks.iter().any(|c| c.status == CheckStatus::Success) { CheckStatus::Success }
        else { CheckStatus::Neutral })
    });
    // Aggregate named run targets only; setup/teardown are not run state.
    let run = scripts.and_then(|scripts| {
        let mut known = None;
        for script in scripts.iter().filter(|s| s.worktree == worktree.id && matches!(s.kind, ScriptKind::Run(_))) {
            let value = match script.state {
                ScriptRunState::Running => RunState::Running,
                ScriptRunState::Idle => RunState::Idle,
                ScriptRunState::Exited { code: Some(0) } => RunState::Succeeded,
                ScriptRunState::Exited { code: Some(_) } => RunState::Failed,
                ScriptRunState::Exited { code: None } => RunState::ExitedUnknown,
            };
            let rank = |r| match r { RunState::Running => 5, RunState::Failed => 4, RunState::ExitedUnknown => 3, RunState::Idle => 2, RunState::Succeeded => 1 };
            if known.is_none_or(|old| rank(value) > rank(old)) { known = Some(value); }
        }
        known
    });
    state.sections.iter().filter(|s| s.workspace == workspace).filter(|s| {
        let SectionKind::Filter(f) = &s.kind else { return false; };
        f.pr.is_none_or(|want| pr.is_some_and(|p| p.state == want))
            && f.review.is_none_or(|want| pr.and_then(|p| p.review) == Some(want))
            && f.checks.is_none_or(|want| checks == Some(want))
            && f.agent.is_none_or(|want| agent == Some(want))
            && f.run.is_none_or(|want| run == Some(want))
            && f.branch_glob.as_deref().is_none_or(|glob| branch.is_some_and(|b| branch_matches(glob, b)))
    }).min_by(|a, b| b.priority.cmp(&a.priority).then(a.order.cmp(&b.order)).then(a.id.cmp(&b.id)))
        .map_or(SectionPlacement::Unassigned, |s| SectionPlacement::Section(s.id))
}

/// Bounded greedy wildcard matcher; no regex interpretation or recursion.
pub fn branch_matches(pattern: &str, branch: &str) -> bool {
    let mut p = pattern.char_indices().peekable();
    let mut b = branch.char_indices().peekable();
    let mut star = None;
    loop {
        match (p.peek().copied(), b.peek().copied()) {
            (Some((_, '*')), _) => { p.next(); star = Some((p.clone(), b.clone())); }
            (Some((_, pc)), Some((_, bc))) if pc == '?' || pc == bc => { p.next(); b.next(); }
            (None, None) => return true,
            _ => {
                let Some((after_star, mut retry)) = star.clone() else { return false; };
                if retry.next().is_none() { return false; }
                p = after_star.clone(); b = retry.clone(); star = Some((after_star, retry));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectId;
    fn fixture() -> (WorkspaceId, Worktree, SectionsState) {
        let workspace = WorkspaceId::new();
        let tree = Worktree { id: WorktreeId::new(), project: ProjectId::new(), path: "/tmp/repo".into(), branch: Some("feature/界".into()), target: None, label: "test".into(), is_primary: false, created_ms: 0, last_active_ms: 0 };
        let state = SectionsState { rev: 1, sections: vec![Section { id: SectionId::new(), workspace, name: "Features".into(), order: 0, priority: 10, kind: SectionKind::Filter(SectionFilter { branch_glob: Some("feature/?".into()), ..Default::default() }) }], memberships: vec![] };
        (workspace, tree, state)
    }
    #[test] fn precedence_and_unknown() {
        let (w, t, mut s) = fixture();
        let filter = s.sections[0].id;
        assert_eq!(classify(&s,w,&t,None,None,None,None), SectionPlacement::Section(filter));
        let manual = Section { id: SectionId::new(), workspace: w, name: "Mine".into(), order: 9, priority: -10, kind: SectionKind::Manual };
        s.memberships.push(SectionMembership { worktree: t.id, manual: Some(manual.id), pinned: false });
        s.sections.push(manual.clone());
        assert_eq!(classify(&s,w,&t,None,None,None,None), SectionPlacement::Section(manual.id));
        s.memberships[0].pinned = true;
        assert_eq!(classify(&s,w,&t,None,None,None,None), SectionPlacement::Pinned);
        s.memberships.clear();
        let SectionKind::Filter(f) = &mut s.sections[0].kind else { unreachable!() }; f.agent = Some(AgentState::Idle);
        assert_eq!(classify(&s,w,&t,None,None,None,None), SectionPlacement::Unassigned);
        assert_eq!(classify(&s,w,&t,None,None,None,Some(AgentState::Idle)), SectionPlacement::Section(filter));
    }
    #[test] fn priority_and_missing_observations() {
        let (w,t,mut s) = fixture();
        let mut second = s.sections[0].clone(); second.id = SectionId::new(); second.priority += 1;
        s.sections.push(second.clone());
        assert_eq!(classify(&s,w,&t,None,None,None,None), SectionPlacement::Section(second.id));
        for field in 0..5 {
            let mut f = SectionFilter::default();
            match field { 0 => f.pr = Some(PrState::Open), 1 => f.review = Some(ReviewDecision::ReviewRequired), 2 => f.checks = Some(CheckStatus::Success), 3 => f.run = Some(RunState::Idle), _ => f.branch_glob = Some("*".into()) }
            s.sections.truncate(1); s.sections[0].kind = SectionKind::Filter(f);
            let detached = GitSummary::default();
            assert_eq!(classify(&s,w,&t,Some(&detached),None,Some(&[]),None), SectionPlacement::Unassigned);
        }
    }
    #[test] fn run_predicates_do_not_confuse_setup_or_unknown_exits() {
        let (w,t,mut s) = fixture();
        let id = s.sections[0].id;
        s.sections[0].kind = SectionKind::Filter(SectionFilter { run: Some(RunState::Succeeded), ..Default::default() });
        let mut scripts = vec![ScriptState { worktree: t.id, kind: ScriptKind::Setup, terminal: None, state: ScriptRunState::Exited { code: Some(0) } }];
        assert_eq!(classify(&s,w,&t,None,None,Some(&scripts),None), SectionPlacement::Unassigned);
        scripts[0].kind = ScriptKind::Run(None);
        assert_eq!(classify(&s,w,&t,None,None,Some(&scripts),None), SectionPlacement::Section(id));
        scripts[0].state = ScriptRunState::Exited { code: None };
        assert_eq!(classify(&s,w,&t,None,None,Some(&scripts),None), SectionPlacement::Unassigned);
        s.sections[0].kind = SectionKind::Filter(SectionFilter { run: Some(RunState::Running), ..Default::default() });
        scripts.push(ScriptState { worktree: t.id, kind: ScriptKind::Run(Some("server".into())), terminal: None, state: ScriptRunState::Running });
        assert_eq!(classify(&s,w,&t,None,None,Some(&scripts),None), SectionPlacement::Section(id));
        scripts.reverse();
        assert_eq!(classify(&s,w,&t,None,None,Some(&scripts),None), SectionPlacement::Section(id));
    }
    #[test] fn cross_workspace_manual_assignment_never_leaks() {
        let (w,t,mut s) = fixture();
        let manual = Section { id: SectionId::new(), workspace: WorkspaceId::new(), name: "Elsewhere".into(), order: 0, priority: 0, kind: SectionKind::Manual };
        s.memberships.push(SectionMembership { worktree: t.id, manual: Some(manual.id), pinned: false });
        s.sections.push(manual);
        assert_eq!(classify(&s,w,&t,None,None,None,None), SectionPlacement::Section(s.sections[0].id));
    }
    #[test] fn forge_predicates_are_conjunctive_and_neutral_is_not_success() {
        use crate::forge::{Check, MergeMethods, Mergeable, PullRequest};
        let (w,t,mut s) = fixture();
        let id = s.sections[0].id;
        s.sections[0].kind = SectionKind::Filter(SectionFilter {
            pr: Some(PrState::Open), review: Some(ReviewDecision::Approved),
            checks: Some(CheckStatus::Neutral), branch_glob: Some("feature/*".into()),
            ..Default::default()
        });
        let mut forge = ForgeState { worktree: t.id, unavailable: None, fetched_ms: 1,
            methods: MergeMethods::default(), pr: Some(PullRequest {
                number: 1, url: "https://example.test/pr/1".into(), title: "Test".into(),
                state: PrState::Open, draft: false, base: "main".into(), head: "feature/界".into(),
                mergeable: Mergeable::Unknown, review: Some(ReviewDecision::Approved),
                checks: vec![Check { name: "skipped".into(), status: CheckStatus::Neutral, url: None }],
                unresolved_threads: 0,
            }),
        };
        assert_eq!(classify(&s,w,&t,None,Some(&forge),None,None), SectionPlacement::Section(id));
        forge.pr.as_mut().unwrap().review = None;
        assert_eq!(classify(&s,w,&t,None,Some(&forge),None,None), SectionPlacement::Unassigned);
        forge.pr.as_mut().unwrap().review = Some(ReviewDecision::Approved);
        let SectionKind::Filter(f) = &mut s.sections[0].kind else { unreachable!() };
        f.checks = Some(CheckStatus::Success);
        assert_eq!(classify(&s,w,&t,None,Some(&forge),None,None), SectionPlacement::Unassigned);
        forge.pr.as_mut().unwrap().checks[0].status = CheckStatus::Success;
        assert_eq!(classify(&s,w,&t,None,Some(&forge),None,None), SectionPlacement::Section(id));
        forge.unavailable = Some("offline".into());
        assert_eq!(classify(&s,w,&t,None,Some(&forge),None,None), SectionPlacement::Unassigned);
    }
    #[test] fn glob_boundaries() {
        for (p,b,want) in [("feature/*","feature/a/b",true),("?","界",true),("[ab]","a",false),("a","ba",false),("*a?","ab",true),("*a","ab",false),("","",true)] { assert_eq!(branch_matches(p,b),want,"{p} {b}"); }
        let (_,_,mut s) = fixture(); s.sections[0].name = "x".repeat(MAX_SECTION_NAME + 1); assert!(s.validate().is_err());
    }
}
