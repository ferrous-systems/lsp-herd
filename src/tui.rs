use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;
use tokio::task;

use crate::git_worktree::{PrewarmProcessSnapshot, PrewarmSnapshot, PrewarmTracker};
use crate::instance::{InstanceMap, InstanceSnapshot};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub fn start(instance_map: Arc<Mutex<InstanceMap>>, prewarm_tracker: Option<Arc<PrewarmTracker>>) {
    task::spawn(async move {
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        loop {
            interval.tick().await;
            let text = render(instance_map.clone(), prewarm_tracker.clone()).await;
            if redraw(&text).is_err() {
                break;
            }
        }
    });
}

async fn render(
    instance_map: Arc<Mutex<InstanceMap>>,
    prewarm_tracker: Option<Arc<PrewarmTracker>>,
) -> String {
    let prewarm = match prewarm_tracker {
        Some(tracker) => Some(tracker.snapshot().await),
        None => None,
    };
    let instance_snapshots = instance_snapshots(instance_map).await;

    render_status(prewarm, instance_snapshots)
}

async fn instance_snapshots(instance_map: Arc<Mutex<InstanceMap>>) -> Vec<InstanceSnapshot> {
    let instances = instance_map.lock().await.instances();
    let mut snapshots = Vec::with_capacity(instances.len());
    for instance in instances {
        snapshots.push(instance.snapshot().await);
    }
    snapshots
}

fn render_status(prewarm: Option<PrewarmSnapshot>, instances: Vec<InstanceSnapshot>) -> String {
    let mut rows = Vec::new();
    let mut seen_workspaces = HashSet::new();
    let instances_by_workspace = instances
        .iter()
        .map(|instance| (instance.workspace_root.clone(), instance))
        .collect::<HashMap<_, _>>();

    if let Some(prewarm) = &prewarm {
        for process in &prewarm.worktrees {
            let instance = instances_by_workspace.get(&process.workspace_root).copied();
            rows.push(Row::from_process(process, instance));
            seen_workspaces.insert(process.workspace_root.clone());
        }
    }

    for instance in &instances {
        if seen_workspaces.contains(&instance.workspace_root) {
            continue;
        }
        rows.push(Row::from_instance(instance));
    }

    let worktree_paths = rows
        .iter()
        .map(|row| row.worktree.clone())
        .collect::<Vec<_>>();
    let worktree_labels = distinct_path_labels(&worktree_paths);

    let mut by_repository = BTreeMap::<String, BTreeMap<PathBuf, Vec<Row>>>::new();
    for row in rows {
        by_repository
            .entry(row.repository_id.clone())
            .or_default()
            .entry(row.worktree.clone())
            .or_default()
            .push(row);
    }

    let mut output = String::new();
    output.push_str("lspmux status\n");
    output.push_str("=============\n");
    if let Some(prewarm) = &prewarm {
        output.push_str(&format!(
            "git repositories: {} tracked, {} identities; templates: {}; prewarm poll: {}\n",
            prewarm.repositories,
            prewarm.repository_identities,
            prewarm.templates,
            format_updated_at(prewarm.updated_at),
        ));
    } else {
        output.push_str("git worktree prewarm: disabled\n");
    }
    output.push_str("refresh: 1s; press Ctrl-C to stop lspmux\n\n");

    if by_repository.is_empty() {
        output.push_str(
            "No rust-analyzer worktree data yet. Connect an LSP client to seed templates.\n",
        );
        return output;
    }

    for (repository_id, worktrees) in by_repository {
        output.push_str(&format!("Remote: {repository_id}\n"));

        for (worktree, mut rows) in worktrees {
            rows.sort_by(|left, right| left.workspace_root.cmp(&right.workspace_root));
            let label = worktree_labels
                .get(&worktree)
                .cloned()
                .unwrap_or_else(|| path_label(&worktree));
            output.push_str(&format!("  {label} ({})\n", worktree.display()));

            for row in rows {
                let workspace = workspace_label(&worktree, &row.workspace_root);
                let pid = row
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "-".to_owned());
                let clients = row
                    .clients
                    .map(|clients| clients.to_string())
                    .unwrap_or_else(|| "-".to_owned());
                let idle = row
                    .idle_for
                    .map(|idle| format!("{idle}s"))
                    .unwrap_or_else(|| "-".to_owned());
                let target = row
                    .target_dir
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "-".to_owned());
                let changed = row
                    .worktree_last_changed
                    .map(|time| format_updated_at(Some(time)))
                    .unwrap_or_else(|| "unknown".to_owned());
                output.push_str(&format!(
                    "    workspace={workspace} server={} status={} pid={pid} clients={clients} idle={idle} changed={changed}\n",
                    row.server,
                    row.status,
                ));
                output.push_str(&format!("      target={target}\n"));
            }
        }

        output.push('\n');
    }

    output
}

fn redraw(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\x1b[2J\x1b[H")?;
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

#[derive(Clone)]
struct Row {
    repository_id: String,
    worktree: PathBuf,
    workspace_root: PathBuf,
    target_dir: Option<PathBuf>,
    server: String,
    status: String,
    pid: Option<u32>,
    clients: Option<usize>,
    idle_for: Option<i64>,
    worktree_last_changed: Option<SystemTime>,
}

impl Row {
    fn from_process(process: &PrewarmProcessSnapshot, instance: Option<&InstanceSnapshot>) -> Self {
        let target_dir = instance
            .and_then(|instance| instance.rust_analyzer_target_dir.clone())
            .or_else(|| process.target_dir.clone());
        let status = instance
            .map(status_for_instance)
            .unwrap_or_else(|| status_for_process(process));

        Self {
            repository_id: process.repository_id.clone(),
            worktree: process.worktree.clone(),
            workspace_root: process.workspace_root.clone(),
            target_dir,
            server: process.server.clone(),
            status,
            pid: instance.map(|instance| instance.pid).or(process.pid),
            clients: instance
                .map(|instance| instance.clients)
                .or(process.clients),
            idle_for: instance
                .map(|instance| instance.idle_for)
                .or(process.idle_for),
            worktree_last_changed: process.worktree_last_changed,
        }
    }

    fn from_instance(instance: &InstanceSnapshot) -> Self {
        Self {
            repository_id: "(untracked)".to_owned(),
            worktree: instance.workspace_root.clone(),
            workspace_root: instance.workspace_root.clone(),
            target_dir: instance.rust_analyzer_target_dir.clone(),
            server: instance.server.clone(),
            status: status_for_instance(instance),
            pid: Some(instance.pid),
            clients: Some(instance.clients),
            idle_for: Some(instance.idle_for),
            worktree_last_changed: None,
        }
    }
}

fn status_for_instance(instance: &InstanceSnapshot) -> String {
    if !instance.server_ready {
        "running-initial-build".to_owned()
    } else if instance.clients > 0 {
        "ready-connected".to_owned()
    } else {
        "ready-prewarmed".to_owned()
    }
}

fn status_for_process(process: &PrewarmProcessSnapshot) -> String {
    match process.status.as_str() {
        "queued" => "pending-spawn".to_owned(),
        "workspace-missing" => "workspace-missing".to_owned(),
        "spawn-error" => "spawn-error".to_owned(),
        other => other.to_owned(),
    }
}

fn format_updated_at(updated_at: Option<SystemTime>) -> String {
    let Some(updated_at) = updated_at else {
        return "not run yet".to_owned();
    };

    match SystemTime::now().duration_since(updated_at) {
        Ok(duration) => format!("{} ago", format_duration(duration)),
        Err(_) => "just now".to_owned(),
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn workspace_label(worktree: &Path, workspace_root: &Path) -> String {
    if workspace_root == worktree {
        return ".".to_owned();
    }

    workspace_root
        .strip_prefix(worktree)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(path_label)
        .unwrap_or_else(|| path_label(workspace_root))
}

fn distinct_path_labels(paths: &[PathBuf]) -> HashMap<PathBuf, String> {
    let unique_paths = paths
        .iter()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let components_by_path = unique_paths
        .iter()
        .map(|path| (path.clone(), path_components(path)))
        .collect::<HashMap<_, _>>();

    let mut labels = HashMap::new();
    for path in unique_paths {
        let components = components_by_path
            .get(&path)
            .expect("components inserted above");
        let mut label = path_label(&path);

        for suffix_len in 1..=components.len() {
            let suffix = suffix_components(components, suffix_len);
            let count = components_by_path
                .values()
                .filter(|other| suffix_components(other, suffix_len) == suffix)
                .count();
            if count == 1 {
                label = suffix;
                break;
            }
        }

        labels.insert(path, label);
    }

    labels
}

fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

fn suffix_components(components: &[String], suffix_len: usize) -> String {
    if suffix_len > components.len() {
        return components.join("/");
    }
    components[components.len() - suffix_len..].join("/")
}

fn path_label(path: &Path) -> String {
    let label = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned);

    label.unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_path_labels_use_shortest_unique_suffix() {
        let paths = vec![
            PathBuf::from("/work/client/main"),
            PathBuf::from("/work/server/main"),
            PathBuf::from("/work/slint-a"),
        ];

        let labels = distinct_path_labels(&paths);

        assert_eq!(
            labels
                .get(&PathBuf::from("/work/client/main"))
                .map(String::as_str),
            Some("client/main")
        );
        assert_eq!(
            labels
                .get(&PathBuf::from("/work/server/main"))
                .map(String::as_str),
            Some("server/main")
        );
        assert_eq!(
            labels
                .get(&PathBuf::from("/work/slint-a"))
                .map(String::as_str),
            Some("slint-a")
        );
    }

    #[test]
    fn workspace_label_prefers_relative_path() {
        assert_eq!(
            workspace_label(Path::new("/work/repo"), Path::new("/work/repo/client")),
            "client"
        );
        assert_eq!(
            workspace_label(Path::new("/work/repo"), Path::new("/work/repo")),
            "."
        );
    }
}
