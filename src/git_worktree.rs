use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task;
use tracing::{debug, info, info_span, instrument, warn, Instrument};

use crate::instance::{self, InstanceKey, InstanceMap, WorkspaceRoot};
use crate::lsp::{InitializeParams, WorkspaceFolder};

const WORKSPACE_ROOT_ENV: &str = "${LSPMUX_WORKSPACE_ROOT}";
const GIT_ROOT_ENV: &str = "${LSPMUX_GIT_ROOT}";

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
    prewarmed_keys: HashSet<InstanceKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TemplateKey {
    git_common_dir: PathBuf,
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

#[derive(Debug)]
struct Repository {
    root: PathBuf,
    common_dir: PathBuf,
    workspace_prefix: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Worktree {
    path: PathBuf,
    prunable: bool,
}

#[derive(Default)]
struct PollReport {
    repos: usize,
    templates: usize,
    observed_worktrees: usize,
    prunable_worktrees: usize,
    desired_workspaces: usize,
    missing_workspaces: usize,
    running: usize,
    connected: usize,
    prewarmed: usize,
    stale_closed: usize,
    stale_retained: usize,
    processes: Vec<ProcessReport>,
}

struct ProcessReport {
    git_common_dir: PathBuf,
    worktree: PathBuf,
    workspace_root: PathBuf,
    server: String,
    status: &'static str,
    pid: Option<u32>,
    clients: Option<usize>,
    idle_for: Option<i64>,
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

        let env_template = normalize_env(&env, &repo.root, &workspace_root);
        let key = TemplateKey {
            git_common_dir: repo.common_dir,
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
        if inserted {
            info!("recorded git worktree prewarm template");
        } else {
            debug!("updated git worktree prewarm template");
        }

        Ok(())
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
        let (templates, previous_prewarmed_keys) = {
            let state = self.state.lock().await;
            (
                state.templates.values().cloned().collect::<Vec<_>>(),
                state.prewarmed_keys.clone(),
            )
        };

        let mut report = PollReport {
            repos: templates
                .iter()
                .map(|template| &template.key.git_common_dir)
                .collect::<HashSet<_>>()
                .len(),
            templates: templates.len(),
            ..PollReport::default()
        };
        let mut desired_keys = HashSet::new();
        let mut managed_keys = HashSet::new();
        let mut worktrees_by_repo = HashMap::<PathBuf, Vec<Worktree>>::new();

        for template in templates {
            if !worktrees_by_repo.contains_key(&template.key.git_common_dir) {
                match list_worktrees(&template.key.git_common_dir).await {
                    Ok(worktrees) => {
                        report.observed_worktrees += worktrees.len();
                        report.prunable_worktrees += worktrees
                            .iter()
                            .filter(|worktree| worktree.prunable)
                            .count();
                        worktrees_by_repo.insert(template.key.git_common_dir.clone(), worktrees);
                    }
                    Err(err) => {
                        warn!(
                            git_common_dir = ?template.key.git_common_dir,
                            ?err,
                            "failed to list git worktrees",
                        );
                        continue;
                    }
                }
            }

            let worktrees = worktrees_by_repo
                .get(&template.key.git_common_dir)
                .expect("worktrees inserted above");

            for worktree in worktrees {
                if worktree.prunable {
                    continue;
                }

                let workspace_root = workspace_root_for_worktree(
                    &worktree.path,
                    &template.key.relative_workspace_root,
                );
                if !is_directory(&workspace_root).await {
                    report.missing_workspaces += 1;
                    report.processes.push(ProcessReport {
                        git_common_dir: template.key.git_common_dir.clone(),
                        worktree: worktree.path.clone(),
                        workspace_root: workspace_root.clone(),
                        server: template.key.server.clone(),
                        status: "workspace-missing",
                        pid: None,
                        clients: None,
                        idle_for: None,
                    });
                    debug!(?workspace_root, "skipping missing worktree workspace");
                    continue;
                }
                report.desired_workspaces += 1;

                let (key, init_params) =
                    match build_instance(&template, &worktree.path, &workspace_root).await {
                        Ok(instance) => instance,
                        Err(err) => {
                            report.processes.push(ProcessReport {
                                git_common_dir: template.key.git_common_dir.clone(),
                                worktree: worktree.path.clone(),
                                workspace_root: workspace_root.clone(),
                                server: template.key.server.clone(),
                                status: "error",
                                pid: None,
                                clients: None,
                                idle_for: None,
                            });
                            warn!(
                                ?workspace_root,
                                ?err,
                                "failed to build prewarm instance key"
                            );
                            continue;
                        }
                    };

                desired_keys.insert(key.clone());

                let existing = instance_map.lock().await.get(&key);
                if let Some(instance) = existing {
                    if previous_prewarmed_keys.contains(&key) {
                        instance.keep_alive();
                    }
                    push_instance_report(
                        &mut report,
                        &template,
                        &worktree.path,
                        workspace_root,
                        instance,
                    )
                    .await;
                    managed_keys.insert(key);
                    continue;
                }

                match instance::prewarm(instance_map.clone(), key.clone(), init_params).await {
                    Ok(Some(_instance)) => {
                        info!(?workspace_root, "prewarmed git worktree language server");
                        if let Some(instance) = instance_map.lock().await.get(&key) {
                            push_instance_report(
                                &mut report,
                                &template,
                                &worktree.path,
                                workspace_root,
                                instance,
                            )
                            .await;
                        }
                        managed_keys.insert(key);
                    }
                    Ok(None) => {
                        if let Some(instance) = instance_map.lock().await.get(&key) {
                            push_instance_report(
                                &mut report,
                                &template,
                                &worktree.path,
                                workspace_root,
                                instance,
                            )
                            .await;
                        }
                        managed_keys.insert(key);
                    }
                    Err(err) => {
                        report.processes.push(ProcessReport {
                            git_common_dir: template.key.git_common_dir.clone(),
                            worktree: worktree.path.clone(),
                            workspace_root: workspace_root.clone(),
                            server: template.key.server.clone(),
                            status: "spawn-error",
                            pid: None,
                            clients: None,
                            idle_for: None,
                        });
                        warn!(?workspace_root, ?err, "failed to prewarm language server");
                    }
                }
            }
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

        log_report(&report);

        Ok(())
    }
}

async fn push_instance_report(
    report: &mut PollReport,
    template: &PrewarmTemplate,
    worktree: &Path,
    workspace_root: PathBuf,
    instance: Arc<instance::Instance>,
) {
    let clients = instance.client_count().await;
    let status = if clients == 0 {
        "prewarmed"
    } else {
        "connected"
    };

    report.running += 1;
    if clients == 0 {
        report.prewarmed += 1;
    } else {
        report.connected += 1;
    }
    report.processes.push(ProcessReport {
        git_common_dir: template.key.git_common_dir.clone(),
        worktree: worktree.to_owned(),
        workspace_root,
        server: template.key.server.clone(),
        status,
        pid: Some(instance.pid()),
        clients: Some(clients),
        idle_for: Some(instance.idle()),
    });
}

fn log_report(report: &PollReport) {
    info!(
        repos = report.repos,
        templates = report.templates,
        worktrees = report.observed_worktrees,
        prunable_worktrees = report.prunable_worktrees,
        desired_workspaces = report.desired_workspaces,
        missing_workspaces = report.missing_workspaces,
        running_processes = report.running,
        connected_processes = report.connected,
        prewarmed_processes = report.prewarmed,
        stale_closed = report.stale_closed,
        stale_retained = report.stale_retained,
        "git worktree prewarm status",
    );

    for process in &report.processes {
        info!(
            git_common_dir = ?process.git_common_dir,
            worktree = ?process.worktree,
            workspace_root = ?process.workspace_root,
            server = ?process.server,
            status = process.status,
            pid = ?process.pid,
            clients = ?process.clients,
            idle_for = ?process.idle_for,
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

    Ok(Some(Repository {
        root: PathBuf::from(root),
        common_dir: PathBuf::from(common_dir),
        workspace_prefix: prefix_to_relative_path(prefix),
    }))
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

    parse_worktree_list(&output.stdout)
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
                },
                Worktree {
                    path: PathBuf::from("/repo-wt"),
                    prunable: false,
                },
                Worktree {
                    path: PathBuf::from("/missing"),
                    prunable: true,
                },
            ]
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
