//! A trait-level fake Herdr over an in-memory model, shared by the ticker
//! and the agent subcommands a test runs. `snapshot()` is built from the
//! model, so callers decide from a snapshot as they do against Herdr.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::oneshot;

use super::{Agent, AgentStatus, Herdr, HerdrError, Pane, PaneId, Placed, Snapshot, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub name: String,
    pub kind: String,
    pub pane: PaneId,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub pane: PaneId,
    pub source: String,
    pub display_agent: String,
    pub tokens: Vec<(String, String)>,
    pub ttl_ms: u64,
}

#[derive(Debug, Clone)]
struct FakeAgent {
    agent: Agent,
    /// Snapshots left before Herdr detects the agent.
    hidden_for: u32,
}

#[derive(Default)]
struct Model {
    home: PathBuf,
    workspaces: u32,
    panes: BTreeMap<PaneId, Pane>,
    labels: BTreeMap<WorkspaceId, String>,
    agents: BTreeMap<PaneId, FakeAgent>,
    requests: Vec<String>,
    starts: Vec<Start>,
    prompts: Vec<(PaneId, String)>,
    keys: Vec<(PaneId, String)>,
    renames: Vec<(PaneId, String)>,
    closed: Vec<WorkspaceId>,
    focused: Vec<WorkspaceId>,
    notifications: Vec<(String, String)>,
    metadata: Vec<Metadata>,
    worktrees: Vec<(String, String, String)>,
    /// The next `agent.start` fails with this code; `agent_not_ready` leaves
    /// a blocked agent in the pane.
    start_error: Option<String>,
    /// Snapshots a started agent stays hidden for.
    detection_lag: u32,
    /// Snapshots a new pane stays hidden for.
    pane_lag: u32,
    /// Panes that are not in snapshots yet, with the snapshots left.
    hidden_panes: BTreeMap<PaneId, u32>,
    /// The next placement (`workspace.create`, `worktree.create`,
    /// `worktree.open`) does its work, then answers `OutcomeUnknown`.
    placement_unknown: bool,
    /// The next `agent.prompt` is delivered, then answers `OutcomeUnknown`.
    prompt_unknown: bool,
    /// The next `agent.start` fails as `NotSent` without doing anything.
    start_not_sent: bool,
    /// Every request fails as `NotSent`.
    down: bool,
    /// The next snapshot is built, then answered only after the test let
    /// it go: whatever runs meanwhile is newer than the snapshot.
    hold: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    /// Panes whose entries (the pane and its agent) do not parse: left out
    /// of snapshots and counted in `skipped`.
    unparsed: Vec<PaneId>,
    /// Panes whose agent entry alone does not parse.
    unparsed_agents: Vec<PaneId>,
}

#[derive(Clone)]
pub struct FakeHerdr {
    model: Arc<Mutex<Model>>,
}

fn not_found(what: &str, id: &str) -> HerdrError {
    HerdrError::Api {
        code: format!("{what}_not_found"),
        message: format!("{what} {id} not found"),
    }
}

impl Model {
    fn request(&mut self, method: &str) -> Result<(), HerdrError> {
        if self.down {
            return Err(HerdrError::NotSent("the fake Herdr is down".into()));
        }
        self.requests.push(method.into());
        Ok(())
    }

    fn new_workspace(&mut self, cwd: &str, label: &str) -> Pane {
        self.workspaces += 1;
        let n = self.workspaces;
        let workspace = WorkspaceId(format!("w{n}"));
        let pane = Pane {
            id: PaneId(format!("w{n}:p1")),
            workspace: workspace.clone(),
            tab: format!("w{n}:t1"),
            terminal: format!("term-{n}"),
            cwd: Some(cwd.into()),
            foreground_cwd: Some(cwd.into()),
            label: None,
        };
        self.labels.insert(workspace, label.into());
        self.panes.insert(pane.id.clone(), pane.clone());
        if self.pane_lag > 0 {
            self.hidden_panes.insert(pane.id.clone(), self.pane_lag);
        }
        pane
    }

    fn placed(&mut self, pane: Pane, worktree_path: Option<String>) -> Result<Placed, HerdrError> {
        let placed = Placed {
            workspace: pane.workspace,
            tab: pane.tab,
            pane: pane.id,
            cwd: pane.cwd.unwrap_or_default(),
            worktree_path,
        };
        if std::mem::take(&mut self.placement_unknown) {
            return Err(HerdrError::OutcomeUnknown("no answer in time".into()));
        }
        Ok(placed)
    }

    fn remove_workspace(&mut self, workspace: &WorkspaceId) {
        self.panes.retain(|_, p| p.workspace != *workspace);
        let panes = &self.panes;
        self.agents.retain(|pane, _| panes.contains_key(pane));
    }

    fn snapshot(&mut self) -> Result<Snapshot, HerdrError> {
        self.request("session.snapshot")?;
        let mut agents = Vec::new();
        for fake in self.agents.values_mut() {
            if fake.hidden_for > 0 {
                fake.hidden_for -= 1;
            } else {
                agents.push(fake.agent.clone());
            }
        }
        let hidden: Vec<PaneId> = self.hidden_panes.keys().cloned().collect();
        self.hidden_panes.retain(|_, left| {
            *left -= 1;
            *left > 0
        });
        let panes: BTreeMap<PaneId, Pane> = self
            .panes
            .iter()
            .filter(|(id, _)| !hidden.contains(id))
            .map(|(id, pane)| (id.clone(), pane.clone()))
            .collect();
        agents.retain(|a| !hidden.contains(&a.pane));
        let mut skipped = 0;
        let mut panes = panes;
        for pane in &self.unparsed {
            skipped += usize::from(panes.remove(pane).is_some());
            let before = agents.len();
            agents.retain(|a| a.pane != *pane);
            skipped += before - agents.len();
        }
        for pane in &self.unparsed_agents {
            let before = agents.len();
            agents.retain(|a| a.pane != *pane);
            skipped += before - agents.len();
        }
        Ok(Snapshot {
            version: "0.9.1".into(),
            protocol: 22,
            panes,
            agents,
            skipped,
        })
    }

    fn agent_in(&self, pane: &PaneId) -> Result<(), HerdrError> {
        match self.agents.get(pane) {
            Some(agent) if agent.hidden_for == 0 => Ok(()),
            _ => Err(not_found("agent", &pane.0)),
        }
    }
}

impl FakeHerdr {
    /// A fake whose worktrees are folders under `home/worktrees`.
    pub fn new(home: &Path) -> Self {
        FakeHerdr {
            model: Arc::new(Mutex::new(Model {
                home: home.to_path_buf(),
                ..Model::default()
            })),
        }
    }

    fn model(&self) -> MutexGuard<'_, Model> {
        self.model.lock().unwrap()
    }

    /// The method names of every request that reached the fake, in order.
    pub fn requests(&self) -> Vec<String> {
        self.model().requests.clone()
    }

    pub fn starts(&self) -> Vec<Start> {
        self.model().starts.clone()
    }

    pub fn prompts_to(&self, pane: &str) -> Vec<String> {
        self.model()
            .prompts
            .iter()
            .filter(|(p, _)| p.0 == pane)
            .map(|(_, text)| text.clone())
            .collect()
    }

    pub fn keys(&self) -> Vec<(PaneId, String)> {
        self.model().keys.clone()
    }

    pub fn renames(&self) -> Vec<(PaneId, String)> {
        self.model().renames.clone()
    }

    pub fn closed(&self) -> Vec<WorkspaceId> {
        self.model().closed.clone()
    }

    pub fn focused(&self) -> Vec<WorkspaceId> {
        self.model().focused.clone()
    }

    pub fn notifications(&self) -> Vec<(String, String)> {
        self.model().notifications.clone()
    }

    pub fn metadata(&self) -> Vec<Metadata> {
        self.model().metadata.clone()
    }

    /// `(cwd, branch, base)` of every worktree created.
    pub fn worktrees(&self) -> Vec<(String, String, String)> {
        self.model().worktrees.clone()
    }

    /// The label a workspace was created with.
    pub fn label(&self, workspace: &str) -> Option<String> {
        self.model()
            .labels
            .get(&WorkspaceId(workspace.into()))
            .cloned()
    }

    pub fn panes(&self) -> Vec<Pane> {
        self.model().panes.values().cloned().collect()
    }

    pub fn fail_next_start(&self, code: &str) {
        self.model().start_error = Some(code.into());
    }

    pub fn delay_detection(&self, snapshots: u32) {
        self.model().detection_lag = snapshots;
    }

    /// New panes stay out of the next `snapshots` snapshots, as when Herdr
    /// answers a placement before its pane list shows the pane.
    pub fn delay_panes(&self, snapshots: u32) {
        self.model().pane_lag = snapshots;
    }

    pub fn next_placement_unknown(&self) {
        self.model().placement_unknown = true;
    }

    pub fn next_prompt_unknown(&self) {
        self.model().prompt_unknown = true;
    }

    pub fn next_start_not_sent(&self) {
        self.model().start_not_sent = true;
    }

    /// Holds the next snapshot's answer: the first receiver fires once it
    /// is built, and the answer goes out when the sender is used.
    pub fn hold_next_snapshot(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (taken_tx, taken) = oneshot::channel();
        let (go, go_rx) = oneshot::channel();
        self.model().hold = Some((taken_tx, go_rx));
        (taken, go)
    }

    /// The pane's entries stop parsing (`true`) or parse again.
    pub fn unparsed(&self, pane: &str, unparsed: bool) {
        let mut model = self.model();
        let pane = PaneId(pane.into());
        model.unparsed.retain(|p| *p != pane);
        if unparsed {
            model.unparsed.push(pane);
        }
    }

    /// The agent entry of the pane stops parsing (`true`) or parses again;
    /// the pane itself still does.
    pub fn unparsed_agent(&self, pane: &str, unparsed: bool) {
        let mut model = self.model();
        let pane = PaneId(pane.into());
        model.unparsed_agents.retain(|p| *p != pane);
        if unparsed {
            model.unparsed_agents.push(pane);
        }
    }

    pub fn set_down(&self, down: bool) {
        self.model().down = down;
    }

    /// A pane that appeared outside the plugin, for example a person's split.
    pub fn add_pane(&self, cwd: &str) -> PaneId {
        self.model().new_workspace(cwd, "").id
    }

    /// Sets the status of the agent named `name`; a change of status or a
    /// repeated one is a new episode with a higher sequence.
    pub fn set_status(&self, name: &str, status: &str) {
        let mut model = self.model();
        let agent = model
            .agents
            .values_mut()
            .find(|a| a.agent.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no agent named {name}"));
        agent.agent.status = AgentStatus::parse(status);
        agent.agent.state_change_seq += 1;
    }

    /// The agent named `name` loses its name, as a natively resumed agent
    /// does.
    pub fn forget_name(&self, name: &str) {
        let mut model = self.model();
        if let Some(agent) = model
            .agents
            .values_mut()
            .find(|a| a.agent.name.as_deref() == Some(name))
        {
            agent.agent.name = None;
        }
    }

    /// The workspace goes away without a request, as when a person closes it.
    pub fn remove_workspace(&self, workspace: &str) {
        self.model()
            .remove_workspace(&WorkspaceId(workspace.into()));
    }

    /// Herdr restarted: panes stay, agents are gone.
    pub fn restart(&self) {
        self.model().agents.clear();
    }
}

impl Herdr for FakeHerdr {
    async fn snapshot(&self) -> Result<Snapshot, HerdrError> {
        let (snapshot, hold) = {
            let mut model = self.model();
            let snapshot = model.snapshot()?;
            (snapshot, model.hold.take())
        };
        if let Some((taken, go)) = hold {
            let _ = taken.send(());
            let _ = go.await;
        }
        Ok(snapshot)
    }

    async fn workspace_create(&self, cwd: &str, label: &str) -> Result<Placed, HerdrError> {
        let mut model = self.model();
        model.request("workspace.create")?;
        let pane = model.new_workspace(cwd, label);
        model.placed(pane, None)
    }

    async fn worktree_create(
        &self,
        cwd: &str,
        branch: &str,
        base: &str,
    ) -> Result<Placed, HerdrError> {
        let mut model = self.model();
        model.request("worktree.create")?;
        let path = model.home.join("worktrees").join(branch.replace('/', "-"));
        std::fs::create_dir_all(&path).unwrap();
        let path = path.to_string_lossy().into_owned();
        model
            .worktrees
            .push((cwd.into(), branch.into(), base.into()));
        let pane = model.new_workspace(&path, branch);
        model.placed(pane, Some(path))
    }

    async fn worktree_open(&self, cwd: &str, path: &str) -> Result<Placed, HerdrError> {
        let mut model = self.model();
        model.request("worktree.open")?;
        if cwd.is_empty() {
            return Err(HerdrError::Api {
                code: "not_git_worktree".into(),
                message: "Herdr worktree actions require a workspace inside a Git work tree".into(),
            });
        }
        let pane = model.new_workspace(path, path);
        model.placed(pane, Some(path.into()))
    }

    async fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane: &PaneId,
        args: &[String],
    ) -> Result<(), HerdrError> {
        let mut model = self.model();
        if std::mem::take(&mut model.start_not_sent) {
            return Err(HerdrError::NotSent(
                "the fake Herdr dropped the start".into(),
            ));
        }
        model.request("agent.start")?;
        model.starts.push(Start {
            name: name.into(),
            kind: kind.into(),
            pane: pane.clone(),
            args: args.to_vec(),
        });
        let Some(found) = model.panes.get(pane) else {
            return Err(not_found("pane", &pane.0));
        };
        let mut agent = Agent {
            pane: pane.clone(),
            kind: Some(kind.into()),
            name: Some(name.into()),
            status: AgentStatus::Idle,
            session: Some(format!("sess-{name}")),
            cwd: found.cwd.clone(),
            foreground_cwd: found.cwd.clone(),
            terminal: found.terminal.clone(),
            interactive_ready: true,
            launch_pending: false,
            state_change_seq: 1,
            state_labels: BTreeMap::new(),
        };
        let error = model.start_error.take();
        if error.is_some() && error.as_deref() != Some("agent_not_ready") {
            return Err(HerdrError::Api {
                code: error.unwrap_or_default(),
                message: "startup failed".into(),
            });
        }
        if error.is_some() {
            agent.status = AgentStatus::Blocked;
            agent.interactive_ready = false;
        }
        let hidden_for = model.detection_lag;
        model
            .agents
            .insert(pane.clone(), FakeAgent { agent, hidden_for });
        match error {
            Some(code) => Err(HerdrError::Api {
                code,
                message: "startup failed".into(),
            }),
            None => Ok(()),
        }
    }

    async fn agent_prompt(&self, pane: &PaneId, text: &str) -> Result<(), HerdrError> {
        let mut model = self.model();
        model.request("agent.prompt")?;
        model.agent_in(pane)?;
        model.prompts.push((pane.clone(), text.into()));
        if std::mem::take(&mut model.prompt_unknown) {
            return Err(HerdrError::OutcomeUnknown("no answer in time".into()));
        }
        Ok(())
    }

    async fn send_escape(&self, pane: &PaneId) -> Result<(), HerdrError> {
        let mut model = self.model();
        model.request("agent.send_keys")?;
        model.agent_in(pane)?;
        model.keys.push((pane.clone(), "esc".into()));
        Ok(())
    }

    async fn agent_rename(&self, pane: &PaneId, name: &str) -> Result<(), HerdrError> {
        let mut model = self.model();
        model.request("agent.rename")?;
        model.agent_in(pane)?;
        if let Some(agent) = model.agents.get_mut(pane) {
            agent.agent.name = Some(name.into());
        }
        model.renames.push((pane.clone(), name.into()));
        Ok(())
    }

    async fn workspace_close(&self, workspace: &WorkspaceId) -> Result<(), HerdrError> {
        let mut model = self.model();
        model.request("workspace.close")?;
        if !model.panes.values().any(|p| p.workspace == *workspace) {
            return Err(not_found("workspace", &workspace.0));
        }
        model.remove_workspace(workspace);
        model.closed.push(workspace.clone());
        Ok(())
    }

    async fn workspace_focus(&self, workspace: &WorkspaceId) -> Result<(), HerdrError> {
        let mut model = self.model();
        model.request("workspace.focus")?;
        model.focused.push(workspace.clone());
        Ok(())
    }

    async fn notification_show(&self, title: &str, body: &str) -> Result<(), HerdrError> {
        let mut model = self.model();
        model.request("notification.show")?;
        model.notifications.push((title.into(), body.into()));
        Ok(())
    }

    async fn report_metadata(
        &self,
        pane: &PaneId,
        source: &str,
        display_agent: &str,
        tokens: &[(String, String)],
        ttl_ms: u64,
    ) -> Result<(), HerdrError> {
        let mut model = self.model();
        model.request("pane.report_metadata")?;
        model.metadata.push(Metadata {
            pane: pane.clone(),
            source: source.into(),
            display_agent: display_agent.into(),
            tokens: tokens.to_vec(),
            ttl_ms,
        });
        Ok(())
    }

    async fn pane_current(&self, caller: &PaneId) -> Result<Pane, HerdrError> {
        let mut model = self.model();
        model.request("pane.current")?;
        model
            .panes
            .get(caller)
            .cloned()
            .ok_or_else(|| not_found("pane", &caller.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn starts_are_seen_in_later_snapshots_and_the_knobs_hold() {
        let home = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(home.path());
        let placed = herdr.workspace_create("/run", "DATA-1 Fix").await.unwrap();
        assert_eq!(placed.pane, PaneId("w1:p1".into()));
        assert_eq!(herdr.label("w1").as_deref(), Some("DATA-1 Fix"));

        herdr.delay_detection(1);
        herdr
            .agent_start("data-1-coordinator", "claude", &placed.pane, &[])
            .await
            .unwrap();
        assert!(herdr.snapshot().await.unwrap().agents.is_empty());
        let snapshot = herdr.snapshot().await.unwrap();
        assert_eq!(
            snapshot.agents[0].session.as_deref(),
            Some("sess-data-1-coordinator")
        );
        assert_eq!(snapshot.agents[0].cwd.as_deref(), Some("/run"));

        herdr.delay_detection(0);
        herdr.fail_next_start("agent_not_ready");
        let error = herdr
            .agent_start("data-1-coordinator", "claude", &placed.pane, &[])
            .await
            .unwrap_err();
        assert!(matches!(error, HerdrError::Api { ref code, .. } if code == "agent_not_ready"));
        assert_eq!(
            herdr.snapshot().await.unwrap().agents[0].status,
            AgentStatus::Blocked
        );

        herdr.next_placement_unknown();
        assert!(matches!(
            herdr.workspace_create("/run", "again").await,
            Err(HerdrError::OutcomeUnknown(_))
        ));
        assert_eq!(
            herdr.snapshot().await.unwrap().panes.len(),
            2,
            "the work was done"
        );

        herdr.set_down(true);
        assert!(matches!(
            herdr.snapshot().await,
            Err(HerdrError::NotSent(_))
        ));
        herdr.set_down(false);
        herdr.workspace_close(&placed.workspace).await.unwrap();
        let snapshot = herdr.snapshot().await.unwrap();
        assert!(snapshot.agents.is_empty() && snapshot.panes.len() == 1);
        assert_eq!(herdr.closed(), [placed.workspace]);
    }
}
