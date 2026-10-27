use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use std::{env, fmt};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::{select, task};
use tracing::{debug, error, field, info, instrument, trace, warn, Instrument};

use crate::client::Client;
use crate::config::{Config, RustAnalyzerTargetDirGrouping};
use crate::lsp::ext::Tag;
use crate::lsp::jsonrpc::{Message, Notification, Request, RequestId, ResponseSuccess, Version};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::{self, ext};

/// Specifies server configuration
///
/// If another server with the same configuration is requested we can reuse it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstanceKey {
    pub server: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workspace_root: WorkspaceRoot,
}

/// Represents workspace root as a unique directory
///
/// On some file systems or operating systems file/directory equality is more
/// complicated than equality on paths. Windows is by default case-insensitive,
/// and MacOS's APFS compares after unicode normalization. The only reliable way
/// to ensure two paths are the same is to compare their inode and dev numbers.
#[derive(Clone)]
pub struct WorkspaceRoot {
    path: String,
    device_id: u64,
    file_id: u64,
}

impl WorkspaceRoot {
    #[cfg(any(target_os = "redox", unix))]
    pub fn from_path(path: String) -> Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = Path::new(&path)
            .metadata()
            .with_context(|| format!("error getting metadata for path {path:?}"))?;

        Ok(Self {
            path,
            device_id: metadata.dev(),
            file_id: metadata.ino(),
        })
    }

    #[cfg(windows)]
    pub fn from_path(path: String) -> Result<Self> {
        use winapi_util::{file, Handle};

        let handle =
            Handle::from_path_any(&path).with_context(|| format!("error opening path {path:?}"))?;
        let information = file::information(&handle)
            .with_context(|| format!("error getting file information for path {path:?}"))?;

        Ok(Self {
            path,
            device_id: information.volume_serial_number(),
            file_id: information.file_index(),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl PartialEq for WorkspaceRoot {
    fn eq(&self, other: &Self) -> bool {
        // Skip `self.path`
        self.device_id == other.device_id && self.file_id == other.file_id
    }
}

impl Eq for WorkspaceRoot {}

impl Hash for WorkspaceRoot {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Skip `self.path`
        self.device_id.hash(state);
        self.file_id.hash(state);
    }
}

impl fmt::Debug for WorkspaceRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <String as fmt::Debug>::fmt(&self.path, f)
    }
}

/// Language server instance
pub struct Instance {
    key: InstanceKey,

    /// Language server child process id
    pid: u32,

    /// Server's response to `initialize` request
    init_result: lsp::InitializeResult,

    /// Handle for sending messages to the language server instance
    server: mpsc::Sender<Message>,

    /// Data of associated clients
    clients: Mutex<HashMap<usize, ClientData>>,

    /// Dynamic capabilities registered by the server
    dynamic_capabilities: Mutex<HashMap<String, lsp::Registration>>,

    /// Wakes up `wait_task` and asks it to send SIGKILL to the instance.
    close: Notify,

    /// Last time a message was sent to this instance
    ///
    /// Uses seconds elapsed since an arbitrary point in the past, as returned
    /// by [`elapsed_seconds`].
    last_used: AtomicI64,

    /// Whether the server has reported that startup/background work is done.
    server_ready: AtomicBool,

    /// rust-analyzer target directory injected into initialization options.
    rust_analyzer_target_dir: Option<PathBuf>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Make sure we're not leaking anything
        debug!("instance dropped");
    }
}

/// Wrapper around client handle with additional data only the server instance
/// knows about
struct ClientData {
    /// Handle for sending messages to clients
    client: Client,

    /// URIs of files currently opened by this client
    files: HashSet<String>,
}

impl ClientData {
    fn get_status(&self) -> ext::Client {
        ext::Client {
            id: self.client.id(),
            files: self.files.iter().cloned().collect(),
        }
    }
}

impl Deref for ClientData {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

// Elapsed seconds since an arbitrary [`Instant`] when this function was called for the first time
fn elapsed_seconds() -> i64 {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    START.elapsed().as_secs() as i64
}

impl Instance {
    /// Mark the instance as used
    pub fn keep_alive(&self) {
        self.last_used.store(elapsed_seconds(), Ordering::Relaxed);
    }

    /// How many seconds is the instance idle for
    pub fn idle(&self) -> i64 {
        i64::max(
            0,
            elapsed_seconds() - self.last_used.load(Ordering::Relaxed),
        )
    }

    pub fn initialize_result(&self) -> lsp::InitializeResult {
        self.init_result.clone()
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn server_ready(&self) -> bool {
        self.server_ready.load(Ordering::Relaxed)
    }

    pub async fn snapshot(&self) -> InstanceSnapshot {
        InstanceSnapshot {
            pid: self.pid(),
            server: self.key.server.clone(),
            workspace_root: PathBuf::from(self.key.workspace_root.path.clone()),
            clients: self.client_count().await,
            idle_for: self.idle(),
            server_ready: self.server_ready(),
            rust_analyzer_target_dir: self.rust_analyzer_target_dir.clone(),
        }
    }

    fn mark_server_ready(&self) {
        if !self.server_ready.swap(true, Ordering::Relaxed) {
            debug!("server reported ready");
        }
    }

    /// Add client to the instance so it can receive traffic from it
    ///
    /// It replays all registered dynamic capabilities to it.
    pub async fn add_client(&self, client: Client) {
        // NOTE: this method holds both `clients` and `dynamic_capabilities`
        // locks across a `send_message` await. This is acceptable because the
        // client channel is freshly created and no receiver task has been
        // spawned yet, so the send completes immediately without yielding.
        //
        // If `add_client` is ever called with an active client whose channel
        // might be full, this must be restructured to snapshot and release the
        // locks first.
        let mut clients = self.clients.lock().await;
        let dyn_capabilities = self.dynamic_capabilities.lock().await;

        if !dyn_capabilities.is_empty() {
            // Register all currently cached dynamic capabilities if there are
            // any. We will drop the client response and we need to make sure
            // the request ID is unique.
            let id = RequestId::String("replay:registerCapabilities".into()).tag(Tag::Drop);
            let params = lsp::RegistrationParams {
                registrations: dyn_capabilities.values().cloned().collect(),
            };
            let req = Request {
                id,
                method: "client/registerCapability".into(),
                params: serde_json::to_value(params).unwrap(),
                jsonrpc: Version,
            };
            debug!(?req, "replaying server request");
            let _ = client.send_message(req.into()).await;
        }

        let client = ClientData {
            client,
            files: HashSet::new(),
        };
        if clients.insert(client.id(), client).is_some() {
            unreachable!("BUG: added two clients with the same ID");
        }
    }

    /// Get a single client.
    ///
    /// This returns owned data so the lock can be acquired and dropped
    /// immediately.
    pub async fn get_client(&self, id: usize) -> Option<Client> {
        let clients = self.clients.lock().await;
        clients.get(&id).map(|cd| cd.client.clone())
    }

    /// Return a snapshot of clients.
    ///
    /// This returns owned data so the lock can be acquired and dropped
    /// immediately.
    pub async fn clients(&self) -> Vec<Client> {
        let clients = self.clients.lock().await;
        clients.values().map(|cd| cd.client.clone()).collect()
    }

    /// Return one arbitrary client.
    ///
    /// This returns owned data so the lock can be acquired and dropped
    /// immediately.
    pub async fn one_client(&self) -> Option<Client> {
        let clients = self.clients.lock().await;
        clients.values().next().map(|cd| cd.client.clone())
    }

    pub async fn client_count(&self) -> usize {
        self.clients.lock().await.len()
    }

    pub fn request_close(&self) {
        self.close.notify_one();
    }

    /// Send cleanup messages and remove remove client for client map
    pub async fn cleanup_client(&self, client: Client) -> Result<()> {
        debug!("cleaning up client");

        let mut clients = self.clients.lock().await;

        let Some(client) = clients.remove(&client.id()) else {
            // TODO This happens for example when the language server died while
            // client was still connected, and the client cleanup is attempted
            // with the instance being gone already. We should try notifying
            // these clients immediately and handling the cleanup separately.
            bail!("client was not connected");
        };

        let files = client.files.into_iter().collect::<Vec<_>>();
        self.close_all_files(&clients, files)
            .await
            .context("error closing files")?;

        Ok(())
    }

    /// Send a message to the language server channel
    pub async fn send_message(&self, message: Message) -> Result<(), SendError<Message>> {
        self.server.send(message).await
    }

    /// Save registered capabilities to allow later replaying them to new clients
    async fn register_capabilities(&self, params: Value) -> Result<()> {
        let params =
            serde_json::from_value::<lsp::RegistrationParams>(params).context("parsing params")?;

        let mut dyn_capabilities = self.dynamic_capabilities.lock().await;
        for reg in params.registrations {
            dyn_capabilities.insert(reg.id.clone(), reg);
        }

        Ok(())
    }

    /// Remove cached capability registration to stop replaying them to new clients
    async fn unregister_capabilities(&self, params: Value) -> Result<()> {
        let params = serde_json::from_value::<lsp::UnregistrationParams>(params)
            .context("parsing params")?;

        let mut dyn_capabilities = self.dynamic_capabilities.lock().await;
        for unreg in params.unregistrations {
            dyn_capabilities.remove(&unreg.id);
        }

        Ok(())
    }

    /// Handle `textDocument/didOpen` client notification
    pub async fn open_file(&self, client_id: usize, params: Value) -> Result<()> {
        let params = serde_json::from_value::<lsp::DidOpenTextDocumentParams>(params)
            .context("parsing params")?;
        let uri = &params.text_document.uri;

        let mut send_notification = true;

        let mut clients = self.clients.lock().await;
        for client in clients.values() {
            if client.files.contains(uri) {
                debug!(?uri, "file is already opened by another client");
                send_notification = false;
                break;
            }
        }

        clients
            .get_mut(&client_id)
            .expect("no matching client")
            .files
            .insert(uri.clone());

        if send_notification {
            let notif = Notification {
                jsonrpc: Version,
                method: "textDocument/didOpen".into(),
                params: serde_json::to_value(params).unwrap(),
            };
            debug!(?notif, "first client opened file");
            let _ = self.send_message(notif.into()).await;
        }

        Ok(())
    }

    /// Handle `textDocument/didClose` client notification
    pub async fn close_file(&self, client_id: usize, params: Value) -> Result<()> {
        let params = serde_json::from_value::<lsp::DidCloseTextDocumentParams>(params)
            .context("parsing params")?;

        let mut clients = self.clients.lock().await;

        clients
            .get_mut(&client_id)
            .context("no matching client")?
            .files
            .remove(&params.text_document.uri);

        self.close_all_files(&clients, vec![params.text_document.uri])
            .await
    }

    /// Handle closing many files at once and sending notifications for
    /// definitely closed files
    async fn close_all_files(
        &self,
        clients: &HashMap<usize, ClientData>,
        files: Vec<String>,
    ) -> Result<()> {
        for uri in files {
            let mut send_notification = true;

            for client in clients.values() {
                if client.files.contains(&uri) {
                    debug!(?uri, "file still opened by another client");
                    send_notification = false;
                    break;
                }
            }

            if send_notification {
                let params = lsp::DidCloseTextDocumentParams {
                    text_document: lsp::TextDocumentIdentifier { uri },
                };
                let notif = Notification {
                    jsonrpc: Version,
                    method: "textDocument/didClose".into(),
                    params: serde_json::to_value(params).unwrap(),
                };
                debug!(?notif, "last client closed file");
                let _ = self.send_message(notif.into()).await;
            }
        }

        Ok(())
    }

    pub async fn get_status(&self) -> ext::Instance {
        let clients = self
            .clients
            .lock()
            .await
            .values()
            .map(|client| client.get_status())
            .collect();

        let registered_dyn_capabilities = self
            .dynamic_capabilities
            .lock()
            .await
            .values()
            .map(|reg| reg.method.clone())
            .collect();

        ext::Instance {
            pid: self.pid,
            server: self.key.server.clone(),
            args: self.key.args.clone(),
            env: self.key.env.clone(),
            workspace_root: ext::WorkspaceRoot {
                path: self.key.workspace_root.path.clone(),
                device_id: self.key.workspace_root.device_id,
                file_id: self.key.workspace_root.file_id,
            },
            idle_for: self.idle(),
            clients,
            registered_dyn_capabilities,
        }
    }

    /// Read all files currently open by any client from disk and generate a
    /// `textDocument/didChange` notification with the full content for each
    /// of them.
    pub async fn sync_files_from_disk(&self) -> Result<()> {
        let unique_files = self
            .clients
            .lock()
            .await
            .values()
            .flat_map(|client| client.files.iter().cloned())
            .collect::<HashSet<String>>();

        for uri in unique_files {
            let path = match lsp::parse_file_uri(&uri) {
                Ok(path) => path,
                Err(err) => {
                    warn!(?uri, ?err, "failed to parse URI");
                    continue;
                }
            };

            let text = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                Err(err) => {
                    warn!(?path, ?err, "failed to read file from disk");
                    continue;
                }
            };

            let params = lsp::DidChangeTextDocumentParams {
                text_document: lsp::VersionedTextDocumentIdentifier { uri, version: None },
                content_changes: vec![lsp::TextDocumentContentChangeEvent { text }],
            };

            let notif = Notification {
                jsonrpc: Version,
                method: "textDocument/didChange".into(),
                params: serde_json::to_value(params).unwrap(),
            };

            self.send_message(notif.into())
                .await
                .context("instance closed")?;
        }

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InstanceSnapshot {
    pub pid: u32,
    pub server: String,
    pub workspace_root: PathBuf,
    pub clients: usize,
    pub idle_for: i64,
    pub server_ready: bool,
    pub rust_analyzer_target_dir: Option<PathBuf>,
}

pub struct InstanceMap {
    instances: HashMap<InstanceKey, Arc<Instance>>,
    rust_analyzer_target_dirs: RustAnalyzerTargetDirs,
}

#[derive(Clone)]
struct RustAnalyzerTargetDirs {
    root: PathBuf,
    grouping: RustAnalyzerTargetDirGrouping,
}

impl InstanceMap {
    pub fn new(config: &Config) -> Arc<Mutex<Self>> {
        let instance_map = Arc::new(Mutex::new(InstanceMap {
            instances: HashMap::new(),
            rust_analyzer_target_dirs: RustAnalyzerTargetDirs {
                root: config.resolved_rust_analyzer_target_dir_root(),
                grouping: config.rust_analyzer_target_dir_grouping,
            },
        }));
        task::spawn(gc_task(
            instance_map.clone(),
            config.gc_interval,
            config.instance_timeout,
        ));
        instance_map
    }

    /// Finds an instance with the longest path such as
    /// `cwd.starts_with(workspace_root)` is true.
    ///
    /// This returns owned data to allow callers to drop the lock immediately.
    pub fn get_by_cwd(&self, cwd: &str) -> Option<Arc<Instance>> {
        self.instances
            .iter()
            .filter(|(key, _)| Path::new(cwd).starts_with(&key.workspace_root.path))
            .max_by_key(|(key, _)| key.workspace_root.path.len())
            .map(|(_, inst)| inst.clone())
    }

    /// Return a snapshot of instances.
    ///
    /// This returns owned data to allow callers to drop the lock immediately.
    pub fn instances(&self) -> Vec<Arc<Instance>> {
        self.instances.values().cloned().collect()
    }

    pub fn get(&self, key: &InstanceKey) -> Option<Arc<Instance>> {
        self.instances.get(key).cloned()
    }
}

/// Periodically check for for idle language server instances
#[instrument("garbage collector", skip_all)]
async fn gc_task(
    instance_map: Arc<Mutex<InstanceMap>>,
    gc_interval: u32,
    instance_timeout: Option<u32>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(gc_interval.into()));
    loop {
        interval.tick().await;

        let instances = instance_map.lock().await.instances();
        for instance in &instances {
            // The instance might have been removed from the map since we took
            // the snapshot. If so, wait_task has already exited and
            // notify_one below is a no-op. Otherwise, wait_task will receive
            // the notification and kill the child.
            let clients = instance.clients.lock().await;

            let idle = instance.idle();
            debug!(path = ?instance.key.workspace_root, idle, clients = clients.len(), "check instance");

            if let Some(instance_timeout) = instance_timeout {
                // Close timed out instance
                if idle > i64::from(instance_timeout) && clients.is_empty() {
                    info!(pid = instance.pid, path = ?instance.key.workspace_root, idle, "instance timed out");
                    instance.close.notify_one();
                }
            }
        }
    }
}

/// Find existing or spawn a new language server instance
///
/// The instance is looked up based on `instance_key`. If an existing one is
/// found then it's returned and `init_req_params` are discarded. If it's
/// not found a new instance is spawned and initialized using the provided
/// `init_req_params`, this insance is then inserted into the map and returned.
pub async fn get_or_spawn(
    map: Arc<Mutex<InstanceMap>>,
    key: InstanceKey,
    init_req_params: lsp::InitializeParams,
    client: Client,
) -> Result<Arc<Instance>> {
    // We have locked a clone of an Arc of the map, we can assume noone else
    // tries to spawn the same instance again. But we have to make sure `spawn`
    // doesn't try to lock its copy as well. This is a bit unfortunate code
    // organization but we want to have spawn in a separate tracing context and
    // we want to include `wait_task` in it as well in it as well
    let map_clone = map.clone();
    let mut map_guard = map_clone.lock().await;
    let rust_analyzer_target_dirs = map_guard.rust_analyzer_target_dirs.clone();
    match map_guard.instances.entry(key.clone()) {
        Entry::Occupied(e) => {
            info!("reusing language server instance");
            e.get().add_client(client).await;
            Ok(e.get().clone())
        }
        Entry::Vacant(e) => {
            let instance = spawn(
                key,
                init_req_params,
                map,
                rust_analyzer_target_dirs,
                Some(client),
            )
            .await
            .context("spawning instance")?;
            e.insert(instance.clone());
            Ok(instance)
        }
    }
}

/// Spawn a language server instance without attaching a client.
///
/// This is used for speculative prewarming. If an equivalent instance already
/// exists, it is left untouched and `Ok(None)` is returned.
pub async fn prewarm(
    map: Arc<Mutex<InstanceMap>>,
    key: InstanceKey,
    init_req_params: lsp::InitializeParams,
) -> Result<Option<Arc<Instance>>> {
    let map_clone = map.clone();
    let mut map_guard = map_clone.lock().await;
    let rust_analyzer_target_dirs = map_guard.rust_analyzer_target_dirs.clone();
    match map_guard.instances.entry(key.clone()) {
        Entry::Occupied(_) => Ok(None),
        Entry::Vacant(e) => {
            let instance = spawn(key, init_req_params, map, rust_analyzer_target_dirs, None)
                .await
                .context("spawning instance")?;
            e.insert(instance.clone());
            Ok(Some(instance))
        }
    }
}

async fn apply_rust_analyzer_target_dir(
    key: &InstanceKey,
    config: &RustAnalyzerTargetDirs,
    init_req_params: &mut lsp::InitializeParams,
) -> Result<Option<PathBuf>> {
    if !is_rust_analyzer_server(&key.server) {
        return Ok(None);
    }

    let target_dir = rust_analyzer_target_dir(key, config).await?;
    tokio::fs::create_dir_all(&target_dir)
        .await
        .with_context(|| format!("creating rust-analyzer target directory {target_dir:?}"))?;
    set_rust_analyzer_target_dir(init_req_params, &target_dir)?;

    info!(?target_dir, "configured rust-analyzer target directory");
    Ok(Some(target_dir))
}

pub async fn expected_rust_analyzer_target_dir(
    map: Arc<Mutex<InstanceMap>>,
    key: &InstanceKey,
) -> Option<PathBuf> {
    if !is_rust_analyzer_server(&key.server) {
        return None;
    }

    let config = map.lock().await.rust_analyzer_target_dirs.clone();
    rust_analyzer_target_dir(key, &config).await.ok()
}

async fn rust_analyzer_target_dir(
    key: &InstanceKey,
    config: &RustAnalyzerTargetDirs,
) -> Result<PathBuf> {
    let directory = match config.grouping {
        RustAnalyzerTargetDirGrouping::Shared => "shared".to_owned(),
        RustAnalyzerTargetDirGrouping::PerGitRemote => {
            let remote = git_remote_or_workspace_key(key.workspace_root.path()).await;
            format!("remote-{}", stable_hash([remote]))
        }
        RustAnalyzerTargetDirGrouping::PerInstance => {
            format!("instance-{}", stable_hash(instance_hash_parts(key)))
        }
    };

    Ok(config.root.join(directory))
}

fn is_rust_analyzer_server(server: &str) -> bool {
    matches!(
        Path::new(server).file_name().and_then(|name| name.to_str()),
        Some("rust-analyzer" | "rust-analyzer.exe")
    )
}

async fn git_remote_or_workspace_key(workspace_root: &str) -> String {
    if let Some(remote) = git_remote_url(workspace_root, ["remote", "get-url", "origin"]).await {
        return remote;
    }
    if let Some(remote) = git_remote_url(workspace_root, ["remote", "-v"]).await {
        return first_fetch_remote_url(&remote).unwrap_or(remote);
    }

    workspace_root.to_owned()
}

async fn git_remote_url<const N: usize>(workspace_root: &str, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(args)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let remote = stdout.trim();
    (!remote.is_empty()).then(|| remote.to_owned())
}

fn first_fetch_remote_url(output: &str) -> Option<String> {
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let Some(_) = fields.next() else {
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

fn instance_hash_parts(key: &InstanceKey) -> Vec<String> {
    let mut parts = vec![
        key.server.clone(),
        key.workspace_root.path.clone(),
        key.workspace_root.device_id.to_string(),
        key.workspace_root.file_id.to_string(),
    ];
    parts.extend(key.args.iter().map(|arg| format!("arg:{arg}")));
    parts.extend(
        key.env
            .iter()
            .map(|(name, value)| format!("env:{name}={value}")),
    );
    parts
}

fn stable_hash(parts: impl IntoIterator<Item = String>) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for part in parts {
        for byte in part.as_bytes().iter().copied().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }

    format!("{hash:016x}")
}

fn set_rust_analyzer_target_dir(
    init_req_params: &mut lsp::InitializeParams,
    target_dir: &Path,
) -> Result<()> {
    let target_dir = target_dir
        .to_str()
        .with_context(|| format!("target directory is not valid utf-8: {target_dir:?}"))?
        .to_owned();
    let options = init_req_params
        .initialization_options
        .get_or_insert_with(Default::default);

    let cargo = options
        .other_options
        .entry("cargo".to_owned())
        .or_insert_with(|| json!({}));
    if !cargo.is_object() {
        *cargo = json!({});
    }
    cargo
        .as_object_mut()
        .expect("cargo option object inserted above")
        .insert("targetDir".to_owned(), Value::String(target_dir));

    Ok(())
}

#[instrument(name = "instance", fields(pid = field::Empty), skip_all, parent = None)]
async fn spawn(
    key: InstanceKey,
    mut init_req_params: lsp::InitializeParams,
    // Caller `get_or_spawn` is holding a lock to the map, we must not try to
    // lock it within this function to not cause deadlock, only spawned tasks
    // are allowed to lock it again.
    map: Arc<Mutex<InstanceMap>>,
    rust_analyzer_target_dirs: RustAnalyzerTargetDirs,
    client: Option<Client>,
) -> Result<Arc<Instance>> {
    let rust_analyzer_target_dir =
        apply_rust_analyzer_target_dir(&key, &rust_analyzer_target_dirs, &mut init_req_params)
            .await
            .context("configure rust-analyzer target directory")?;

    let mut child = Command::new(&key.server)
        .args(&key.args)
        .envs(&key.env)
        .current_dir(&key.workspace_root.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            let InstanceKey {
                server,
                args,
                env,
                workspace_root,
            } = &key;
            let path = env
                .get("PATH")
                .map(<_>::to_owned)
                // Display PATH from our environment Command will if none was
                // passed from the client environment.
                .or_else(|| env::var("PATH").ok())
                .unwrap_or_default();
            format!(
                "spawning language server: server={server:?}, args={args:?}, \
                cwd={workspace_root:?}, path={path:?}, env={env:?}",
            )
        })?;

    let pid = child.id().context("child exited early, couldn't get PID")?;
    tracing::Span::current().record("pid", pid);

    info!(server = ?key.server, args = ?key.args, cwd = ?key.workspace_root, "spawned language server");

    let stderr = child.stderr.take().unwrap();
    task::spawn(stderr_task(stderr).in_current_span());

    let stdout = child.stdout.take().unwrap();
    let mut reader = LspReader::new(BufReader::new(stdout), "server");

    let stdin = child.stdin.take().unwrap();
    let mut writer = LspWriter::new(stdin, "server");

    // Some LSP servers monitor client PIDs and exit if none remain (as
    // documented in the LSP spec). We need to replace the client PID in the
    // initialization request with lsp-herd server PID to prevent the server from
    // exitting when the original client exits.
    init_req_params.process_id = Some(std::process::id() as u64);

    let init_result = initialize_handshake(init_req_params, &mut reader, &mut writer)
        .await
        .context("server handshake")?;

    info!("initialized server");

    let (message_writer, rx) = mpsc::channel(64);

    let mut clients = HashMap::new();
    if let Some(client) = client {
        let client = ClientData {
            client,
            files: HashSet::new(),
        };
        clients.insert(client.id(), client);
    }

    let instance = Arc::new(Instance {
        key,
        pid,
        init_result,
        server: message_writer,
        clients: Mutex::new(clients),
        dynamic_capabilities: Mutex::default(),
        close: Notify::new(),
        last_used: AtomicI64::new(elapsed_seconds()),
        server_ready: AtomicBool::new(false),
        rust_analyzer_target_dir,
    });

    task::spawn(stdout_task(instance.clone(), reader).in_current_span());
    task::spawn(stdin_task(rx, writer).in_current_span());

    task::spawn(wait_task(instance.clone(), map, child).in_current_span());

    Ok(instance)
}

#[instrument(skip_all)]
async fn initialize_handshake(
    init_req_params: lsp::InitializeParams,
    reader: &mut LspReader<BufReader<ChildStdout>>,
    writer: &mut LspWriter<ChildStdin>,
) -> Result<lsp::InitializeResult> {
    let request_id = "lsp-herd:initialize_request";

    // Use the first client's `InitializeParams` to initialize server. We assume
    // all subsequent clients configuration will be somewhat compatible with
    // whatever the first client negotiated for the same `workspace_root`,
    // `server` and `args`.
    let req = Request {
        jsonrpc: Version,
        method: "initialize".into(),
        params: serde_json::to_value(init_req_params).unwrap(),
        id: RequestId::String(request_id.into()),
    };
    writer
        .write_message(&req.into())
        .await
        .context("send initialize request")?;

    // TODO: Ignoring messages here is not ideal. The spec explicitly permits
    // sending `window/showMessage`, `window/logMessage`, `telemetry/event`,
    // `window/showMessageRequest` and when configured also `$/progress` [1],
    // which we should forward to the client(s). Not forwarding them means
    // the server will appear unresponsive while it's initializing.
    //
    // [1]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#initialize
    //
    // FIXME: The larger problem with this approach is that some servers (e.g.
    // `kotlin-language-server`) do not respond with an initialize response
    // immediately, but block until they've indexed the whole project. And
    // we hold the `InstanceMap` lock _the whole time_ this loop is waiting,
    // the lock held in `instance::get_or_spawn` -> `instance::spawn` ->
    // `instance::initialize_handshake` which blocks _any_ connection to the
    // lsp-herd client, as all the functions called from `client::process` sooner
    // or later need to lock the `InstanceMap` as well.
    let res = loop {
        match reader
            .read_message()
            .await
            .context("receive initialize response")?
            .context("stream ended")?
        {
            Message::ResponseSuccess(res) if res.id == request_id => break res,
            msg => {
                warn!(
                    ?msg,
                    "ignoring message while waiting for initialize response"
                );
            }
        }
    };
    let result = serde_json::from_value(res.result).context("parse initialize response result")?;

    // Send a "fake" `initialized` notification to the server. We wait for them
    // from each client's `initialized` notification ourselves and they don't
    // contain any data that would need passing on.
    let init_notif = Notification {
        jsonrpc: Version,
        method: "initialized".into(),
        params: json!({}),
    };
    writer
        .write_message(&init_notif.into())
        .await
        .context("send initialized notification")?;

    Ok(result)
}

/// Read errors from language server stderr and log them
async fn stderr_task(stderr: ChildStderr) {
    let mut stderr = BufReader::new(stderr);
    let mut buffer = String::new();

    loop {
        buffer.clear();
        match stderr.read_line(&mut buffer).await {
            Ok(0) => {
                // reached EOF
                debug!("stderr closed");
                break;
            }
            Ok(_) => {
                let line = buffer.trim_end(); // remove trailing '\n' or possibly '\r\n'
                error!(%line, "stderr");
            }
            Err(err) => {
                let err = anyhow::Error::from(err);
                error!(?err, "error reading from stderr");
            }
        }
    }
}

/// Receive messages from clients' channel and write them into language server stdin
async fn stdin_task(mut receiver: mpsc::Receiver<Message>, mut writer: LspWriter<ChildStdin>) {
    // Because we (stdin task) don't keep a reference to `self` it will be dropped when the
    // child closes and all the clients disconnect including the sender and this receiver
    // will not keep blocking (unlike in client input task)
    while let Some(message) = receiver.recv().await {
        if let Err(err) = writer.write_message(&message).await {
            match err.kind() {
                // stdin is closed, no need to log an error
                ErrorKind::BrokenPipe => {}
                _ => {
                    let err = anyhow::Error::from(err);
                    error!(?err, "error writing to stdin");
                }
            }
            break;
        }
    }
    debug!("stdin closed");
}

/// Wait for child and log when it exits
async fn wait_task(
    instance: Arc<Instance>,
    instance_map: Arc<Mutex<InstanceMap>>,
    mut child: Child,
) {
    let key = instance.key.clone();
    loop {
        select! {
            _ = instance.close.notified() => {
                if let Err(err) = child.start_kill() {
                    error!(?err, "failed to close child");
                }
            }
            exit = child.wait() => {
                // Remove the closing instance from the map so new clients spawn their own instance
                instance_map.lock().await.instances.remove(&key);

                // Disconnect all current clients
                //
                // We'll rely on the editor client to restart the lsp-herd client,
                // start a new connection and we'll spawn another instance like
                // we'd with any other new client.
                instance.clients.lock().await.clear();

                match exit {
                    Ok(status) => {
                        #[cfg(unix)]
                        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                        #[cfg(not(unix))]
                        let signal = tracing::field::Empty;

                        error!(
                            success = status.success(),
                            code = status.code(),
                            signal,
                            "child exited",
                        );
                    }
                    Err(err) => error!(?err, "error waiting for child"),
                }
                break;
            }
        }
    }
}

/// Read messages from server stdout and send them to corresponding client channels
async fn stdout_task(instance: Arc<Instance>, mut reader: LspReader<BufReader<ChildStdout>>) {
    loop {
        let message = match reader.read_message().await {
            Ok(Some(message)) => message,
            Ok(None) => {
                debug!("stdout closed");
                break;
            }
            Err(err) => {
                error!(?err, "reading message");
                continue;
            }
        };

        // Each match arm holds a lock for as little as possible (this is
        // encapsulated within `get_client`, etc). The lock protocol prevents a
        // slow client from holding the lock and blocking all other clients and
        // lock-dependent operations.
        match message {
            Message::ResponseSuccess(mut res) => {
                // Forward successful response to the right client based on the
                // Request ID tag.
                match res.id.untag() {
                    (Some(Tag::ClientId(client_id)), id) => {
                        res.id = id;
                        if let Some(client) = instance.get_client(client_id).await {
                            let _ = client.send_message(res.into()).await;
                        } else {
                            debug!(?client_id, "no matching client");
                        }
                    }
                    (Some(Tag::Drop), _) => {
                        // Drop the message
                    }
                    _ => {
                        warn!(?res, "ignoring improperly tagged server response")
                    }
                }
            }

            Message::ResponseError(mut res) => {
                // Forward the error response to the right client based on the
                // Request ID tag.
                match res.id.untag() {
                    (Some(Tag::ClientId(client_id)), id) => {
                        warn!(?res, "server responded with error");
                        res.id = id;
                        if let Some(client) = instance.get_client(client_id).await {
                            let _ = client.send_message(res.into()).await;
                        } else {
                            debug!(?client_id, "no matching client");
                        }
                    }
                    (Some(Tag::Drop), _) => {
                        // Drop the message
                    }
                    _ => {
                        warn!(?res, "ignoring improperly tagged server response")
                    }
                }
            }

            Message::Request(mut req)
                if [
                    "window/workDoneProgress/create",
                    "workspace/codeLens/refresh",
                    "workspace/semanticTokens/refresh",
                    "workspace/inlayHint/refresh",
                    "workspace/inlineValue/refresh",
                    "workspace/diagnostic/refresh",
                ]
                .contains(&req.method.as_str()) =>
            {
                // All these server requests have null responses and we need
                // to inform all clients. We can forward the request to all
                // clients, send a fake successful response and ignore the real
                // client responses.
                trace!(?req, "server request {}", req.method.as_str());

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                for client in instance.clients().await {
                    let _ = client.send_message(req.clone().into()).await;
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(mut req) if req.method == "workspace/configuration" => {
                // Response to `workspace/configuration` should be the same from
                // any client. So we'll just pick the first and let it answer.
                debug!(?req, "server request workspace/configuration");

                let id = req.id.clone();
                req.id = req.id.tag(Tag::Forward);

                if let Some(client) = instance.one_client().await {
                    let _ = client.send_message(req.into()).await;
                } else {
                    let result = empty_workspace_configuration_response(&req.params);
                    let _ = instance
                        .send_message(
                            ResponseSuccess {
                                jsonrpc: Version,
                                result,
                                id,
                            }
                            .into(),
                        )
                        .await;
                }
            }

            Message::Request(mut req) if req.method == "client/registerCapability" => {
                // These need to be forwarded to every client so they're aware
                // of the capability. The response doesn't contain anything
                // important so we can safely ignore the real answers and send a
                // fake one to the server.
                debug!(?req, "server request client/registerCapability");

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                // Cache before broadcasting so that a client connecting
                // between the cache update and the broadcast will see
                // the capability via add_client's replay. The worst case
                // is a duplicate registerCapability, which is benign.
                if let Err(err) = instance.register_capabilities(req.params.clone()).await {
                    warn!(?err, "error registering capabilities");
                }

                for client in instance.clients().await {
                    let _ = client.send_message(req.clone().into()).await;
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(mut req) if req.method == "client/unregisterCapability" => {
                // These need to be forwarded to every client so they're aware
                // of the capability not being available anymore. The response
                // doesn't contain anything important so we can safely ignore
                // the real answers and send a fake one to the server.
                debug!(?req, "server request client/unregisterCapability");

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                // Uncache before broadcasting so that a client connecting
                // between the cache update and the broadcast won't receive
                // the stale capability via add_client's replay.
                if let Err(err) = instance.unregister_capabilities(req.params.clone()).await {
                    warn!(?err, "error unregistering capabilities");
                }

                for client in instance.clients().await {
                    let _ = client.send_message(req.clone().into()).await;
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(req) => {
                // Unimplemented server -> client requests I've found in the LSP Spec.
                // TODO workspace/workspaceFolders request
                // TODO workspace/applyEdit request
                debug!(message = ?req, "ignoring unknown server request");
            }

            Message::Notification(notif) => {
                if notif.method == "experimental/serverStatus"
                    && notif.params.get("quiescent").and_then(Value::as_bool) == Some(true)
                {
                    instance.mark_server_ready();
                }

                // Server notifications don't expect a response. We can forward
                // them to all clients.
                for client in instance.clients().await {
                    let _ = client.send_message(notif.clone().into()).await;
                }
            }
        }
    }
}

fn empty_workspace_configuration_response(params: &Value) -> Value {
    let len = params
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    Value::Array(vec![Value::Null; len])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::lsp::{InitializationOptions, TraceValue};

    fn workspace_root(path: &str) -> WorkspaceRoot {
        WorkspaceRoot {
            path: path.to_owned(),
            device_id: 1,
            file_id: 2,
        }
    }

    fn instance_key(path: &str) -> InstanceKey {
        InstanceKey {
            server: "rust-analyzer".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            workspace_root: workspace_root(path),
        }
    }

    fn init_params() -> lsp::InitializeParams {
        lsp::InitializeParams {
            process_id: Some(123),
            client_info: None,
            locale: None,
            root_path: Some("/repo".into()),
            root_uri: Some("file:///repo".into()),
            initialization_options: Some(InitializationOptions {
                lsp_mux: None,
                other_options: serde_json::Map::from_iter([(
                    "cargo".into(),
                    json!({ "features": ["unstable"], "targetDir": "client-target" }),
                )]),
            }),
            capabilities: None,
            trace: Some(TraceValue::Off),
            workspace_folders: None,
        }
    }

    #[test]
    fn detects_rust_analyzer_server_binary() {
        assert!(is_rust_analyzer_server("rust-analyzer"));
        assert!(is_rust_analyzer_server("/usr/bin/rust-analyzer"));
        assert!(!is_rust_analyzer_server("clangd"));
    }

    #[test]
    fn parses_first_fetch_remote_for_target_grouping() {
        assert_eq!(
            first_fetch_remote_url(
                "upstream\thttps://example.com/repo.git (fetch)\n\
                 upstream\thttps://example.com/repo.git (push)\n",
            )
            .as_deref(),
            Some("https://example.com/repo.git")
        );
    }

    #[test]
    fn injects_rust_analyzer_target_dir_without_dropping_cargo_options() {
        let mut params = init_params();

        set_rust_analyzer_target_dir(&mut params, Path::new("/tmp/lsp-herd-ra-target")).unwrap();

        let options = params.initialization_options.unwrap().other_options;
        assert_eq!(options["cargo"]["features"], json!(["unstable"]));
        assert_eq!(
            options["cargo"]["targetDir"],
            json!("/tmp/lsp-herd-ra-target")
        );
    }

    #[tokio::test]
    async fn target_dir_grouping_shapes_paths() {
        let root = PathBuf::from("/tmp/lsp-herd-ra-root");
        let shared = RustAnalyzerTargetDirs {
            root: root.clone(),
            grouping: RustAnalyzerTargetDirGrouping::Shared,
        };
        assert_eq!(
            rust_analyzer_target_dir(&instance_key("/repo"), &shared)
                .await
                .unwrap(),
            root.join("shared")
        );

        let per_instance = RustAnalyzerTargetDirs {
            root: root.clone(),
            grouping: RustAnalyzerTargetDirGrouping::PerInstance,
        };
        let first = rust_analyzer_target_dir(&instance_key("/repo-a"), &per_instance)
            .await
            .unwrap();
        let second = rust_analyzer_target_dir(&instance_key("/repo-b"), &per_instance)
            .await
            .unwrap();
        assert!(first.starts_with(&root));
        assert!(second.starts_with(&root));
        assert_ne!(first, second);
    }
}
