use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task;
use tracing::{debug, info, info_span, instrument, warn, Instrument};

use crate::config::{Config, PrewarmRepository};
use crate::instance::{self, InstanceKey, InstanceMap, WorkspaceRoot};
use crate::lsp::{InitializationOptions, InitializeParams, WorkspaceFolder};

const WORKSPACE_ROOT_ENV: &str = "${LSP_HERD_WORKSPACE_ROOT}";
const GIT_ROOT_ENV: &str = "${LSP_HERD_GIT_ROOT}";
const PREWARM_SPAWN_INTERVAL: Duration = Duration::from_secs(60);

const URI_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

#[derive(Clone)]
pub struct PrewarmTracker {
    state: Arc<Mutex<State>>,
    poll_interval: Duration,
}

#[derive(Default)]
struct State {
    templates: HashMap<TemplateKey, PrewarmTemplate>,
    repositories: HashMap<RepositoryId, HashMap<PathBuf, TrackedRepository>>,
    prewarmed_keys: HashSet<InstanceKey>,
    last_prewarm_spawn: Option<LastPrewarmSpawn>,
    last_report: Option<LastPollReport>,
}

#[derive(Clone)]
struct LastPrewarmSpawn {
    at: Instant,
    key: Option<InstanceKey>,
}

#[derive(Clone)]
struct LastPollReport {
    updated_at: SystemTime,
    report: PollReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RepositoryId {
    Remote(String),
    Local(PathBuf),
}

impl RepositoryId {
    fn display(&self) -> String {
        match self {
            RepositoryId::Remote(remote) => remote.clone(),
            RepositoryId::Local(common_dir) => common_dir.display().to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TemplateKey {
    repository_id: RepositoryId,
    relative_workspace_root: PathBuf,
    server: String,
    args: Vec<String>,
    env_template: BTreeMap<String, String>,
}

#[derive(Clone)]
struct PrewarmTemplate {
    key: TemplateKey,
    init_params: InitializeParams,
}

#[derive(Clone)]
struct TrackedRepository {
    root: PathBuf,
    common_dir: PathBuf,
}

#[derive(Debug)]
struct Repository {
    root: PathBuf,
    common_dir: PathBuf,
    id: RepositoryId,
    workspace_prefix: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Worktree {
    path: PathBuf,
    prunable: bool,
    last_changed: Option<SystemTime>,
}

#[derive(Clone)]
struct PrewarmCandidate {
    template: PrewarmTemplate,
    repository: TrackedRepository,
    worktree: Worktree,
}

#[derive(Clone, Default)]
struct PollReport {
    repositories: usize,
    repository_identities: usize,
    templates: usize,
    observed_worktrees: usize,
    prunable_worktrees: usize,
    desired_workspaces: usize,
    missing_workspaces: usize,
    running: usize,
    connected: usize,
    prewarmed: usize,
    queued: usize,
    stale_closed: usize,
    stale_retained: usize,
    processes: Vec<ProcessReport>,
}

#[derive(Clone)]
struct ProcessReport {
    repository_id: String,
    git_root: PathBuf,
    git_common_dir: PathBuf,
    worktree: PathBuf,
    workspace_root: PathBuf,
    worktree_last_changed: Option<SystemTime>,
    target_dir: Option<PathBuf>,
    server: String,
    status: &'static str,
    pid: Option<u32>,
    clients: Option<usize>,
    idle_for: Option<i64>,
    server_ready: Option<bool>,
}

impl ProcessReport {
    fn snapshot(&self) -> PrewarmProcessSnapshot {
        PrewarmProcessSnapshot {
            repository_id: self.repository_id.clone(),
            worktree: self.worktree.clone(),
            workspace_root: self.workspace_root.clone(),
            worktree_last_changed: self.worktree_last_changed,
            target_dir: self.target_dir.clone(),
            server: self.server.clone(),
            status: self.status.to_owned(),
            pid: self.pid,
            clients: self.clients,
            idle_for: self.idle_for,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrewarmSnapshot {
    pub updated_at: Option<SystemTime>,
    pub repositories: usize,
    pub repository_identities: usize,
    pub templates: usize,
    pub worktrees: Vec<PrewarmProcessSnapshot>,
}

#[derive(Clone, Debug)]
pub struct PrewarmProcessSnapshot {
    pub repository_id: String,
    pub worktree: PathBuf,
    pub workspace_root: PathBuf,
    pub worktree_last_changed: Option<SystemTime>,
    pub target_dir: Option<PathBuf>,
    pub server: String,
    pub status: String,
    pub pid: Option<u32>,
    pub clients: Option<usize>,
    pub idle_for: Option<i64>,
}

impl PrewarmTracker {
    pub fn new(poll_interval_seconds: u32) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::default(),
            poll_interval: Duration::from_secs(poll_interval_seconds.into()),
        })
    }

    pub fn start(self: &Arc<Self>, instance_map: Arc<Mutex<InstanceMap>>) {
        let tracker = self.clone();
        task::spawn(
            async move {
                tracker.poll_task(instance_map).await;
            }
            .instrument(info_span!("git worktree prewarm")),
        );
    }

    pub async fn snapshot(&self) -> PrewarmSnapshot {
        let state = self.state.lock().await;
        let Some(last_report) = &state.last_report else {
            return PrewarmSnapshot {
                updated_at: None,
                repositories: state.repositories.values().map(HashMap::len).sum(),
                repository_identities: state.repositories.len(),
                templates: state.templates.len(),
                worktrees: Vec::new(),
            };
        };

        PrewarmSnapshot {
            updated_at: Some(last_report.updated_at),
            repositories: last_report.report.repositories,
            repository_identities: last_report.report.repository_identities,
            templates: last_report.report.templates,
            worktrees: last_report
                .report
                .processes
                .iter()
                .map(ProcessReport::snapshot)
                .collect(),
        }
    }

    pub async fn record_configured_repository(
        &self,
        repository: &PrewarmRepository,
        config: &Config,
    ) -> Result<()> {
        let workspace_root = tokio::fs::canonicalize(&repository.path)
            .await
            .context("resolve workspace path")?;
        let repo = discover_repository(&workspace_root)
            .await?
            .context("workspace path is not inside a git worktree")?;
        let workspace_root_path = path_to_string(&workspace_root)?;
        let init_params = InitializeParams {
            process_id: None,
            client_info: None,
            locale: None,
            root_path: Some(workspace_root_path),
            root_uri: Some(file_uri(&workspace_root)?),
            initialization_options: Some(InitializationOptions {
                lsp_mux: None,
                other_options: repository.initialization_options.clone(),
            }),
            capabilities: Some(serde_json::json!({})),
            trace: None,
            workspace_folders: None,
        };
        let env = std::env::vars()
            .filter(|(key, _)| {
                config
                    .pass_environment
                    .iter()
                    .any(|filter| !filter.negative && filter.pattern.matches(key))
                    && !config
                        .pass_environment
                        .iter()
                        .any(|filter| filter.negative && filter.pattern.matches(key))
            })
            .collect();
        self.record_template(
            repo,
            workspace_root,
            repository.server.clone(),
            repository.args.clone(),
            env,
            init_params,
        )
        .await;
        Ok(())
    }

    #[instrument(skip_all, fields(workspace_root = %workspace_root))]
    pub async fn record_client(
        &self,
        server: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        workspace_root: String,
        init_params: InitializeParams,
    ) -> Result<()> {
        let workspace_root = PathBuf::from(workspace_root);
        let Some(repo) = discover_repository(&workspace_root).await? else {
            debug!("workspace is not inside a git repository");
            return Ok(());
        };

        self.record_template(repo, workspace_root, server, args, env, init_params)
            .await;
        Ok(())
    }

    async fn record_template(
        &self,
        repo: Repository,
        workspace_root: PathBuf,
        server: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        init_params: InitializeParams,
    ) {
        let env_template = normalize_env(&env, &repo.root, &workspace_root);
        let key = TemplateKey {
            repository_id: repo.id.clone(),
            relative_workspace_root: repo.workspace_prefix,
            server,
            args,
            env_template,
        };
        let template = PrewarmTemplate {
            key: key.clone(),
            init_params,
        };

        let mut state = self.state.lock().await;
        let inserted = state.templates.insert(key, template).is_none();
        state.repositories.entry(repo.id).or_default().insert(
            repo.common_dir.clone(),
            TrackedRepository {
                root: repo.root,
                common_dir: repo.common_dir,
            },
        );
        if inserted {
            info!("recorded git worktree prewarm template");
        } else {
            debug!("updated git worktree prewarm template");
        }
    }

    async fn poll_task(&self, instance_map: Arc<Mutex<InstanceMap>>) {
        let mut interval = tokio::time::interval(self.poll_interval);
        loop {
            interval.tick().await;

            if let Err(err) = self.poll_once(instance_map.clone()).await {
                warn!(?err, "git worktree prewarm poll failed");
            }
        }
    }

    async fn poll_once(&self, instance_map: Arc<Mutex<InstanceMap>>) -> Result<()> {
        let (templates, repositories, previous_prewarmed_keys, last_prewarm_spawn) = {
            let state = self.state.lock().await;
            (
                state.templates.values().cloned().collect::<Vec<_>>(),
                state.repositories.clone(),
                state.prewarmed_keys.clone(),
                state.last_prewarm_spawn.clone(),
            )
        };
        let spawn_available =
            prewarm_spawn_available(instance_map.clone(), last_prewarm_spawn.as_ref()).await;

        let mut templates_by_repo = HashMap::<RepositoryId, Vec<PrewarmTemplate>>::new();
        for template in templates {
            templates_by_repo
                .entry(template.key.repository_id.clone())
                .or_default()
                .push(template);
        }

        let mut report = PollReport {
            repositories: repositories.values().map(HashMap::len).sum(),
            repository_identities: repositories.len(),
            templates: templates_by_repo.values().map(Vec::len).sum(),
            ..PollReport::default()
        };
        let mut desired_keys = HashSet::new();
        let mut managed_keys = HashSet::new();
        let mut worktrees_by_repo = HashMap::<PathBuf, Vec<Worktree>>::new();
        let mut candidates = Vec::new();

        for (repository_id, repositories) in repositories {
            let templates = templates_by_repo
                .get(&repository_id)
                .map(Vec::as_slice)
                .unwrap_or_default();

            for repository in repositories.values() {
                if !worktrees_by_repo.contains_key(&repository.common_dir) {
                    match list_worktrees(&repository.common_dir).await {
                        Ok(worktrees) => {
                            report.observed_worktrees += worktrees.len();
                            report.prunable_worktrees += worktrees
                                .iter()
                                .filter(|worktree| worktree.prunable)
                                .count();
                            worktrees_by_repo.insert(repository.common_dir.clone(), worktrees);
                        }
                        Err(err) => {
                            warn!(
                                git_common_dir = ?repository.common_dir,
                                ?err,
                                "failed to list git worktrees",
                            );
                            continue;
                        }
                    }
                }

                let worktrees = worktrees_by_repo
                    .get(&repository.common_dir)
                    .expect("worktrees inserted above");

                for template in templates {
                    for worktree in worktrees.iter().filter(|worktree| !worktree.prunable) {
                        candidates.push(PrewarmCandidate {
                            template: template.clone(),
                            repository: repository.clone(),
                            worktree: worktree.clone(),
                        });
                    }
                }
            }
        }
        sort_candidates_by_recency(&mut candidates);

        let mut spawn_budget = usize::from(spawn_available);
        let mut spawn_attempted = false;
        let mut spawned_key = None;
        for candidate in candidates {
            process_candidate(
                candidate,
                &mut report,
                &mut desired_keys,
                &mut managed_keys,
                instance_map.clone(),
                &previous_prewarmed_keys,
                &mut spawn_budget,
                &mut spawn_attempted,
                &mut spawned_key,
            )
            .await;
        }

        let mut retained_stale_keys = HashSet::new();
        for key in previous_prewarmed_keys.difference(&desired_keys) {
            if !close_if_idle(instance_map.clone(), key).await {
                report.stale_retained += 1;
                retained_stale_keys.insert(key.clone());
            } else {
                report.stale_closed += 1;
            }
        }

        let mut state = self.state.lock().await;
        state
            .prewarmed_keys
            .retain(|key| desired_keys.contains(key) || retained_stale_keys.contains(key));
        state.prewarmed_keys.extend(managed_keys);
        if spawn_attempted {
            state.last_prewarm_spawn = Some(LastPrewarmSpawn {
                at: Instant::now(),
                key: spawned_key,
            });
        }
        state.last_report = Some(LastPollReport {
            updated_at: SystemTime::now(),
            report: report.clone(),
        });

        log_report(&report);

        Ok(())
    }
}

async fn process_candidate(
    candidate: PrewarmCandidate,
    report: &mut PollReport,
    desired_keys: &mut HashSet<InstanceKey>,
    managed_keys: &mut HashSet<InstanceKey>,
    instance_map: Arc<Mutex<InstanceMap>>,
    previous_prewarmed_keys: &HashSet<InstanceKey>,
    spawn_budget: &mut usize,
    spawn_attempted: &mut bool,
    spawned_key: &mut Option<InstanceKey>,
) {
    let PrewarmCandidate {
        template,
        repository,
        worktree,
    } = candidate;

    let workspace_root =
        workspace_root_for_worktree(&worktree.path, &template.key.relative_workspace_root);
    if !is_directory(&workspace_root).await {
        report.missing_workspaces += 1;
        report.processes.push(ProcessReport {
            repository_id: template.key.repository_id.display(),
            git_root: repository.root.clone(),
            git_common_dir: repository.common_dir.clone(),
            worktree: worktree.path.clone(),
            workspace_root: workspace_root.clone(),
            worktree_last_changed: worktree.last_changed,
            target_dir: None,
            server: template.key.server.clone(),
            status: "workspace-missing",
            pid: None,
            clients: None,
            idle_for: None,
            server_ready: None,
        });
        debug!(?workspace_root, "skipping missing worktree workspace");
        return;
    }
    report.desired_workspaces += 1;

    let (key, init_params) = match build_instance(&template, &worktree.path, &workspace_root).await
    {
        Ok(instance) => instance,
        Err(err) => {
            report.processes.push(ProcessReport {
                repository_id: template.key.repository_id.display(),
                git_root: repository.root.clone(),
                git_common_dir: repository.common_dir.clone(),
                worktree: worktree.path.clone(),
                workspace_root: workspace_root.clone(),
                worktree_last_changed: worktree.last_changed,
                target_dir: None,
                server: template.key.server.clone(),
                status: "error",
                pid: None,
                clients: None,
                idle_for: None,
                server_ready: None,
            });
            warn!(
                ?workspace_root,
                ?err,
                "failed to build prewarm instance key"
            );
            return;
        }
    };

    desired_keys.insert(key.clone());
    let target_dir = instance::expected_rust_analyzer_target_dir(instance_map.clone(), &key).await;

    let existing = instance_map.lock().await.get(&key);
    if let Some(instance) = existing {
        if previous_prewarmed_keys.contains(&key) {
            instance.keep_alive();
        }
        push_instance_report(
            report,
            &template,
            &repository,
            &worktree.path,
            workspace_root,
            worktree.last_changed,
            instance,
        )
        .await;
        managed_keys.insert(key);
        return;
    }

    if *spawn_budget == 0 {
        report.queued += 1;
        report.processes.push(ProcessReport {
            repository_id: template.key.repository_id.display(),
            git_root: repository.root.clone(),
            git_common_dir: repository.common_dir.clone(),
            worktree: worktree.path.clone(),
            workspace_root,
            worktree_last_changed: worktree.last_changed,
            target_dir,
            server: template.key.server.clone(),
            status: "queued",
            pid: None,
            clients: None,
            idle_for: None,
            server_ready: None,
        });
        return;
    }

    *spawn_budget -= 1;
    *spawn_attempted = true;

    match instance::prewarm(instance_map.clone(), key.clone(), init_params).await {
        Ok(Some(_instance)) => {
            info!(?workspace_root, "prewarmed git worktree language server");
            *spawned_key = Some(key.clone());
            if let Some(instance) = instance_map.lock().await.get(&key) {
                push_instance_report(
                    report,
                    &template,
                    &repository,
                    &worktree.path,
                    workspace_root,
                    worktree.last_changed,
                    instance,
                )
                .await;
            }
            managed_keys.insert(key);
        }
        Ok(None) => {
            if let Some(instance) = instance_map.lock().await.get(&key) {
                push_instance_report(
                    report,
                    &template,
                    &repository,
                    &worktree.path,
                    workspace_root,
                    worktree.last_changed,
                    instance,
                )
                .await;
            }
            managed_keys.insert(key);
        }
        Err(err) => {
            report.processes.push(ProcessReport {
                repository_id: template.key.repository_id.display(),
                git_root: repository.root.clone(),
                git_common_dir: repository.common_dir.clone(),
                worktree: worktree.path.clone(),
                workspace_root: workspace_root.clone(),
                worktree_last_changed: worktree.last_changed,
                target_dir,
                server: template.key.server.clone(),
                status: "spawn-error",
                pid: None,
                clients: None,
                idle_for: None,
                server_ready: None,
            });
            warn!(?workspace_root, ?err, "failed to prewarm language server");
        }
    }
}

async fn push_instance_report(
    report: &mut PollReport,
    template: &PrewarmTemplate,
    repository: &TrackedRepository,
    worktree: &Path,
    workspace_root: PathBuf,
    worktree_last_changed: Option<SystemTime>,
    instance: Arc<instance::Instance>,
) {
    let snapshot = instance.snapshot().await;
    let status = if !snapshot.server_ready {
        "initializing"
    } else if snapshot.clients == 0 {
        "prewarmed"
    } else {
        "connected"
    };

    report.running += 1;
    if snapshot.clients == 0 {
        report.prewarmed += 1;
    } else {
        report.connected += 1;
    }
    report.processes.push(ProcessReport {
        repository_id: template.key.repository_id.display(),
        git_root: repository.root.clone(),
        git_common_dir: repository.common_dir.clone(),
        worktree: worktree.to_owned(),
        workspace_root,
        worktree_last_changed,
        target_dir: snapshot.rust_analyzer_target_dir,
        server: template.key.server.clone(),
        status,
        pid: Some(snapshot.pid),
        clients: Some(snapshot.clients),
        idle_for: Some(snapshot.idle_for),
        server_ready: Some(snapshot.server_ready),
    });
}

fn log_report(report: &PollReport) {
    info!(
        repositories = report.repositories,
        repository_identities = report.repository_identities,
        templates = report.templates,
        worktrees = report.observed_worktrees,
        prunable_worktrees = report.prunable_worktrees,
        desired_workspaces = report.desired_workspaces,
        missing_workspaces = report.missing_workspaces,
        running_processes = report.running,
        connected_processes = report.connected,
        prewarmed_processes = report.prewarmed,
        queued_processes = report.queued,
        stale_closed = report.stale_closed,
        stale_retained = report.stale_retained,
        "git worktree prewarm status",
    );

    for process in &report.processes {
        info!(
            repository_id = %process.repository_id,
            git_root = ?process.git_root,
            git_common_dir = ?process.git_common_dir,
            worktree = ?process.worktree,
            workspace_root = ?process.workspace_root,
            worktree_last_changed = ?process.worktree_last_changed,
            target_dir = ?process.target_dir,
            server = ?process.server,
            status = process.status,
            pid = ?process.pid,
            clients = ?process.clients,
            idle_for = ?process.idle_for,
            server_ready = ?process.server_ready,
            "git worktree language server status",
        );
    }
}

async fn close_if_idle(instance_map: Arc<Mutex<InstanceMap>>, key: &InstanceKey) -> bool {
    let instance = instance_map.lock().await.get(key);
    let Some(instance) = instance else {
        return true;
    };

    if instance.client_count().await == 0 {
        info!(?key, "closing stale prewarmed language server");
        instance.request_close();
        true
    } else {
        false
    }
}

async fn prewarm_spawn_available(
    instance_map: Arc<Mutex<InstanceMap>>,
    last_spawn: Option<&LastPrewarmSpawn>,
) -> bool {
    let Some(last_spawn) = last_spawn else {
        return true;
    };

    if last_spawn.at.elapsed() >= PREWARM_SPAWN_INTERVAL {
        return true;
    }

    let Some(key) = &last_spawn.key else {
        return false;
    };

    instance_map
        .lock()
        .await
        .get(key)
        .map(|instance| instance.server_ready())
        .unwrap_or(true)
}

async fn build_instance(
    template: &PrewarmTemplate,
    git_root: &Path,
    workspace_root: &Path,
) -> Result<(InstanceKey, InitializeParams)> {
    let init_params = retarget_initialize_params(&template.init_params, workspace_root)
        .context("retarget initialize params")?;
    let workspace_root_string = path_to_string(workspace_root)?;
    let workspace_root =
        task::spawn_blocking(move || WorkspaceRoot::from_path(workspace_root_string))
            .await
            .expect("blocking task panicked")
            .context("get workspace_root metadata")?;

    let key = InstanceKey {
        server: template.key.server.clone(),
        args: template.key.args.clone(),
        env: materialize_env(&template.key.env_template, git_root, workspace_root.path()),
        workspace_root,
    };

    Ok((key, init_params))
}

async fn discover_repository(workspace_root: &Path) -> Result<Option<Repository>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "rev-parse",
            "--is-inside-work-tree",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
            "--show-prefix",
        ])
        .output()
        .await
        .context("running git rev-parse")?;

    if !output.status.success() {
        debug!(
            status = ?output.status,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "git rev-parse failed",
        );
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout).context("git rev-parse output was not utf-8")?;
    let mut lines = stdout.lines();
    if lines.next() != Some("true") {
        return Ok(None);
    }

    let root = lines
        .next()
        .context("git rev-parse output missing repository root")?;
    let common_dir = lines
        .next()
        .context("git rev-parse output missing common git directory")?;
    let prefix = lines
        .next()
        .context("git rev-parse output missing workspace prefix")?;
    let common_dir = PathBuf::from(common_dir);
    let remote_url = discover_remote_url(workspace_root).await?;

    Ok(Some(Repository {
        root: PathBuf::from(root),
        common_dir: common_dir.clone(),
        id: repository_id(remote_url, &common_dir),
        workspace_prefix: prefix_to_relative_path(prefix),
    }))
}

async fn discover_remote_url(workspace_root: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["remote", "get-url", "origin"])
        .output()
        .await
        .context("running git remote get-url origin")?;

    if output.status.success() {
        let stdout =
            String::from_utf8(output.stdout).context("git remote get-url output was not utf-8")?;
        let url = stdout.trim();
        if !url.is_empty() {
            return Ok(Some(url.to_owned()));
        }
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["remote", "-v"])
        .output()
        .await
        .context("running git remote -v")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout).context("git remote -v output was not utf-8")?;
    Ok(first_fetch_remote_url(&stdout))
}

fn repository_id(remote_url: Option<String>, common_dir: &Path) -> RepositoryId {
    remote_url.map_or_else(
        || RepositoryId::Local(common_dir.to_owned()),
        RepositoryId::Remote,
    )
}

fn first_fetch_remote_url(output: &str) -> Option<String> {
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let Some(_name) = fields.next() else {
            continue;
        };
        let Some(url) = fields.next() else {
            continue;
        };
        if fields.next() == Some("(fetch)") {
            return Some(url.to_owned());
        }
    }

    None
}

async fn list_worktrees(git_common_dir: &Path) -> Result<Vec<Worktree>> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_common_dir)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .await
        .with_context(|| format!("running git worktree list for {git_common_dir:?}"))?;

    if !output.status.success() {
        bail!(
            "git worktree list failed: status={status:?}, stderr={stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr),
        );
    }

    let mut worktrees = parse_worktree_list(&output.stdout)?;
    for worktree in &mut worktrees {
        worktree.last_changed = worktree_last_changed(&worktree.path).await;
    }
    sort_worktrees_by_recency(&mut worktrees);

    Ok(worktrees)
}

fn parse_worktree_list(output: &[u8]) -> Result<Vec<Worktree>> {
    let mut worktrees = Vec::new();
    let mut current = None::<Worktree>;

    for field in output.split(|byte| *byte == b'\0') {
        if field.is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            continue;
        }

        let field = std::str::from_utf8(field).context("git worktree output was not utf-8")?;
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            current = Some(Worktree {
                path: PathBuf::from(path),
                prunable: false,
                last_changed: None,
            });
            continue;
        }

        if field.starts_with("prunable") {
            if let Some(worktree) = current.as_mut() {
                worktree.prunable = true;
            }
        }
    }

    if let Some(worktree) = current {
        worktrees.push(worktree);
    }

    Ok(worktrees)
}

async fn worktree_last_changed(worktree_path: &Path) -> Option<SystemTime> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--modified",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .await
        .ok()?;

    let root = worktree_path.to_owned();
    let paths = if output.status.success() {
        parse_git_ls_files_z(&output.stdout).unwrap_or_default()
    } else {
        Vec::new()
    };

    task::spawn_blocking(move || newest_mtime_for_paths(&root, &paths))
        .await
        .ok()
        .flatten()
}

fn parse_git_ls_files_z(output: &[u8]) -> Result<Vec<PathBuf>> {
    output
        .split(|byte| *byte == b'\0')
        .filter(|field| !field.is_empty())
        .map(|field| {
            let path = std::str::from_utf8(field).context("git ls-files output was not utf-8")?;
            Ok(PathBuf::from(path))
        })
        .collect()
}

fn newest_mtime_for_paths(root: &Path, relative_paths: &[PathBuf]) -> Option<SystemTime> {
    let mut newest = modified_time(root);
    for relative_path in relative_paths {
        let path = root.join(relative_path);
        if let Some(modified) = modified_time(&path) {
            newest = max_system_time(newest, modified);
        }
    }
    newest
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn max_system_time(current: Option<SystemTime>, candidate: SystemTime) -> Option<SystemTime> {
    match current {
        Some(current) => Some(current.max(candidate)),
        None => Some(candidate),
    }
}

fn sort_worktrees_by_recency(worktrees: &mut [Worktree]) {
    worktrees.sort_by(|left, right| {
        right
            .last_changed
            .cmp(&left.last_changed)
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn sort_candidates_by_recency(candidates: &mut [PrewarmCandidate]) {
    candidates.sort_by(|left, right| {
        right
            .worktree
            .last_changed
            .cmp(&left.worktree.last_changed)
            .then_with(|| left.worktree.path.cmp(&right.worktree.path))
            .then_with(|| {
                left.template
                    .key
                    .relative_workspace_root
                    .cmp(&right.template.key.relative_workspace_root)
            })
            .then_with(|| left.template.key.server.cmp(&right.template.key.server))
    });
}

fn prefix_to_relative_path(prefix: &str) -> PathBuf {
    PathBuf::from(prefix.trim_end_matches('/'))
}

fn workspace_root_for_worktree(worktree_path: &Path, relative_workspace_root: &Path) -> PathBuf {
    worktree_path.join(relative_workspace_root)
}

fn normalize_env(
    env: &BTreeMap<String, String>,
    git_root: &Path,
    workspace_root: &Path,
) -> BTreeMap<String, String> {
    let mut env = env.clone();
    let git_root = git_root.to_str();
    let workspace_root = workspace_root.to_str();

    for value in env.values_mut() {
        if Some(value.as_str()) == workspace_root {
            *value = WORKSPACE_ROOT_ENV.to_owned();
        } else if Some(value.as_str()) == git_root {
            *value = GIT_ROOT_ENV.to_owned();
        }
    }

    env
}

fn materialize_env(
    env_template: &BTreeMap<String, String>,
    git_root: &Path,
    workspace_root: &str,
) -> BTreeMap<String, String> {
    let mut env = env_template.clone();
    let git_root = git_root.to_str();

    for value in env.values_mut() {
        match value.as_str() {
            WORKSPACE_ROOT_ENV => *value = workspace_root.to_owned(),
            GIT_ROOT_ENV => {
                if let Some(git_root) = git_root {
                    *value = git_root.to_owned();
                }
            }
            _ => {}
        }
    }

    env
}

fn retarget_initialize_params(
    init_params: &InitializeParams,
    workspace_root: &Path,
) -> Result<InitializeParams> {
    let workspace_root_path = path_to_string(workspace_root)?;
    let workspace_root_uri = file_uri(workspace_root)?;
    let workspace_root_name = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&workspace_root_path)
        .to_owned();

    let mut init_params = init_params.clone();
    if init_params.root_uri.is_some() {
        init_params.root_uri = Some(workspace_root_uri.clone());
    }
    if init_params.root_path.is_some() {
        init_params.root_path = Some(workspace_root_path);
    }
    if let Some(workspace_folders) = init_params.workspace_folders.as_mut() {
        if let [workspace_folder] = workspace_folders.as_mut_slice() {
            *workspace_folder = WorkspaceFolder {
                uri: workspace_root_uri,
                name: workspace_root_name,
            };
        }
    }

    Ok(init_params)
}

fn file_uri(path: &Path) -> Result<String> {
    let path = path_to_string(path)?.replace('\\', "/");
    let prefix = if cfg!(windows) && path.as_bytes().get(1) == Some(&b':') {
        "file:///"
    } else {
        "file://"
    };
    Ok(format!(
        "{prefix}{}",
        utf8_percent_encode(&path, URI_PATH_ENCODE_SET)
    ))
}

fn path_to_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("path is not valid utf-8: {path:?}"))
}

async fn is_directory(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::lsp::{InitializationOptions, TraceValue};

    use super::*;

    struct TestRepository(PathBuf);

    impl TestRepository {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "lsp-herd-prewarm-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn git(&self, args: &[&str]) {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn configured_repository_discovers_worktrees_without_a_client() {
        let repo = TestRepository::new();
        repo.git(&["init"]);
        repo.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        repo.git(&["worktree", "add", "--detach", "other"]);
        fs::create_dir(repo.0.join("rust")).unwrap();
        fs::create_dir(repo.0.join("other/rust")).unwrap();

        let config = Config {
            pass_environment: vec![],
            ..Config::default()
        };
        let repository = PrewarmRepository {
            path: repo.0.join("rust"),
            server: "custom-rust-analyzer".into(),
            args: vec!["--quiet".into()],
            initialization_options: serde_json::Map::from_iter([(
                "check".into(),
                json!({ "command": "clippy" }),
            )]),
        };
        let tracker = PrewarmTracker::new(30);
        tracker
            .record_configured_repository(&repository, &config)
            .await
            .unwrap();
        let (template, common_dir) = {
            let state = tracker.state.lock().await;
            assert_eq!(state.templates.len(), 1);
            assert_eq!(state.repositories.len(), 1);
            let template = state.templates.values().next().unwrap().clone();
            assert_eq!(template.key.relative_workspace_root, Path::new("rust"));
            let common_dir = state
                .repositories
                .values()
                .next()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .common_dir
                .clone();
            (template, common_dir)
        };
        let worktrees = list_worktrees(&common_dir).await.unwrap();
        assert_eq!(worktrees.len(), 2);
        for worktree in worktrees {
            let workspace_root = worktree.path.join("rust");
            let (key, params) = build_instance(&template, &worktree.path, &workspace_root)
                .await
                .unwrap();
            assert_eq!(key.server, "custom-rust-analyzer");
            assert_eq!(key.args, ["--quiet"]);
            assert!(key.env.is_empty());
            assert_eq!(
                key.workspace_root.path(),
                path_to_string(&workspace_root).unwrap()
            );
            assert_eq!(params.root_uri.unwrap(), file_uri(&workspace_root).unwrap());
            assert_eq!(
                params.initialization_options.unwrap().other_options["check"]["command"],
                "clippy"
            );
        }
    }

    #[tokio::test]
    async fn configured_repository_rejects_invalid_paths() {
        let repo = TestRepository::new();
        let mut repository = PrewarmRepository {
            path: repo.0.clone(),
            server: "rust-analyzer".into(),
            args: vec![],
            initialization_options: serde_json::Map::new(),
        };
        let tracker = PrewarmTracker::new(30);
        let err = tracker
            .record_configured_repository(&repository, &Config::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not inside a git worktree"));
        repository.path = repo.0.join("missing");
        let err = tracker
            .record_configured_repository(&repository, &Config::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("resolve workspace path"));
    }

    fn init_params() -> InitializeParams {
        InitializeParams {
            process_id: Some(123),
            client_info: None,
            locale: None,
            root_path: Some("/repo/rust".into()),
            root_uri: Some("file:///repo/rust".into()),
            initialization_options: Some(InitializationOptions {
                lsp_mux: None,
                other_options: serde_json::Map::from_iter([("check".into(), json!(true))]),
            }),
            capabilities: Some(json!({ "workspace": { "configuration": true } })),
            trace: Some(TraceValue::Off),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: "file:///repo/rust".into(),
                name: "rust".into(),
            }]),
        }
    }

    #[test]
    fn parses_porcelain_z_worktrees() {
        let worktrees = parse_worktree_list(
            b"worktree /repo\0HEAD abc\0branch refs/heads/main\0\0\
              worktree /repo-wt\0HEAD def\0detached\0\0\
              worktree /missing\0HEAD ghi\0prunable gitdir file points to non-existent location\0\0",
        )
        .unwrap();

        assert_eq!(
            worktrees,
            vec![
                Worktree {
                    path: PathBuf::from("/repo"),
                    prunable: false,
                    last_changed: None,
                },
                Worktree {
                    path: PathBuf::from("/repo-wt"),
                    prunable: false,
                    last_changed: None,
                },
                Worktree {
                    path: PathBuf::from("/missing"),
                    prunable: true,
                    last_changed: None,
                },
            ]
        );
    }

    #[test]
    fn parses_git_ls_files_z_output() {
        assert_eq!(
            parse_git_ls_files_z(b"src/main.rs\0Cargo.toml\0").unwrap(),
            vec![PathBuf::from("src/main.rs"), PathBuf::from("Cargo.toml")]
        );
    }

    #[test]
    fn sorts_worktrees_by_recent_activity() {
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
        let mut worktrees = vec![
            Worktree {
                path: PathBuf::from("/old"),
                prunable: false,
                last_changed: Some(older),
            },
            Worktree {
                path: PathBuf::from("/unknown"),
                prunable: false,
                last_changed: None,
            },
            Worktree {
                path: PathBuf::from("/new"),
                prunable: false,
                last_changed: Some(newer),
            },
        ];

        sort_worktrees_by_recency(&mut worktrees);

        assert_eq!(
            worktrees
                .iter()
                .map(|worktree| worktree.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("/new"), Path::new("/old"), Path::new("/unknown")]
        );
    }

    #[test]
    fn converts_git_prefix_to_relative_path() {
        assert_eq!(prefix_to_relative_path(""), PathBuf::new());
        assert_eq!(prefix_to_relative_path("rust/"), PathBuf::from("rust"));
        assert_eq!(
            prefix_to_relative_path("crates/server/"),
            PathBuf::from("crates/server")
        );
    }

    #[test]
    fn parses_first_fetch_remote_url() {
        assert_eq!(
            first_fetch_remote_url(
                "origin\tgit@github.com:slint-ui/slint.git (fetch)\n\
                 origin\tgit@github.com:slint-ui/slint.git (push)\n",
            )
            .as_deref(),
            Some("git@github.com:slint-ui/slint.git")
        );
        assert_eq!(
            first_fetch_remote_url("origin\tgit@github.com:slint-ui/slint.git (push)\n"),
            None
        );
    }

    #[test]
    fn repository_id_prefers_remote_and_falls_back_to_local_common_dir() {
        assert_eq!(
            repository_id(
                Some("git@github.com:slint-ui/slint.git".into()),
                Path::new("/repo/.git"),
            ),
            RepositoryId::Remote("git@github.com:slint-ui/slint.git".into())
        );
        assert_eq!(
            repository_id(None, Path::new("/repo/.git")),
            RepositoryId::Local(PathBuf::from("/repo/.git"))
        );
    }

    #[test]
    fn expands_worktree_workspace_root() {
        assert_eq!(
            workspace_root_for_worktree(Path::new("/worktree"), Path::new("rust")),
            PathBuf::from("/worktree/rust")
        );
        assert_eq!(
            workspace_root_for_worktree(Path::new("/worktree"), Path::new("crates/server")),
            PathBuf::from("/worktree/crates/server")
        );
        assert_eq!(
            workspace_root_for_worktree(Path::new("/worktree"), Path::new("")),
            PathBuf::from("/worktree/")
        );
    }

    #[test]
    fn retargets_workspace_locations() {
        let params = retarget_initialize_params(&init_params(), Path::new("/worktree/rust"))
            .expect("retarget initialize params");

        assert_eq!(params.root_path.as_deref(), Some("/worktree/rust"));
        assert_eq!(params.root_uri.as_deref(), Some("file:///worktree/rust"));
        assert_eq!(
            params.workspace_folders.as_ref().unwrap(),
            &[WorkspaceFolder {
                uri: "file:///worktree/rust".into(),
                name: "rust".into(),
            }]
        );
        assert_eq!(
            params.capabilities,
            Some(json!({ "workspace": { "configuration": true } }))
        );
    }

    #[test]
    fn encodes_file_uri_paths() {
        assert_eq!(
            file_uri(Path::new("/repo/rust workspace")).unwrap(),
            "file:///repo/rust%20workspace"
        );
        assert_eq!(
            file_uri(Path::new("/repo/rust#main")).unwrap(),
            "file:///repo/rust%23main"
        );
    }

    #[test]
    fn normalizes_and_materializes_pwd() {
        let env = BTreeMap::from_iter([
            ("PATH".into(), "/bin".into()),
            ("PWD".into(), "/repo/rust".into()),
        ]);
        let env_template = normalize_env(&env, Path::new("/repo"), Path::new("/repo/rust"));
        assert_eq!(
            env_template.get("PWD").map(String::as_str),
            Some(WORKSPACE_ROOT_ENV)
        );

        let env = materialize_env(&env_template, Path::new("/repo-wt"), "/repo-wt/rust");
        assert_eq!(env.get("PWD").map(String::as_str), Some("/repo-wt/rust"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
    }
}
