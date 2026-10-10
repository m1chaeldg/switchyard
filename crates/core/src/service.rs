//! The core service: owns the profile store, secret store, drivers, sessions and running
//! queries, and handles [`Command`]s on the runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::bus::{TermId, TermTarget};
use crate::components::MIRROR_SETTING;
use crate::entra::EntraSignIn;
use crate::prompts::BusPrompter;
use crate::terminals::{SshTerminalSpec, TermInput, Terminals};
use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::d1::D1Driver;
use switchyard_db::guard;
use switchyard_db::mssql::MssqlDriver;
use switchyard_db::pg::PgDriver;
use switchyard_db::{
    CancelHandle, CatalogChunk, DbConfig, DbError, DbSession, Driver, Engine, IntrospectScope,
    ResultEvent, dialect_for,
};
use switchyard_db::{DbAuthMethod, TunnelEndpoint};
use switchyard_drivers::Registry;
use switchyard_drivers::install::CommandRunner;
use switchyard_remote::ssh::{
    ForwardSpec, KnownHosts, SshAuthMethod, SshManager, SshTarget, Tunnel, TunnelInfo,
};
use switchyard_remote::{LocalFs, RemoteFs};
use switchyard_store::{
    AppPaths, DbConnection, FileConnection, HistoryEntry, HistoryStatus, Host, HostPatch,
    KeychainStore, MemoryStore, Profile, ProfileId, SecretRef, SecretStore, SshAuth, Store,
    StoreError, VaultStore, now_ms,
};
use switchyard_term::{LocalShell, TermSize};
use tokio::sync::mpsc;
use tracing::{Instrument, info, info_span, warn};

use crate::bus::{
    Command, Event, FetchLimit, QueryEvent, QueryId, RequestId, SessionContext, SessionId,
    StatementRequest,
};
use crate::error::{CoreError, Result};
use crate::runtime::EventSender;

mod activity;
pub mod agent;
pub mod agent_plan;
pub mod agent_ssh;
mod assistant;
mod redis;

/// The SSH layer's description of a saved forward.
fn forward_spec(f: &switchyard_store::PortForward) -> ForwardSpec {
    use switchyard_store::ForwardDirection;
    let bind_address = f.bind_address.trim().to_owned();
    match f.direction {
        ForwardDirection::Local => ForwardSpec::Local {
            bind_address,
            bind_port: f.bind_port,
            host: f.target_host.trim().to_owned(),
            port: f.target_port,
        },
        ForwardDirection::Remote => ForwardSpec::Remote {
            bind_address: if bind_address.is_empty() {
                "localhost".into()
            } else {
                bind_address
            },
            bind_port: f.bind_port,
            host: f.target_host.trim().to_owned(),
            port: f.target_port,
        },
        ForwardDirection::Dynamic => ForwardSpec::Dynamic {
            bind_address,
            bind_port: f.bind_port,
        },
    }
}

/// Where secrets go.
#[derive(Clone, Debug)]
pub enum SecretBackendChoice {
    /// OS keychain if available, else the vault at this path.
    Auto(PathBuf),
    /// The local vault at this path, even when a keychain exists (`SWITCHYARD_SECRETS=vault`).
    Vault(PathBuf),
    /// In-memory (tests, demos).
    Memory,
}

/// Service configuration.
#[derive(Clone)]
pub struct ServiceConfig {
    /// Profile store file; `None` keeps it in memory.
    pub store_path: Option<PathBuf>,
    /// Secret backend.
    pub secrets: SecretBackendChoice,
    /// Extra drivers (tests); the PostgreSQL driver is always registered.
    pub extra_drivers: Vec<(Engine, Arc<dyn Driver>)>,
    /// Host key files.
    pub known_hosts: KnownHosts,
    /// App-managed native components (`<data>/drivers`).
    pub drivers_dir: PathBuf,
    /// Package-manager runner (tests replace it).
    pub package_runner: Option<Arc<dyn CommandRunner>>,
    /// Data directory (assistant session tokens).
    pub data_dir: PathBuf,
    /// The `swy` executable for assistant runs; `None` finds it next to the app or on PATH.
    pub swy: Option<PathBuf>,
}

impl ServiceConfig {
    /// Everything in memory (tests).
    pub fn in_memory() -> Self {
        Self {
            store_path: None,
            secrets: SecretBackendChoice::Memory,
            extra_drivers: Vec::new(),
            known_hosts: KnownHosts {
                user_file: None,
                app_file: std::env::temp_dir()
                    .join(format!("switchyard-known-hosts-{}", std::process::id())),
            },
            drivers_dir: std::env::temp_dir()
                .join(format!("switchyard-drivers-{}", std::process::id())),
            package_runner: None,
            data_dir: std::env::temp_dir().join(format!("switchyard-data-{}", std::process::id())),
            swy: None,
        }
    }

    /// Files under the platform directories.
    pub fn from_paths(paths: &AppPaths) -> Self {
        let secrets = match std::env::var("SWITCHYARD_SECRETS").as_deref() {
            Ok("memory") => SecretBackendChoice::Memory,
            Ok("vault") => SecretBackendChoice::Vault(paths.vault_file()),
            _ => SecretBackendChoice::Auto(paths.vault_file()),
        };
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        Self {
            store_path: Some(paths.store_file()),
            secrets,
            extra_drivers: Vec::new(),
            known_hosts: KnownHosts {
                user_file: home.map(|h| PathBuf::from(h).join(".ssh").join("known_hosts")),
                app_file: paths.data.join("known_hosts"),
            },
            drivers_dir: paths.drivers_dir(),
            package_runner: None,
            data_dir: paths.data.clone(),
            swy: None,
        }
    }
}

enum Resume {
    More,
    All,
    Cancel,
}

struct QueryControl {
    cancel: CancelHandle,
    resume: mpsc::UnboundedSender<Resume>,
}

struct SessionInner {
    session: Box<dyn DbSession>,
    txn_statements: u32,
}

struct SessionSlot {
    connection: DbConnection,
    inner: tokio::sync::Mutex<SessionInner>,
    /// Keeps the SSH tunnel open while the session uses it.
    tunnel: Option<Arc<Tunnel>>,
    /// Database and schema switched to with [`Command::SetSessionContext`]; written
    /// while `inner` is locked, readable without waiting for a running query.
    context: Mutex<SessionContext>,
}

/// The core service.
pub struct Service {
    events: EventSender,
    store: Arc<Mutex<Store>>,
    secrets: Arc<dyn SecretStore>,
    vault: Option<Arc<VaultStore>>,
    drivers: HashMap<Engine, Arc<dyn Driver>>,
    sessions: Mutex<HashMap<SessionId, Arc<SessionSlot>>>,
    /// Open Redis key browsers.
    redis: Mutex<HashMap<SessionId, Arc<redis::RedisSlot>>>,
    /// Running assistant runs, to cancel.
    agent_runs: Mutex<HashMap<crate::bus::AgentRunId, switchyard_agents::CancelHandle>>,
    /// Agent commands on Hosts waiting for the user's approval.
    agent_approvals: agent_ssh::Approvals,
    /// Data directory (assistant session tokens).
    data_dir: PathBuf,
    /// `swy` override for assistant runs.
    swy: Option<PathBuf>,
    queries: Mutex<HashMap<QueryId, QueryControl>>,
    terminals: Arc<Terminals>,
    ssh: Arc<SshManager>,
    tunnels: Mutex<Vec<Weak<Tunnel>>>,
    /// Running saved forwards, by (Host id, forward id). Held here: nothing else owns them.
    forwards: Mutex<HashMap<(String, String), Arc<Tunnel>>>,
    /// Serializes tunnel creation so concurrent sessions share one tunnel.
    tunnel_open: tokio::sync::Mutex<()>,
    next_tunnel: std::sync::atomic::AtomicU64,
    prompter: Arc<BusPrompter>,
    entra: EntraSignIn,
    components: Arc<Registry>,
    package_runner: Arc<dyn CommandRunner>,
    files: Arc<crate::files::Files>,
}

/// Run one statement and drain its results (session settings such as `USE`).
async fn run_silently(session: &mut dyn DbSession, sql: &str) -> Result<()> {
    let mut stream = session.execute(sql, &[]).await?;
    while let Some(ev) = stream.next().await {
        ev?;
    }
    Ok(())
}

fn endpoint_of(t: &Tunnel) -> TunnelEndpoint {
    TunnelEndpoint {
        host: t.local().ip().to_string(),
        port: t.local().port(),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Rows fetched per [`Command::FetchMore`] after the limit.
const FETCH_STEP: usize = 10_000;

impl Service {
    /// The secret backend (keychain or vault) the service uses.
    pub fn secret_backend(&self) -> Arc<dyn SecretStore> {
        self.secrets.clone()
    }

    /// Build the service.
    pub fn new(config: ServiceConfig, events: EventSender) -> Result<Self> {
        let store = match &config.store_path {
            Some(p) => Store::open(p)?,
            None => Store::open_in_memory()?,
        };
        let (secrets, vault): (Arc<dyn SecretStore>, Option<Arc<VaultStore>>) =
            match &config.secrets {
                SecretBackendChoice::Memory => (Arc::new(MemoryStore::default()), None),
                SecretBackendChoice::Auto(vault_path) => {
                    if KeychainStore::available() {
                        (Arc::new(KeychainStore), None)
                    } else {
                        let v = Arc::new(VaultStore::new(vault_path));
                        (v.clone(), Some(v))
                    }
                }
                SecretBackendChoice::Vault(vault_path) => {
                    let v = Arc::new(VaultStore::new(vault_path));
                    (v.clone(), Some(v))
                }
            };
        info!(backend = secrets.backend(), "secret backend selected");
        let prompter = BusPrompter::new(events.clone());
        let ssh = Arc::new(SshManager::new(
            config.known_hosts.clone(),
            prompter.clone(),
        ));
        let components = Arc::new(Registry::new(config.drivers_dir.clone()));
        // Debug builds only: try the Driver Manager against a local, unsigned manifest.
        if cfg!(debug_assertions)
            && let Some(path) = std::env::var_os("SWITCHYARD_DEV_MANIFEST")
        {
            match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|b| switchyard_drivers::Manifest::parse(&b).map_err(|e| e.to_string()))
            {
                Ok(m) => components.set_manifest(m),
                Err(e) => warn!(error = %e, "SWITCHYARD_DEV_MANIFEST ignored"),
            }
        }
        if let Ok(Some(mirror)) = store.setting::<String>(MIRROR_SETTING) {
            components.set_mirror(Some(mirror));
        }
        let mut drivers: HashMap<Engine, Arc<dyn Driver>> = HashMap::new();
        drivers.insert(Engine::Postgres, Arc::new(PgDriver));
        drivers.insert(Engine::D1, Arc::new(D1Driver::default()));
        drivers.insert(
            Engine::Sqlite,
            Arc::new(switchyard_db::sqlite::SqliteDriver),
        );
        drivers.insert(
            Engine::Oracle,
            Arc::new(switchyard_db::oracle::OracleDriver),
        );
        drivers.insert(
            Engine::Snowflake,
            Arc::new(switchyard_db::snowflake::SnowflakeDriver::default()),
        );
        drivers.insert(Engine::SqlServer, Arc::new(MssqlDriver));
        drivers.insert(Engine::MySql, Arc::new(switchyard_db::mysql::MySqlDriver));
        drivers.insert(Engine::MongoDb, Arc::new(switchyard_db::mongo::MongoDriver));
        for (engine, d) in config.extra_drivers {
            drivers.insert(engine, d);
        }
        Ok(Self {
            events,
            store: Arc::new(Mutex::new(store)),
            secrets: secrets.clone(),
            vault,
            drivers,
            sessions: Mutex::default(),
            redis: Mutex::default(),
            agent_runs: Mutex::default(),
            agent_approvals: Mutex::default(),
            data_dir: config.data_dir.clone(),
            swy: config.swy.clone(),
            queries: Mutex::default(),
            terminals: Arc::default(),
            ssh,
            tunnels: Mutex::default(),
            forwards: Mutex::default(),
            tunnel_open: tokio::sync::Mutex::new(()),
            next_tunnel: std::sync::atomic::AtomicU64::new(1),
            entra: EntraSignIn::new(prompter.clone(), secrets.clone()),
            components,
            files: Arc::default(),
            package_runner: config
                .package_runner
                .clone()
                .unwrap_or_else(|| Arc::new(crate::components::system_runner())),
            prompter,
        })
    }

    /// Process commands until the channel closes.
    pub async fn run(self: Arc<Self>, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.emit_secret_backend();
        // Tunnel byte counters change without any command; report them once a second
        // while something changed.
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            let mut last = Vec::new();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(this) = weak.upgrade() else { break };
                let now = this.tunnel_infos();
                if now != last {
                    this.emit(Event::Tunnels(now.clone()));
                    last = now;
                }
            }
        });
        while let Some(cmd) = commands.recv().await {
            // A terminal on a Host starts its auto-start forwards, beside the login.
            if let Command::OpenTerminal {
                target: TermTarget::Host(host),
                ..
            } = &cmd
            {
                let (this, host) = (self.clone(), host.clone());
                tokio::spawn(async move { this.auto_start_forwards(&host).await });
            }
            // Keystrokes must reach the program in the order they were typed, so terminal
            // input is delivered here (it never blocks) instead of on a spawned task.
            let cmd = match cmd {
                Command::TerminalInput { term, bytes } => {
                    self.terminals.send(term, TermInput::Data(bytes));
                    continue;
                }
                Command::TerminalResize { term, size } => {
                    self.terminals.send(term, TermInput::Resize(size));
                    continue;
                }
                Command::ReconnectTerminal { term } => {
                    self.terminals.send(term, TermInput::Reconnect);
                    continue;
                }
                Command::AnswerPrompt { request, answer } => {
                    self.prompter.answer(request, answer);
                    continue;
                }
                Command::StartHandoff { data_dir } => {
                    tokio::spawn(crate::handoff::serve(
                        crate::handoff::handoff_file(&data_dir),
                        self.events.clone(),
                        Some(crate::handoff::AgentResponders {
                            command: self.agent_responder(),
                            plan: self.plan_approver(),
                        }),
                    ));
                    continue;
                }
                other => other,
            };
            let this = self.clone();
            tokio::spawn(async move { this.handle(cmd).await });
        }
    }

    fn emit(&self, e: Event) {
        self.events.emit(e);
    }

    fn emit_secret_backend(&self) {
        self.emit(Event::SecretBackend {
            name: self.secrets.backend(),
            locked: self.vault.as_ref().is_some_and(|v| !v.is_unlocked()),
        });
    }

    async fn with_store<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&mut lock(&store)))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .map_err(CoreError::from)
    }

    /// Emit the terminal macros ([`Event::Macros`]).
    async fn emit_macros(&self) {
        match self.with_store(|s| s.macros()).await {
            Ok(list) => self.emit(Event::Macros(list)),
            Err(e) => self.error("Macros", e),
        }
    }

    /// Emit the user's snippets ([`Event::Snippets`]).
    async fn emit_snippets(&self) {
        match self.with_store(|s| s.snippets()).await {
            Ok(list) => self.emit(Event::Snippets(list)),
            Err(e) => self.error("Snippets", e),
        }
    }

    /// Emit the pinned objects ([`Event::Favorites`]).
    async fn emit_favorites(&self) {
        match self.with_store(|s| s.favorites()).await {
            Ok(list) => self.emit(Event::Favorites(list)),
            Err(e) => self.error("Favorites", e),
        }
    }

    async fn with_secrets<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SecretStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T> {
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || f(secrets.as_ref()))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .map_err(CoreError::from)
    }

    async fn emit_profiles(&self) {
        match self.with_store(|s| s.profiles()).await {
            Ok(p) => self.emit(Event::Profiles(p)),
            Err(e) => self.error("Loading profiles", e),
        }
    }

    fn error(&self, context: &str, e: impl std::fmt::Display) {
        warn!(context, error = %e, "command failed");
        self.emit(Event::Error {
            context: context.into(),
            message: e.to_string(),
        });
    }

    async fn handle(&self, cmd: Command) {
        match cmd {
            Command::Ping { id, delay } => {
                tokio::time::sleep(delay).await;
                self.emit(Event::Pong { id });
            }
            Command::LoadProfiles => self.emit_profiles().await,
            Command::SaveProfile {
                request,
                profile,
                secret,
            } => {
                // New user, tenant or method: the next connect asks Microsoft again
                // (the stored refresh token is tried first).
                if let Profile::Db(d) = &profile {
                    self.entra.drop_cached(&d.id);
                }
                self.save_profile(request, profile, secret).await
            }
            Command::DeleteProfile { id } => {
                let removed = self.with_store(move |s| s.delete_profile(&id)).await;
                match removed {
                    Ok(Some(p)) => {
                        self.files.ftp.lock().await.remove(&p.id().0);
                        if let Some(key) = p.secret().cloned() {
                            let _ = self.with_secrets(move |s| s.delete(&key)).await;
                        }
                        if let Profile::Db(d) = &p
                            && d.auth.is_entra()
                        {
                            let _ = self.entra.forget(&d.id).await;
                        }
                        self.emit(Event::Toast(format!("Deleted {}", p.name())));
                    }
                    Ok(None) => {}
                    Err(e) => self.error("Delete", e),
                }
                self.emit_profiles().await;
            }
            Command::DuplicateProfile { id } => {
                match self.duplicate_profile(id).await {
                    Ok(name) => self.emit(Event::Toast(format!("Saved {name}"))),
                    Err(e) => self.error("Duplicate", e),
                }
                self.emit_profiles().await;
            }
            Command::UpdateHosts { ids, patch } => {
                match self.update_hosts(ids, patch).await {
                    Ok(n) => self.emit(Event::Toast(format!(
                        "Updated {n} Host{}",
                        if n == 1 { "" } else { "s" }
                    ))),
                    Err(e) => self.error("Bulk edit", e),
                }
                self.emit_profiles().await;
            }
            Command::ReorderProfiles { ids } => {
                if let Err(e) = self.with_store(move |s| s.reorder(&ids)).await {
                    self.error("Reorder", e);
                }
                self.emit_profiles().await;
            }
            Command::ExportProfiles { path } => {
                let r = self
                    .with_store(move |s| {
                        let json = s.export_json()?;
                        std::fs::write(&path, json)?;
                        Ok(path)
                    })
                    .await;
                match r {
                    Ok(p) => self.emit(Event::Toast(format!(
                        "Exported profiles to {} · no secrets",
                        p.display()
                    ))),
                    Err(e) => self.error("Export", e),
                }
            }
            Command::ImportProfiles { path } => {
                let r = self
                    .with_store(move |s| {
                        let json = std::fs::read_to_string(&path)?;
                        s.import_json(&json)
                    })
                    .await;
                match r {
                    Ok(n) => self.emit(Event::Toast(format!("Imported {n} profiles"))),
                    Err(e) => self.error("Import", e),
                }
                self.emit_profiles().await;
            }
            Command::UnlockVault { password } => {
                if let Some(v) = self.vault.clone() {
                    let r = tokio::task::spawn_blocking(move || v.unlock(&password)).await;
                    match r {
                        Ok(Ok(())) => self.emit(Event::Toast("Vault unlocked".into())),
                        Ok(Err(e)) => self.error("Unlock vault", e),
                        Err(e) => self.error("Unlock vault", e),
                    }
                }
                self.emit_secret_backend();
            }
            Command::TestConnection {
                request,
                connection,
                secret,
            } => {
                let result = self.test_connection(connection, secret).await;
                self.emit(Event::TestResult {
                    request,
                    result: result.map_err(|e| e.to_string()),
                });
            }
            Command::TestFiles {
                request,
                connection,
                secret,
            } => {
                let result = self.test_files(connection, secret).await;
                self.emit(Event::TestResult {
                    request,
                    result: result.map_err(|e| e.to_string()),
                });
            }
            Command::TestHost {
                request,
                host,
                secret,
            } => {
                let result = self.test_host(host, secret).await;
                self.emit(Event::TestResult {
                    request,
                    result: result.map_err(|e| e.to_string()),
                });
            }
            Command::OpenSession {
                session,
                connection,
            } => match self.open_session(session, connection).await {
                Ok(v) => self.emit(Event::SessionOpened {
                    session,
                    server_version: v,
                }),
                Err(e) => self.emit(Event::SessionFailed {
                    session,
                    message: e.to_string(),
                }),
            },
            Command::OpenTerminal { term, target, size } => {
                self.open_terminal(term, target, size).await
            }
            Command::TerminalInput { term, bytes } => {
                self.terminals.send(term, TermInput::Data(bytes));
            }
            Command::TerminalResize { term, size } => {
                self.terminals.send(term, TermInput::Resize(size));
            }
            Command::CloseTerminal { term } => self.terminals.close(term),
            Command::ReconnectTerminal { term } => {
                self.terminals.send(term, TermInput::Reconnect);
            }
            Command::StartTerminalLog { term, settings } => {
                let terminals = self.terminals.clone();
                let dir = self.data_dir.join("terminal-logs");
                let started =
                    tokio::task::spawn_blocking(move || terminals.start_log(term, &settings, &dir))
                        .await;
                let state = match started {
                    Ok(Ok(path)) => crate::bus::TermLogState::Started(path),
                    Ok(Err(message)) => crate::bus::TermLogState::Failed(message),
                    Err(e) => crate::bus::TermLogState::Failed(e.to_string()),
                };
                self.emit(Event::TerminalLog { term, state });
            }
            Command::StopTerminalLog { term } => {
                let terminals = self.terminals.clone();
                let _ = tokio::task::spawn_blocking(move || terminals.stop_log(term)).await;
                self.emit(Event::TerminalLog {
                    term,
                    state: crate::bus::TermLogState::Stopped,
                });
            }
            Command::AnswerPrompt { request, answer } => self.prompter.answer(request, answer),
            Command::AcceptChangedHostKey { host, fingerprint } => {
                self.ssh.accept_changed_key(&host.0, &fingerprint);
            }
            Command::StopTunnel { id } => self.stop_tunnel(id),
            Command::ListTunnels => self.emit(Event::Tunnels(self.tunnel_infos())),
            Command::RunAgent {
                run,
                agent,
                connection,
                prompt,
                resume,
                databases,
            } => {
                self.run_agent(
                    run,
                    agent,
                    connection.filter(|_| databases),
                    databases,
                    prompt,
                    resume,
                )
                .await
            }
            Command::CancelAgent { run } => self.cancel_agent(run),
            Command::OpenAgentTerminal {
                term,
                agent,
                connection,
                size,
            } => {
                self.open_agent_terminal(term, agent, connection, size)
                    .await
            }
            Command::StartForward { host, forward } => {
                if let Err(e) = self.start_forward(&host, &forward).await {
                    self.emit(Event::Error {
                        context: "Port forward".into(),
                        message: e.to_string(),
                    });
                }
            }
            Command::CloseSession { session } => {
                lock(&self.sessions).remove(&session);
                lock(&self.redis).remove(&session);
            }
            Command::RedisOpen {
                session,
                connection,
            } => {
                let result = self.redis_open(session, connection).await;
                self.emit(Event::RedisOpened {
                    session,
                    result: result.map_err(|e| e.to_string()),
                });
            }
            Command::RedisScan {
                session,
                request,
                pattern,
                kind,
                cursor,
            } => {
                self.redis_scan(session, request, pattern, kind, cursor)
                    .await
            }
            Command::RedisLoad {
                session,
                request,
                key,
            } => self.redis_load(session, request, key).await,
            Command::RedisEdit {
                session,
                request,
                key,
                edit,
                create,
            } => self.redis_edit(session, request, key, edit, create).await,
            Command::RedisRun {
                session,
                request,
                line,
                confirmed,
            } => self.redis_run(session, request, line, confirmed).await,
            Command::SetSessionContext {
                session,
                request,
                database,
                schema,
            } => {
                let result = self
                    .set_session_context(session, database, schema)
                    .await
                    .map_err(|e| e.to_string());
                self.emit(Event::SessionContext {
                    session,
                    request,
                    result,
                });
            }
            Command::Execute {
                session,
                query,
                statements,
                tags,
                confirmed_destructive,
                fetch_limit,
            } => {
                let span = info_span!("query", query, session);
                self.execute(
                    session,
                    query,
                    statements,
                    tags,
                    confirmed_destructive,
                    fetch_limit,
                )
                .instrument(span)
                .await
            }
            Command::FetchMore { query, all } => {
                if let Some(q) = lock(&self.queries).get(&query) {
                    let _ = q.resume.send(if all { Resume::All } else { Resume::More });
                }
            }
            Command::Cancel { query } => {
                let ctl = lock(&self.queries)
                    .get(&query)
                    .map(|q| (q.cancel.clone(), q.resume.clone()));
                if let Some((cancel, resume)) = ctl {
                    let _ = resume.send(Resume::Cancel);
                    if let Err(e) = cancel.cancel().await {
                        self.error("Cancel", e);
                    }
                }
            }
            Command::Begin { session } => self.transaction(session, TxnOp::Begin).await,
            Command::Commit { session } => self.transaction(session, TxnOp::Commit).await,
            Command::Rollback { session } => self.transaction(session, TxnOp::Rollback).await,
            Command::Introspect {
                session,
                scope,
                refresh,
            } => self.introspect(session, scope, refresh).await,
            Command::Explain {
                session,
                query,
                sql,
                analyze,
                confirmed,
                tags,
            } => {
                let span = info_span!("explain", query, session, analyze);
                self.explain(session, query, sql, analyze, confirmed, tags)
                    .instrument(span)
                    .await
            }
            Command::LoadPlan {
                request,
                history_id,
            } => self.load_plan(request, history_id).await,
            Command::AgentQuery {
                session,
                query,
                sql,
                row_cap,
                timeout,
                tags,
            } => {
                let span = info_span!("agent_query", query, session);
                self.agent_query(session, query, sql, row_cap, timeout, tags)
                    .instrument(span)
                    .await
            }
            // Started in `run`, which can hand the listener this service.
            Command::StartHandoff { .. } => {}
            Command::AnswerAgentApproval { id, approve } => self.answer_agent_approval(id, approve),
            Command::AgentRedis {
                session,
                request,
                line,
                tags,
            } => self.agent_redis(session, request, line, tags).await,
            Command::RecordAgentCall {
                session,
                summary,
                error,
                tags,
            } => self.record_agent_call(session, summary, error, tags).await,
            Command::Activity {
                session,
                connection,
                request,
            } => self.activity(session, connection, request).await,
            Command::SessionAction {
                session,
                request,
                action,
                target,
                confirmed,
            } => {
                let span = info_span!("session_action", request, session);
                self.session_action(session, request, action, target, confirmed)
                    .instrument(span)
                    .await
            }
            Command::Workload { session, request } => {
                let span = info_span!("workload", request, session);
                self.workload(session, request).instrument(span).await
            }
            Command::WhatIf {
                session,
                query,
                sql,
                indexes,
            } => {
                let span = info_span!("what_if", query, session);
                self.what_if(session, query, sql, indexes)
                    .instrument(span)
                    .await
            }
            Command::LoadSnippets => self.emit_snippets().await,
            Command::SaveSnippet(snippet) => {
                match self.with_store(move |s| s.save_snippet(&snippet)).await {
                    Ok(_) => self.emit_snippets().await,
                    Err(e) => self.error("Snippets", e),
                }
            }
            Command::DeleteSnippet { id } => {
                match self.with_store(move |s| s.delete_snippet(&id)).await {
                    Ok(_) => self.emit_snippets().await,
                    Err(e) => self.error("Snippets", e),
                }
            }
            Command::LoadMacros => self.emit_macros().await,
            Command::SaveMacro(m) => match self.with_store(move |s| s.save_macro(&m)).await {
                Ok(_) => self.emit_macros().await,
                Err(e) => self.error("Macros", e),
            },
            Command::DeleteMacro { id } => {
                match self.with_store(move |s| s.delete_macro(&id)).await {
                    Ok(_) => self.emit_macros().await,
                    Err(e) => self.error("Macros", e),
                }
            }
            Command::LoadFavorites => self.emit_favorites().await,
            Command::AddFavorite(fav) => {
                match self.with_store(move |s| s.add_favorite(&fav)).await {
                    Ok(_) => self.emit_favorites().await,
                    Err(e) => self.error("Favorites", e),
                }
            }
            Command::RemoveFavorite { id } => {
                match self.with_store(move |s| s.remove_favorite(id)).await {
                    Ok(_) => self.emit_favorites().await,
                    Err(e) => self.error("Favorites", e),
                }
            }
            Command::ReorderFavorites { ids } => {
                match self.with_store(move |s| s.reorder_favorites(&ids)).await {
                    Ok(()) => self.emit_favorites().await,
                    Err(e) => self.error("Favorites", e),
                }
            }
            Command::SearchHistory {
                request,
                query,
                connection,
            } => {
                let r = self
                    .with_store(move |s| s.search_history(&query, connection.as_ref(), 500))
                    .await;
                match r {
                    Ok(entries) => self.emit(Event::History { request, entries }),
                    Err(e) => self.error("History", e),
                }
            }
            Command::SaveBuffer { buffer, position } => {
                if let Err(e) = self
                    .with_store(move |s| s.save_buffer(&buffer, position))
                    .await
                {
                    self.error("Autosave", e);
                }
            }
            Command::DeleteBuffer { id } => {
                let _ = self.with_store(move |s| s.delete_buffer(&id)).await;
            }
            Command::LoadWorkspace => match self.with_store(|s| s.workspace()).await {
                Ok(w) => self.emit(Event::Workspace(w)),
                Err(e) => self.error("Workspace", e),
            },
            Command::SaveWorkspace(w) => {
                if let Err(e) = self.with_store(move |s| s.save_workspace(&w)).await {
                    self.error("Workspace", e);
                }
            }
            Command::SetSetting { key, value } => {
                if let Err(e) = self.with_store(move |s| s.set_setting(&key, &value)).await {
                    self.error("Settings", e);
                }
            }
            Command::ListDir { request, fs, path } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, posix)) => {
                        let path = match path {
                            Some(p) => crate::files::expand_path(&p, &f.home(), posix),
                            None => f.home(),
                        };
                        let r = f.list(&path).await.map_err(|e| e.to_string());
                        Ok((path, r))
                    }
                    Err(e) => Err(e.to_string()),
                };
                let (path, result) = match result {
                    Ok((p, r)) => (p, r),
                    Err(e) => (PathBuf::new(), Err(e)),
                };
                self.emit(Event::FsListing {
                    request,
                    fs,
                    path,
                    result,
                });
            }
            Command::Transfer {
                id,
                from,
                path,
                to,
                dir,
                on_conflict,
                resume,
            } => {
                let result = self
                    .transfer(id, &from, &path, &to, dir, on_conflict, resume)
                    .await;
                self.files.finish(id);
                self.emit(Event::TransferDone { id, result });
            }
            Command::SyncFolder {
                id,
                from,
                src,
                to,
                dst,
            } => {
                let result = self.sync_folder(id, &from, &src, &to, &dst).await;
                self.files.finish(id);
                match result {
                    Ok(rep) => self.emit(Event::FolderSynced {
                        id,
                        files_checked: rep.files_checked,
                        files_transferred: rep.files_transferred,
                        bytes_transferred: rep.bytes_transferred,
                        error: None,
                    }),
                    Err(e) => self.emit(Event::FolderSynced {
                        id,
                        files_checked: 0,
                        files_transferred: 0,
                        bytes_transferred: 0,
                        error: Some(format!("{e:?}")),
                    }),
                }
            }
            Command::CancelTransfer { id } => self.files.cancel(id),
            Command::PauseTransfer { id } => self.files.pause(id),
            Command::FsOp { request, fs, op } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, posix)) => match &op {
                        crate::bus::FsOp::Mkdir(p) => f.mkdir(p).await,
                        crate::bus::FsOp::Rename(a, b) => f.rename(a, b).await,
                        crate::bus::FsOp::Delete(p) => {
                            crate::files::delete_tree(f.as_ref(), p, posix).await
                        }
                    }
                    .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                self.emit(Event::FsOpDone {
                    request,
                    fs,
                    result,
                });
            }
            Command::ReadTextFile { request, fs, path } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, _)) => crate::files::read_text(f.as_ref(), &path).await,
                    Err(e) => Err(e.to_string()),
                };
                self.emit(Event::TextFileRead { request, result });
            }
            Command::WriteTextFile {
                request,
                fs,
                path,
                content,
                expect_modified,
                force,
            } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, _)) => {
                        crate::files::write_text(
                            f.as_ref(),
                            &path,
                            &content,
                            expect_modified,
                            force,
                        )
                        .await
                    }
                    Err(e) => Err(crate::bus::SaveError::Failed(e.to_string())),
                };
                self.emit(Event::TextFileSaved { request, result });
            }
            Command::LoadSetting { key } => {
                let k = key.clone();
                let value = self
                    .with_store(move |s| s.setting::<serde_json::Value>(&k))
                    .await
                    .unwrap_or(None);
                self.emit(Event::Setting { key, value });
            }
            Command::ListLocalDir { request, path } => {
                let result = LocalFs.list(&path).await.map_err(|e| e.to_string());
                self.emit(Event::DirListing {
                    request,
                    path,
                    result,
                });
            }
            Command::WriteFile { path, contents } => {
                match tokio::fs::write(&path, contents).await {
                    Ok(()) => self.emit(Event::Toast(format!("Saved {}", path.display()))),
                    Err(e) => self.error("Export", e),
                }
            }
            Command::PreviewSshConfig => match self.preview_ssh_config().await {
                Ok((path, hosts)) => self.emit(Event::SshConfigPreview { path, hosts }),
                Err(e) => self.error("Read ssh config", e),
            },
            Command::ImportSshConfig { only } => match self.import_ssh_config(only).await {
                Ok(0) => self.emit(Event::Toast("No new Hosts found in ~/.ssh/config".into())),
                Ok(n) => {
                    self.emit(Event::Toast(format!(
                        "Imported {n} Hosts from ~/.ssh/config"
                    )));
                    self.emit_profiles().await;
                }
                Err(e) => self.error("Import ssh config", e),
            },
            Command::ApplyEdits {
                session,
                request,
                statements,
            } => {
                let started = Instant::now();
                let result = self
                    .apply_edits(session, statements)
                    .await
                    .map_err(|e| e.to_string());
                self.emit(Event::EditsApplied {
                    request,
                    result,
                    elapsed: started.elapsed(),
                });
            }
            Command::DetectComponents => self.emit_components().await,
            Command::CheckForUpdates { manual } => {
                let status = crate::update::check(&self.data_dir.join("updates")).await;
                self.emit(Event::UpdateStatus { manual, status });
            }
            Command::InstallComponent { id, accept_license } => {
                let r = crate::components::install(
                    &self.components,
                    &self.events,
                    &id,
                    accept_license,
                    None,
                    self.package_runner.as_ref(),
                )
                .await;
                if let Err(e) = r {
                    self.emit(crate::components::failure_event(&id, &e));
                }
                self.emit_components().await;
            }
            Command::InstallComponentFromFile { id, path } => {
                let r = crate::components::install(
                    &self.components,
                    &self.events,
                    &id,
                    true,
                    Some(path),
                    self.package_runner.as_ref(),
                )
                .await;
                if let Err(e) = r {
                    self.emit(crate::components::failure_event(&id, &e));
                }
                self.emit_components().await;
            }
            Command::UseComponentPath { id, path } => {
                let reg = self.components.clone();
                let i = id.clone();
                let r = tokio::task::spawn_blocking(move || reg.set_path(&i, &path))
                    .await
                    .map_err(|e| switchyard_drivers::DriverError::Io(e.to_string()))
                    .and_then(|r| r);
                match r {
                    Ok(component) => self.emit(Event::ComponentInstalled { component }),
                    Err(e) => self.emit(crate::components::failure_event(&id, &e)),
                }
                self.emit_components().await;
            }
            Command::RemoveComponent { id } => {
                let reg = self.components.clone();
                let i = id.clone();
                let r = tokio::task::spawn_blocking(move || reg.remove(&i))
                    .await
                    .map_err(|e| switchyard_drivers::DriverError::Io(e.to_string()))
                    .and_then(|r| r);
                match r {
                    Ok(c) => self.emit(Event::Toast(format!("Removed {}", c.name))),
                    Err(e) => self.emit(crate::components::failure_event(&id, &e)),
                }
                self.emit_components().await;
            }
            Command::SetDriverMirror { url } => {
                let url = url.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty());
                self.components.set_mirror(url.clone());
                let saved = url.clone().unwrap_or_default();
                if let Err(e) = self
                    .with_store(move |s| s.set_setting(MIRROR_SETTING, &saved))
                    .await
                {
                    self.error("Save mirror", e);
                }
                self.emit_components().await;
            }
        }
    }

    /// The file system behind `fs`, and whether its paths are POSIX. SFTP opens once per
    /// Host on the shared SSH session (prompting like a terminal would).
    async fn file_system(
        &self,
        fs: &crate::bus::FsRef,
    ) -> Result<(Arc<dyn switchyard_remote::RemoteFs>, bool)> {
        let id = match fs {
            crate::bus::FsRef::Local => return Ok((Arc::new(LocalFs), cfg!(unix))),
            crate::bus::FsRef::Host(id) => id,
            crate::bus::FsRef::Ftp(id) => return Ok((self.ftp_fs(id).await?, true)),
            crate::bus::FsRef::S3(id) => return Ok((self.s3_fs(id).await?, true)),
        };
        let mut open = self.files.sftp.lock().await;
        if let Some(f) = open.get(&id.0)
            && !f.is_closed()
        {
            return Ok((f.clone(), true));
        }
        let host = self.host(id).await?;
        let target = self.ssh_target(id).await?;
        let conn = self
            .ssh
            .session(&target)
            .await
            .map_err(|e| CoreError::Unsupported(e.to_string()))?;
        let f = Arc::new(
            switchyard_remote::SftpFs::open(conn, host.name.clone())
                .await
                .map_err(|e| CoreError::Unsupported(format!("SFTP on {}: {e}", host.name)))?,
        );
        open.insert(id.0.clone(), f.clone());
        Ok((f, true))
    }

    /// The FTP file system of a saved file connection, logged in once and reused.
    async fn ftp_fs(&self, id: &ProfileId) -> Result<Arc<dyn switchyard_remote::RemoteFs>> {
        let mut open = self.files.ftp.lock().await;
        if let Some(f) = open.get(&id.0) {
            return Ok(f.clone());
        }
        let id2 = id.clone();
        let Some(Profile::File(conn)) = self.with_store(move |s| s.profile(&id2)).await? else {
            return Err(CoreError::NotFound(format!("file connection {}", id.0)));
        };
        let f = Arc::new(self.ftp_connect(&conn, None).await?);
        open.insert(id.0.clone(), f.clone());
        Ok(f)
    }

    /// The S3 file system of a saved file connection, configured and reused.
    async fn s3_fs(&self, id: &ProfileId) -> Result<Arc<dyn switchyard_remote::RemoteFs>> {
        let mut open = self.files.s3.lock().await;
        if let Some(f) = open.get(&id.0) {
            return Ok(f.clone());
        }
        let id2 = id.clone();
        let Some(Profile::File(conn)) = self.with_store(move |s| s.profile(&id2)).await? else {
            return Err(CoreError::NotFound(format!("file connection {}", id.0)));
        };
        let f = Arc::new(self.s3_connect(&conn).await?);
        open.insert(id.0.clone(), f.clone());
        Ok(f)
    }

    /// Construct an S3Fs for an S3 file connection.
    async fn s3_connect(&self, c: &FileConnection) -> Result<switchyard_remote::S3Fs> {
        use secrecy::ExposeSecret as _;
        use switchyard_remote::{AwsCredentials, S3Config, S3Fs, load_aws_profile};
        use switchyard_store::{FileProtocol, S3Auth};

        let FileProtocol::S3 {
            region,
            bucket,
            endpoint,
            force_path_style,
            auth,
        } = &c.protocol
        else {
            return Err(CoreError::Unsupported(format!(
                "{} is not an S3 connection",
                c.name
            )));
        };

        let (creds, resolved_region) = match auth {
            S3Auth::Profile { profile } => {
                let (creds, reg) = load_aws_profile(profile)
                    .map_err(|e| CoreError::Unsupported(e.to_string()))?;
                (creds, reg)
            }
            S3Auth::Credentials {
                access_key_id,
                session_token,
            } => {
                let secret_key = match c.secret.clone() {
                    Some(key) => self
                        .with_secrets(move |s| s.get(&key))
                        .await?
                        .map(|s| s.expose_secret().to_string())
                        .unwrap_or_default(),
                    None => String::new(),
                };
                let creds = AwsCredentials {
                    access_key_id: access_key_id.clone(),
                    secret_access_key: secret_key,
                    session_token: session_token.clone(),
                };
                (creds, None)
            }
        };

        let effective_region = if region.trim().is_empty() {
            resolved_region.unwrap_or_else(|| "us-east-1".to_string())
        } else {
            region.clone()
        };

        let cfg = S3Config {
            region: effective_region,
            bucket: bucket.clone(),
            endpoint: endpoint.clone(),
            force_path_style: *force_path_style,
            credentials: creds,
        };

        Ok(S3Fs::new(cfg))
    }

    /// Log in to an FTP file connection (`secret` overrides the stored password).
    async fn ftp_connect(
        &self,
        c: &FileConnection,
        secret: Option<SecretString>,
    ) -> Result<switchyard_remote::FtpFs> {
        use switchyard_remote::{FtpConfig, FtpDataMode, FtpSecurity};
        use switchyard_store::{FileProtocol, FtpMode, FtpTls};
        let FileProtocol::Ftp {
            server,
            port,
            tls,
            mode,
            user,
        } = &c.protocol
        else {
            return Err(CoreError::Unsupported(format!(
                "{} is not an FTP connection",
                c.name
            )));
        };
        let password = match secret {
            Some(s) => Some(s),
            None => match c.secret.clone() {
                Some(key) => self.with_secrets(move |s| s.get(&key)).await?,
                None => None,
            },
        };
        let mut cfg = FtpConfig::new(
            server.trim(),
            *port,
            user.trim(),
            password.unwrap_or_else(|| SecretString::from(String::new())),
        );
        cfg.security = match tls {
            FtpTls::None => FtpSecurity::None,
            FtpTls::Explicit => FtpSecurity::Explicit,
            FtpTls::Implicit => FtpSecurity::Implicit,
        };
        cfg.mode = match mode {
            FtpMode::Passive => FtpDataMode::Passive,
            FtpMode::Active => FtpDataMode::Active,
        };
        let mut fs = switchyard_remote::FtpFs::connect(cfg, c.name.clone())
            .await
            .map_err(|e| CoreError::Unsupported(format!("FTP {}: {e}", c.name)))?;
        if let Some(dir) = &c.default_path {
            fs.set_home(dir);
        }
        Ok(fs)
    }

    async fn test_files(&self, c: FileConnection, secret: Option<SecretString>) -> Result<String> {
        use switchyard_store::FileProtocol;
        let started = Instant::now();
        match &c.protocol {
            FileProtocol::S3 { .. } => {
                let fs = self.s3_connect(&c).await?;
                let listed = fs
                    .list(&fs.home())
                    .await
                    .map_err(|e| CoreError::Unsupported(format!("Listing S3 {}: {e}", fs.home().display())))?;
                Ok(format!(
                    "Connected to S3 · {} entries in {} · {} ms",
                    listed.len(),
                    fs.home().display(),
                    started.elapsed().as_millis()
                ))
            }
            FileProtocol::Ftp { .. } => {
                let fs = self.ftp_connect(&c, secret).await?;
                let listed = fs
                    .list(&fs.home())
                    .await
                    .map_err(|e| CoreError::Unsupported(format!("Listing {}: {e}", fs.home().display())))?;
                Ok(format!(
                    "Connected · {} · {} entries in {} · {} ms",
                    fs.describe(),
                    listed.len(),
                    fs.home().display(),
                    started.elapsed().as_millis()
                ))
            }
            FileProtocol::Sftp { .. } => Err(CoreError::Unsupported("SFTP tested via Host".into())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn transfer(
        &self,
        id: u64,
        from: &crate::bus::FsRef,
        path: &std::path::Path,
        to: &crate::bus::FsRef,
        dir: Option<PathBuf>,
        on_conflict: crate::bus::OnConflict,
        resume: bool,
    ) -> std::result::Result<PathBuf, crate::bus::TransferError> {
        use crate::bus::TransferError;
        // Pause / cancel work while waiting for a slot too.
        let control = self.files.start(id);
        let slot = match self.files.slots.clone().try_acquire_owned() {
            Ok(s) => s,
            Err(_) => {
                self.emit(Event::TransferQueued { id });
                self.files
                    .slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| TransferError::Failed(e.to_string()))?
            }
        };
        let _slot = slot;
        match control.load(std::sync::atomic::Ordering::SeqCst) {
            1 => return Err(TransferError::Cancelled),
            2 => return Err(TransferError::Paused),
            _ => {}
        }
        let fail = |e: CoreError| TransferError::Failed(e.to_string());
        let (src, src_posix) = self.file_system(from).await.map_err(fail)?;
        let (dst, dst_posix) = self.file_system(to).await.map_err(fail)?;
        let dir = match dir {
            Some(d) => d,
            None => {
                let d = LocalFs.home().join("Downloads");
                tokio::fs::create_dir_all(&d)
                    .await
                    .map_err(|e| TransferError::Failed(e.to_string()))?;
                d
            }
        };
        let name = switchyard_remote::fs::file_name(path);
        let events = self.events.clone();
        let progress = move |done: u64, total: Option<u64>| {
            events.emit(Event::TransferProgress {
                id,
                name: name.clone(),
                done,
                total,
            })
        };
        crate::files::transfer(
            src.as_ref(),
            src_posix,
            path,
            dst.as_ref(),
            dst_posix,
            &dir,
            on_conflict,
            resume,
            &control,
            &progress,
        )
        .await
    }

    async fn sync_folder(
        &self,
        id: u64,
        from: &crate::bus::FsRef,
        src_path: &std::path::Path,
        to: &crate::bus::FsRef,
        dst_path: &std::path::Path,
    ) -> std::result::Result<crate::files::SyncReport, crate::bus::TransferError> {
        let (src, src_posix) = self
            .file_system(from)
            .await
            .map_err(|e| crate::bus::TransferError::Failed(e.to_string()))?;
        let (dst, dst_posix) = self
            .file_system(to)
            .await
            .map_err(|e| crate::bus::TransferError::Failed(e.to_string()))?;

        let control = self.files.start(id);
        let name = switchyard_remote::fs::file_name(src_path);
        let events = self.events.clone();
        let progress = move |done: u64, total: Option<u64>| {
            events.emit(Event::TransferProgress {
                id,
                name: name.clone(),
                done,
                total,
            });
        };

        crate::files::sync_folder(
            src.as_ref(),
            src_posix,
            src_path,
            dst.as_ref(),
            dst_posix,
            dst_path,
            &control,
            &progress,
        )
        .await
    }

    async fn emit_components(&self) {
        let reg = self.components.clone();
        let comps = tokio::task::spawn_blocking(move || reg.components())
            .await
            .unwrap_or_default();
        self.emit(Event::Components(comps));
    }

    async fn read_ssh_config(&self) -> Result<(PathBuf, Vec<switchyard_remote::SshConfigHost>)> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| CoreError::NotFound("home directory".into()))?;
        let path = PathBuf::from(home).join(".ssh").join("config");
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| CoreError::Store(StoreError::Io(e)))?;
        Ok((path, switchyard_remote::parse_ssh_config(&text)))
    }

    fn default_ssh_user() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "root".into())
    }

    async fn saved_host_names(&self) -> Result<HashMap<String, ProfileId>> {
        self.with_store(|s| {
            Ok(s.profiles()?
                .into_iter()
                .filter_map(|p| match p {
                    Profile::Host(h) => Some((h.name, h.id)),
                    _ => None,
                })
                .collect())
        })
        .await
    }

    async fn preview_ssh_config(
        &self,
    ) -> Result<(PathBuf, Vec<crate::ssh_import::SshImportCandidate>)> {
        let (path, parsed) = self.read_ssh_config().await?;
        let saved: std::collections::HashSet<String> =
            self.saved_host_names().await?.into_keys().collect();
        let hosts = crate::ssh_import::candidates(&parsed, &saved, &Self::default_ssh_user());
        Ok((path, hosts))
    }

    async fn import_ssh_config(&self, only: Option<Vec<String>>) -> Result<usize> {
        let (_, parsed) = self.read_ssh_config().await?;
        let saved = self.saved_host_names().await?;
        let only: Option<std::collections::HashSet<String>> = only.map(|o| o.into_iter().collect());
        let hosts =
            crate::ssh_import::plan(&parsed, &saved, only.as_ref(), &Self::default_ssh_user());
        self.with_store(move |s| {
            for h in &hosts {
                s.save_profile(&Profile::Host(h.clone()))?;
            }
            Ok(hosts.len())
        })
        .await
    }

    async fn apply_edits(&self, session: SessionId, statements: Vec<String>) -> Result<u64> {
        let slot = self
            .slot(session)
            .ok_or_else(|| CoreError::NotFound("session".into()))?;
        if slot.connection.read_only {
            return Err(CoreError::Unsupported(
                "this connection is locked read-only".into(),
            ));
        }
        let mut inner = slot.inner.lock().await;
        let mut own_txn = !inner.session.in_transaction();
        if own_txn {
            match inner.session.begin().await {
                Ok(()) => {}
                // No transactions (MongoDB here): apply one statement at a time and say
                // how many went through if one fails.
                Err(switchyard_db::DbError::Unsupported(_)) => own_txn = false,
                Err(e) => return Err(e.into()),
            }
        }
        let atomic = own_txn || inner.session.in_transaction();
        let partial = |total: u64, msg: String| {
            if atomic || total == 0 {
                msg
            } else {
                format!(
                    "{msg} ({total} earlier change{} already saved)",
                    if total == 1 { " was" } else { "s were" }
                )
            }
        };
        let mut total = 0u64;
        for sql in &statements {
            let outcome: Result<u64> = async {
                let mut stream = inner.session.execute(sql, &[]).await?;
                let mut affected = 0;
                while let Some(ev) = stream.next().await {
                    if let ResultEvent::Done(c) = ev? {
                        affected = c.affected.unwrap_or(0);
                    }
                }
                Ok(affected)
            }
            .await;
            match outcome {
                Ok(1) => total += 1,
                Ok(n) => {
                    if own_txn {
                        let _ = inner.session.rollback().await;
                    }
                    let msg = if atomic {
                        format!("expected to change 1 row but changed {n}; nothing was saved")
                    } else {
                        format!("expected to change 1 row but changed {n}")
                    };
                    return Err(CoreError::Unsupported(partial(total, msg)));
                }
                Err(e) => {
                    if own_txn {
                        let _ = inner.session.rollback().await;
                    }
                    if !atomic && total > 0 {
                        return Err(CoreError::Unsupported(partial(total, e.to_string())));
                    }
                    return Err(e);
                }
            }
        }
        if own_txn {
            inner.session.commit().await?;
        }
        let conn = slot.connection.clone();
        for sql in &statements {
            let st = StatementRequest {
                sql: sql.clone(),
                params: vec![],
                offset: 0,
            };
            self.record(
                &conn,
                &st,
                now_ms(),
                Duration::ZERO,
                0,
                &(HistoryStatus::Ok, None, Some(1)),
                &["edit".into()],
            )
            .await;
        }
        Ok(total)
    }

    async fn save_profile(
        &self,
        request: RequestId,
        mut profile: Profile,
        secret: Option<SecretString>,
    ) {
        if let Some(secret) = secret {
            let purpose = match &profile {
                Profile::Host(_) => "passphrase",
                _ => "password",
            };
            let key = SecretRef::for_profile(profile.id(), purpose);
            let k = key.clone();
            if let Err(e) = self.with_secrets(move |s| s.set(&k, &secret)).await {
                self.emit(Event::ProfileError {
                    request,
                    field: None,
                    message: e.to_string(),
                });
                self.emit_secret_backend();
                return;
            }
            match &mut profile {
                Profile::Host(h) => h.secret = Some(key),
                Profile::Db(d) => d.secret = Some(key),
                Profile::File(f) => f.secret = Some(key),
                Profile::Terminal(_) => {}
            }
        }
        let id = profile.id().clone();
        match self.with_store(move |s| s.save_profile(&profile)).await {
            Ok(()) => {
                // An edited FTP connection logs in again with its new settings.
                self.files.ftp.lock().await.remove(&id.0);
                self.emit(Event::ProfileSaved { request, id })
            }
            Err(CoreError::Store(StoreError::Validation(v))) => self.emit(Event::ProfileError {
                request,
                field: Some(v.field),
                message: v.message,
            }),
            Err(e) => self.emit(Event::ProfileError {
                request,
                field: None,
                message: e.to_string(),
            }),
        }
        self.emit_profiles().await;
    }

    async fn open_terminal(&self, term: TermId, target: TermTarget, size: TermSize) {
        let result = match target {
            TermTarget::Local { profile } => {
                let profile = match profile {
                    Some(id) => match self.with_store(move |s| s.profile(&id)).await {
                        Ok(Some(Profile::Terminal(t))) => Some(t),
                        _ => None,
                    },
                    None => None,
                };
                let shell = LocalShell {
                    program: profile
                        .as_ref()
                        .map(|t| t.shell.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                    env: profile.as_ref().map(|t| t.env.clone()).unwrap_or_default(),
                    ..LocalShell::default()
                };
                let description = shell
                    .program
                    .clone()
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "login shell".into());
                let startup = profile.and_then(|t| t.startup_command);
                self.terminals
                    .open_local(
                        term,
                        shell,
                        size,
                        switchyard_term::DEFAULT_SCROLLBACK,
                        self.events.clone(),
                        Vec::new(),
                    )
                    .map(|t| {
                        if let Some(cmd) = startup {
                            self.terminals
                                .send(term, TermInput::Data(format!("{cmd}\r").into_bytes()));
                        }
                        (t, description)
                    })
                    .map_err(|e| e.to_string())
            }
            TermTarget::Host(host_id) => match self.ssh_target(&host_id).await {
                Ok(target) => {
                    let (startup, env) = self.shell_startup(&host_id).await;
                    let description = format!("{}@{}", target.user, target.address);
                    let terminal = self.terminals.open_ssh(
                        SshTerminalSpec {
                            term,
                            host_id,
                            target,
                            size,
                            startup,
                            env,
                        },
                        self.ssh.clone(),
                        self.events.clone(),
                    );
                    Ok((terminal, description))
                }
                Err(e) => Err(e.to_string()),
            },
        };
        match result {
            Ok((terminal, description)) => self.emit(Event::TerminalOpened {
                term,
                terminal,
                description,
            }),
            Err(message) => self.emit(Event::TerminalFailed { term, message }),
        }
    }

    /// The SSH target for a Host, with its jump chain and keychain secrets.
    pub(crate) async fn ssh_target(&self, id: &ProfileId) -> Result<SshTarget> {
        let host = self.host(id).await?;
        let mut chain = Vec::new();
        for jid in host.jump_hosts.iter().take(8) {
            if jid == id {
                return Err(CoreError::Unsupported(
                    "a Host cannot jump through itself".into(),
                ));
            }
            chain.push(self.host(jid).await?);
        }
        let mut jump: Option<Box<SshTarget>> = None;
        for h in chain {
            let mut t = self.one_target(&h).await?;
            t.jump = jump.take();
            jump = Some(Box::new(t));
        }
        let mut t = self.one_target(&host).await?;
        t.jump = jump;
        Ok(t)
    }

    async fn test_host(&self, host: Host, secret: Option<SecretString>) -> Result<String> {
        let mut target = self.one_target(&host).await?;
        if secret.is_some() {
            target.secret = secret;
        }
        let mut jump: Option<Box<SshTarget>> = None;
        for jid in host.jump_hosts.iter().take(8) {
            let mut t = self.one_target(&self.host(jid).await?).await?;
            t.jump = jump.take();
            jump = Some(Box::new(t));
        }
        target.jump = jump;
        // A throwaway id: the test never reuses or replaces a live session.
        target.id = format!("test:{}", host.id.0);
        let conn = self
            .ssh
            .session(&target)
            .await
            .map_err(|e| CoreError::Unsupported(e.to_string()))?;
        Ok(format!("Connected · {}", conn.description))
    }

    /// What to type into each new shell on a Host (start folder, startup command,
    /// connect macro) and the environment variables to send.
    async fn shell_startup(&self, id: &ProfileId) -> (Vec<u8>, Vec<(String, String)>) {
        let Ok(host) = self.host(id).await else {
            return (Vec::new(), Vec::new());
        };
        let input = match host.connect_macro.clone() {
            Some(mid) => match self.with_store(|s| s.macros()).await {
                Ok(list) => list.into_iter().find(|m| m.id == mid).map(|m| m.input),
                Err(e) => {
                    warn!(error = %e, "connect macro not loaded");
                    None
                }
            },
            None => None,
        };
        let startup = crate::terminals::shell_startup(
            host.start_directory.as_deref(),
            host.startup_command.as_deref(),
            input.as_deref(),
        );
        (startup, host.env)
    }

    /// Save a copy of a profile under a new id, with a copy of its stored secret.
    async fn duplicate_profile(&self, id: ProfileId) -> Result<String> {
        let id2 = id.clone();
        let Some(profile) = self.with_store(move |s| s.profile(&id2)).await? else {
            return Err(CoreError::NotFound(format!("profile {}", id.0)));
        };
        let old_secret = profile.secret().cloned();
        let mut copy = match profile {
            Profile::Host(h) => Profile::Host(h.duplicate()),
            Profile::Db(mut d) => {
                d.id = ProfileId::new();
                d.name = format!("{} copy", d.name);
                d.secret = None;
                Profile::Db(d)
            }
            Profile::File(mut f) => {
                f.id = ProfileId::new();
                f.name = format!("{} copy", f.name);
                f.secret = None;
                Profile::File(f)
            }
            Profile::Terminal(mut t) => {
                t.id = ProfileId::new();
                t.name = format!("{} copy", t.name);
                Profile::Terminal(t)
            }
        };
        if let Some(old) = old_secret
            && let Some(value) = self.with_secrets(move |s| s.get(&old)).await?
        {
            let purpose = if matches!(copy, Profile::Host(_)) {
                "passphrase"
            } else {
                "password"
            };
            let key = SecretRef::for_profile(copy.id(), purpose);
            let k = key.clone();
            self.with_secrets(move |s| s.set(&k, &value)).await?;
            match &mut copy {
                Profile::Host(h) => h.secret = Some(key),
                Profile::Db(d) => d.secret = Some(key),
                Profile::File(f) => f.secret = Some(key),
                Profile::Terminal(_) => {}
            }
        }
        let name = copy.name().to_owned();
        self.with_store(move |s| s.save_profile(&copy)).await?;
        Ok(name)
    }

    /// Apply one change to several Hosts.
    async fn update_hosts(&self, ids: Vec<ProfileId>, patch: HostPatch) -> Result<usize> {
        self.with_store(move |s| {
            let mut n = 0;
            for id in &ids {
                if let Some(Profile::Host(mut h)) = s.profile(id)? {
                    patch.apply(&mut h);
                    s.save_profile(&Profile::Host(h))?;
                    n += 1;
                }
            }
            Ok(n)
        })
        .await
    }

    async fn host(&self, id: &ProfileId) -> Result<Host> {
        let id2 = id.clone();
        match self.with_store(move |s| s.profile(&id2)).await? {
            Some(Profile::Host(h)) => Ok(h),
            _ => Err(CoreError::NotFound(format!("Host {}", id.0))),
        }
    }

    async fn one_target(&self, h: &Host) -> Result<SshTarget> {
        let secret = match h.secret.clone() {
            Some(key) => self.with_secrets(move |s| s.get(&key)).await?,
            None => None,
        };
        Ok(SshTarget {
            id: h.id.0.clone(),
            label: h.name.clone(),
            address: h.address.clone(),
            port: h.port,
            user: h.user.clone(),
            auth: match &h.auth {
                SshAuth::Password => SshAuthMethod::Password,
                SshAuth::PublicKey { key_path } => SshAuthMethod::PublicKey {
                    key_path: key_path.clone(),
                },
                SshAuth::KeyboardInteractive => SshAuthMethod::KeyboardInteractive,
                SshAuth::Agent => SshAuthMethod::Agent,
            },
            secret,
            keepalive: Duration::from_secs(u64::from(h.keepalive_secs)),
            jump: None,
            agent_socket: h.identity_agent.clone(),
            agent_key: h.agent_key.clone(),
            forward_agent: h.forward_agent,
            forward_x11: h.forward_x11,
            x11_display: h.x11_display.clone(),
        })
    }

    fn tunnel_infos(&self) -> Vec<TunnelInfo> {
        let mut list = lock(&self.tunnels);
        list.retain(|w| w.strong_count() > 0);
        list.iter()
            .filter_map(Weak::upgrade)
            .filter(|t| !t.is_stopped())
            .map(|t| t.info())
            .collect()
    }

    /// The live tunnel to `host:port` through Host `host_id`, or a new one. Tunnels are
    /// shared by every session that needs the same target.
    async fn tunnel_for(&self, host_id: &ProfileId, host: &str, port: u16) -> Result<Arc<Tunnel>> {
        let _creating = self.tunnel_open.lock().await;
        let existing = lock(&self.tunnels)
            .iter()
            .filter_map(Weak::upgrade)
            .find(|t| !t.is_stopped() && t.host_id() == host_id.0 && t.remote() == (host, port));
        if let Some(t) = existing {
            return Ok(t);
        }
        let target = self.ssh_target(host_id).await?;
        let id = self
            .next_tunnel
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = Arc::new(
            Tunnel::open(id, self.ssh.clone(), target, host.to_owned(), port)
                .await
                .map_err(|e| CoreError::Unsupported(e.to_string()))?,
        );
        lock(&self.tunnels).push(Arc::downgrade(&t));
        self.emit(Event::Tunnels(self.tunnel_infos()));
        Ok(t)
    }

    /// The endpoint a connection should use: a tunnel when it goes through a Host.
    async fn endpoint(&self, c: &DbConnection) -> Result<Option<Arc<Tunnel>>> {
        match &c.via_host {
            Some(h) => Ok(Some(self.tunnel_for(h, &c.server, c.port).await?)),
            None => Ok(None),
        }
    }

    /// Start a Host's saved forward, unless it is running.
    async fn start_forward(&self, host_id: &ProfileId, forward_id: &str) -> Result<()> {
        let key = (host_id.0.clone(), forward_id.to_owned());
        let _creating = self.tunnel_open.lock().await;
        if lock(&self.forwards)
            .get(&key)
            .is_some_and(|t| !t.is_stopped())
        {
            return Ok(());
        }
        let host = self.host(host_id).await?;
        let f = host
            .forwards
            .iter()
            .find(|f| f.id == forward_id)
            .ok_or_else(|| CoreError::NotFound(format!("port forward on {}", host.name)))?;
        f.validate()
            .map_err(|e| CoreError::Unsupported(format!("{}: {e}", f.summary())))?;
        let spec = forward_spec(f);
        let target = self.ssh_target(host_id).await?;
        let id = self
            .next_tunnel
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = Arc::new(
            Tunnel::start(id, Some(f.id.clone()), self.ssh.clone(), target, spec)
                .await
                .map_err(|e| CoreError::Unsupported(format!("{}: {e}", f.summary())))?,
        );
        lock(&self.tunnels).push(Arc::downgrade(&t));
        lock(&self.forwards).insert(key, t);
        self.emit(Event::Tunnels(self.tunnel_infos()));
        Ok(())
    }

    /// Start the Host's auto-start forwards that are not running (when it connects).
    async fn auto_start_forwards(&self, host_id: &ProfileId) {
        let Ok(host) = self.host(host_id).await else {
            return;
        };
        for f in host.forwards.iter().filter(|f| f.auto_start) {
            if let Err(e) = self.start_forward(host_id, &f.id).await {
                self.emit(Event::Error {
                    context: "Port forward".into(),
                    message: e.to_string(),
                });
            }
        }
    }

    /// Stop a tunnel; sessions that used it end with a message saying why.
    fn stop_tunnel(&self, id: u64) {
        lock(&self.forwards).retain(|_, t| t.id() != id);
        let tunnel = lock(&self.tunnels)
            .iter()
            .filter_map(Weak::upgrade)
            .find(|t| t.id() == id);
        let Some(t) = tunnel else { return };
        t.stop();
        let message = format!(
            "The tunnel through {} on port {} was stopped · reconnect to continue",
            t.info().host,
            t.local().port()
        );
        let ended: Vec<SessionId> = lock(&self.sessions)
            .iter()
            .filter(|(_, slot)| slot.tunnel.as_ref().is_some_and(|s| s.id() == id))
            .map(|(id, _)| *id)
            .collect();
        for session in ended {
            lock(&self.sessions).remove(&session);
            self.emit(Event::SessionFailed {
                session,
                message: message.clone(),
            });
        }
        for session in self.end_redis_on_tunnel(id) {
            self.emit(Event::RedisOpened {
                session,
                result: Err(message.clone()),
            });
        }
        self.emit(Event::Tunnels(self.tunnel_infos()));
    }

    async fn db_config(&self, c: &DbConnection, secret: Option<SecretString>) -> Result<DbConfig> {
        let password = match secret {
            Some(s) => Some(s),
            None => match c.secret.clone() {
                Some(key) => self.with_secrets(move |s| s.get(&key)).await?,
                None => None,
            },
        };
        let mut cfg = DbConfig::new(c.engine, c.server.clone(), c.database.clone());
        cfg.port = c.port;
        cfg.user = c.user.clone();
        cfg.password = password;
        cfg.auth = c.auth;
        if c.auth.is_entra() {
            cfg.access_token = Some(self.entra.token(c, cfg.password.clone()).await?);
        }
        // Windows signs in with SSPI inside the driver; elsewhere Kerberos needs GSSAPI.
        if c.auth == DbAuthMethod::Integrated && c.engine == Engine::SqlServer && !cfg!(windows) {
            let k = crate::components::kerberos(&self.components).await?;
            cfg.security = Some(Arc::new(k));
        }
        cfg.ssl_mode = c.ssl_mode;
        cfg.read_only = c.read_only;
        cfg.options = c.options.clone();
        if c.engine == Engine::Oracle {
            // ODPI-C loads the client from the Driver Manager's folder; without one it
            // searches the usual places and says what is missing.
            let reg = self.components.clone();
            let dir = tokio::task::spawn_blocking(move || reg.oracle_client()).await;
            if let Ok(Ok(dir)) = dir {
                cfg.options
                    .insert("client_lib_dir".into(), dir.display().to_string());
            }
        }
        Ok(cfg)
    }

    fn driver(&self, engine: Engine) -> Result<Arc<dyn Driver>> {
        self.drivers.get(&engine).cloned().ok_or_else(|| {
            CoreError::Unsupported(format!(
                "{} driver is not available yet",
                engine.display_name()
            ))
        })
    }

    async fn test_connection(
        &self,
        c: DbConnection,
        secret: Option<SecretString>,
    ) -> Result<String> {
        if c.engine == Engine::Redis {
            let started = Instant::now();
            let (client, tunnel) = self.redis_connect(&c, secret).await?;
            let ms = started.elapsed().as_millis();
            let via = if tunnel.is_some() { " via tunnel" } else { "" };
            return Ok(format!(
                "Connected · {} · db {} · {ms} ms{via}",
                client.server_version(),
                client.db()
            ));
        }
        let driver = self.driver(c.engine)?;
        let cfg = self.db_config(&c, secret).await?;
        let tunnel = self.endpoint(&c).await?;
        let started = Instant::now();
        let session = driver
            .connect(&cfg, tunnel.as_deref().map(endpoint_of))
            .await?;
        let ms = started.elapsed().as_millis();
        let via = if tunnel.is_some() { " via tunnel" } else { "" };
        Ok(format!(
            "Connected · {} · {ms} ms{via}",
            session.server_version()
        ))
    }

    async fn open_session(&self, session: SessionId, id: ProfileId) -> Result<String> {
        let profile = self.with_store(move |s| s.profile(&id)).await?;
        let Some(Profile::Db(conn)) = profile else {
            return Err(CoreError::NotFound("connection".into()));
        };
        let driver = self.driver(conn.engine)?;
        let cfg = self.db_config(&conn, None).await?;
        let tunnel = self.endpoint(&conn).await?;
        let s = driver
            .connect(&cfg, tunnel.as_deref().map(endpoint_of))
            .await?;
        let version = s.server_version();
        lock(&self.sessions).insert(
            session,
            Arc::new(SessionSlot {
                connection: conn,
                inner: tokio::sync::Mutex::new(SessionInner {
                    session: s,
                    txn_statements: 0,
                }),
                context: Mutex::new(SessionContext::default()),
                tunnel,
            }),
        );
        Ok(version)
    }

    /// Make `database` and/or `schema` current in an open session: the dialect's `USE`
    /// statement, or a new connection to the other database on the same tunnel.
    async fn set_session_context(
        &self,
        session: SessionId,
        database: Option<String>,
        schema: Option<String>,
    ) -> Result<SessionContext> {
        let slot = self
            .slot(session)
            .ok_or_else(|| CoreError::NotFound("session".into()))?;
        let driver = self.driver(slot.connection.engine)?;
        let dialect = driver.dialect();
        let mut inner = slot.inner.lock().await;
        if inner.session.in_transaction() {
            return Err(CoreError::Unsupported(
                "Commit or roll back the open transaction before switching".into(),
            ));
        }
        if let Some(db) = database {
            match dialect.use_database(&db) {
                Some(sql) => run_silently(inner.session.as_mut(), &sql).await?,
                None => {
                    let mut cfg = self.db_config(&slot.connection, None).await?;
                    cfg.database = db.clone();
                    let s = driver
                        .connect(&cfg, slot.tunnel.as_deref().map(endpoint_of))
                        .await?;
                    inner.session = s;
                    inner.txn_statements = 0;
                }
            }
            info!(session, "switched database");
            *lock(&slot.context) = SessionContext {
                database: Some(db),
                schema: None,
            };
        }
        if let Some(schema) = schema {
            let sql = dialect.use_schema(&schema).ok_or_else(|| {
                CoreError::Unsupported(format!(
                    "{} cannot switch schemas per session",
                    slot.connection.engine.display_name()
                ))
            })?;
            run_silently(inner.session.as_mut(), &sql).await?;
            lock(&slot.context).schema = Some(schema);
        }
        Ok(lock(&slot.context).clone())
    }

    fn slot(&self, session: SessionId) -> Option<Arc<SessionSlot>> {
        lock(&self.sessions).get(&session).cloned()
    }

    async fn transaction(&self, session: SessionId, op: TxnOp) {
        let Some(slot) = self.slot(session) else {
            return self.error("Transaction", "session is not open");
        };
        let mut inner = slot.inner.lock().await;
        let r = match op {
            TxnOp::Begin => inner.session.begin().await,
            TxnOp::Commit => inner.session.commit().await,
            TxnOp::Rollback => inner.session.rollback().await,
        };
        if let Err(e) = r {
            self.error("Transaction", e);
        }
        inner.txn_statements = 0;
        let open = inner.session.in_transaction();
        self.emit(Event::Transaction {
            session,
            open,
            statements: 0,
        });
        if matches!(op, TxnOp::Commit) {
            self.emit(Event::Toast("Committed".into()));
        } else if matches!(op, TxnOp::Rollback) {
            self.emit(Event::Toast("Rolled back".into()));
        }
    }

    async fn introspect(&self, session: SessionId, scope: IntrospectScope, refresh: bool) {
        let Some(slot) = self.slot(session) else {
            return self.emit(Event::Catalog {
                session,
                scope,
                result: Err("session is not open".into()),
                cached_at: now_ms(),
            });
        };
        let conn_id = slot.connection.id.clone();
        let key = scope.cache_key();
        // A session switched to another database caches apart from the profile's default.
        let key = match lock(&slot.context).database.as_deref() {
            Some(db) => format!("db={db};{key}"),
            None => key,
        };
        // Search results are per keystroke: never read from or written to the schema cache.
        let cacheable = scope.is_cacheable();
        if !refresh && cacheable {
            let (c, k) = (conn_id.clone(), key.clone());
            if let Ok(Some((chunk, at))) = self.with_store(move |s| s.cached_schema(&c, &k)).await {
                return self.emit(Event::Catalog {
                    session,
                    scope,
                    result: Ok(chunk),
                    cached_at: at,
                });
            }
        }
        let result = {
            let mut inner = slot.inner.lock().await;
            inner.session.introspect(scope.clone()).await
        };
        // A hint (missing privilege) is never cached: the next read tries again.
        if let Ok(chunk) = &result
            && cacheable
            && !matches!(chunk, CatalogChunk::Hint(_))
        {
            let chunk = chunk.clone();
            let _ = self
                .with_store(move |s| s.cache_schema(&conn_id, &key, &chunk))
                .await;
        }
        self.emit(Event::Catalog {
            session,
            scope,
            result: result.map_err(|e| e.to_string()),
            cached_at: now_ms(),
        });
    }

    fn query_event(&self, query: QueryId, event: QueryEvent) {
        self.emit(Event::Query { query, event });
    }

    fn plan_failed(&self, request: u64, error: impl std::fmt::Display, needs_confirmation: bool) {
        self.emit(Event::PlanFailed {
            request,
            error: error.to_string(),
            needs_confirmation,
        });
    }

    /// Findings thresholds from Settings (`plan.thresholds`), else the defaults.
    async fn thresholds(&self) -> switchyard_plan::Thresholds {
        self.with_store(|s| s.setting::<switchyard_plan::Thresholds>("plan.thresholds"))
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    async fn explain(
        &self,
        session: SessionId,
        query: QueryId,
        sql: String,
        analyze: bool,
        confirmed: bool,
        mut tags: Vec<String>,
    ) {
        let Some(slot) = self.slot(session) else {
            return self.plan_failed(query, DbError::Closed, false);
        };
        let conn = slot.connection.clone();
        let dialect = dialect_for(conn.engine);
        // An actual plan executes the statement (then rolls it back). Writes are refused on
        // read-only connections and need confirmation on Production.
        if analyze && !guard::classify(dialect, &sql).is_read_only() {
            if conn.read_only {
                return self.plan_failed(
                    query,
                    "this connection is locked read-only; an actual plan would run a write \
                     (it is rolled back, but triggers and sequences still fire). Use Explain.",
                    false,
                );
            }
            if conn.environment.is_production() && !confirmed {
                return self.plan_failed(
                    query,
                    "an actual plan runs this writing statement on Production (then rolls it back)",
                    true,
                );
            }
        }
        let mut inner = slot.inner.lock().await;
        let (resume_tx, _resume_rx) = mpsc::unbounded_channel();
        lock(&self.queries).insert(
            query,
            QueryControl {
                cancel: inner.session.cancel_handle(),
                resume: resume_tx,
            },
        );
        let started = Instant::now();
        let started_at = now_ms();
        let mode = if analyze {
            switchyard_plan::capture::Mode::Actual
        } else {
            switchyard_plan::capture::Mode::Estimated
        };
        let result =
            switchyard_plan::capture::capture(inner.session.as_mut(), conn.engine, &sql, mode)
                .await;
        drop(inner);
        lock(&self.queries).remove(&query);
        let elapsed = started.elapsed();
        tags.push(
            if analyze {
                "explain-analyze"
            } else {
                "explain"
            }
            .into(),
        );
        match result {
            Err(e) => {
                let msg = e.to_string();
                let st = StatementRequest {
                    sql: sql.clone(),
                    params: Vec::new(),
                    offset: 0,
                };
                self.record(
                    &conn,
                    &st,
                    started_at,
                    elapsed,
                    0,
                    &(HistoryStatus::Error, Some(msg.clone()), None),
                    &tags,
                )
                .await;
                self.plan_failed(query, msg, false);
            }
            Ok(plan) => {
                let findings = switchyard_plan::analyze(&plan, &self.thresholds().await);
                let history_id = if conn.history_enabled {
                    let entry = HistoryEntry {
                        id: 0,
                        connection_id: Some(conn.id.clone()),
                        connection_name: conn.name.clone(),
                        sql: sql.clone(),
                        started_at,
                        duration_ms: elapsed.as_millis() as i64,
                        rows: plan.root.rows().map(|r| r as i64),
                        affected: None,
                        status: HistoryStatus::Ok,
                        error: None,
                        tags,
                        has_plan: true,
                    };
                    let json = serde_json::to_string(&plan).unwrap_or_default();
                    match self
                        .with_store(move |s| {
                            let id = s.add_history(&entry)?;
                            s.add_plan(id, &json)?;
                            Ok(id)
                        })
                        .await
                    {
                        Ok(id) => Some(id),
                        Err(e) => {
                            warn!(error = %e, "plan history write failed");
                            None
                        }
                    }
                } else {
                    None
                };
                self.emit(Event::Plan {
                    request: query,
                    history_id,
                    plan: Arc::new(plan),
                    findings,
                });
            }
        }
    }

    async fn workload(&self, session: SessionId, request: RequestId) {
        let Some(slot) = self.slot(session) else {
            return self.emit(Event::Workload {
                request,
                result: Err(DbError::Closed.to_string()),
            });
        };
        let engine = slot.connection.engine;
        let mut inner = slot.inner.lock().await;
        let result = switchyard_plan::access::workload(inner.session.as_mut(), engine).await;
        drop(inner);
        self.emit(Event::Workload {
            request,
            result: result.map(Arc::new).map_err(|e| e.to_string()),
        });
    }

    async fn what_if(&self, session: SessionId, query: QueryId, sql: String, indexes: Vec<String>) {
        let Some(slot) = self.slot(session) else {
            return self.emit(Event::WhatIf {
                request: query,
                result: Err(DbError::Closed.to_string()),
            });
        };
        let engine = slot.connection.engine;
        let mut inner = slot.inner.lock().await;
        let (resume_tx, _resume_rx) = mpsc::unbounded_channel();
        lock(&self.queries).insert(
            query,
            QueryControl {
                cancel: inner.session.cancel_handle(),
                resume: resume_tx,
            },
        );
        let result =
            switchyard_plan::whatif::what_if(inner.session.as_mut(), engine, &sql, &indexes).await;
        drop(inner);
        lock(&self.queries).remove(&query);
        self.emit(Event::WhatIf {
            request: query,
            result: result.map(Arc::new).map_err(|e| e.to_string()),
        });
    }

    async fn load_plan(&self, request: RequestId, history_id: i64) {
        let json = match self.with_store(move |s| s.plan(history_id)).await {
            Ok(Some(j)) => j,
            Ok(None) => {
                return self.plan_failed(request, "no plan is stored with this entry", false);
            }
            Err(e) => return self.plan_failed(request, e, false),
        };
        match serde_json::from_str::<switchyard_plan::Plan>(&json) {
            Ok(plan) => {
                let findings = switchyard_plan::analyze(&plan, &self.thresholds().await);
                self.emit(Event::Plan {
                    request,
                    history_id: Some(history_id),
                    plan: Arc::new(plan),
                    findings,
                });
            }
            Err(e) => self.plan_failed(request, format!("stored plan is unreadable: {e}"), false),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        session: SessionId,
        query: QueryId,
        statements: Vec<StatementRequest>,
        tags: Vec<String>,
        confirmed_destructive: bool,
        fetch_limit: FetchLimit,
    ) {
        let started = Instant::now();
        let Some(slot) = self.slot(session) else {
            self.query_event(
                query,
                QueryEvent::Failed {
                    index: 0,
                    error: DbError::Closed,
                    location: None,
                },
            );
            self.query_event(
                query,
                QueryEvent::Finished {
                    elapsed: started.elapsed(),
                    cancelled: false,
                },
            );
            return;
        };
        let conn = slot.connection.clone();
        let dialect = dialect_for(conn.engine);
        // Guards run before taking the session, so nothing executes on refusal.
        for (index, st) in statements.iter().enumerate() {
            let class = guard::classify(dialect, &st.sql);
            if conn.read_only && !class.is_read_only() {
                self.query_event(
                    query,
                    QueryEvent::Failed {
                        index,
                        error: DbError::Unsupported(
                            "this connection is locked read-only; the statement would write".into(),
                        ),
                        location: None,
                    },
                );
                self.query_event(
                    query,
                    QueryEvent::Finished {
                        elapsed: started.elapsed(),
                        cancelled: false,
                    },
                );
                return;
            }
            if conn.environment.is_production() && !confirmed_destructive {
                let destructive: Vec<_> = class.destructive().to_vec();
                if !destructive.is_empty() {
                    self.query_event(query, QueryEvent::NeedsConfirmation { index, destructive });
                    self.query_event(
                        query,
                        QueryEvent::Finished {
                            elapsed: started.elapsed(),
                            cancelled: false,
                        },
                    );
                    return;
                }
            }
        }

        let mut inner = slot.inner.lock().await;
        let (resume_tx, mut resume_rx) = mpsc::unbounded_channel();
        let cancel = inner.session.cancel_handle();
        lock(&self.queries).insert(
            query,
            QueryControl {
                cancel: cancel.clone(),
                resume: resume_tx,
            },
        );

        let mut cancelled = false;
        let mut limit = match fetch_limit {
            FetchLimit::Rows(n) => Some(n),
            FetchLimit::All => None,
        };
        'statements: for (index, st) in statements.iter().enumerate() {
            if cancel.is_requested() {
                cancelled = true;
                break;
            }
            self.query_event(query, QueryEvent::StatementStarted { index });
            let st_started = Instant::now();
            let started_at = now_ms();
            let mut rows_total: i64 = 0;
            let mut outcome: (HistoryStatus, Option<String>, Option<i64>) =
                (HistoryStatus::Ok, None, None);
            let started = {
                let exec = inner.session.execute(&st.sql, &st.params);
                tokio::pin!(exec);
                loop {
                    tokio::select! {
                        // The statement is polled first: drivers clear their cancel flag as
                        // it starts, so a Cancel that arrived just before is set again here.
                        biased;
                        r = &mut exec => break r,
                        Some(msg) = resume_rx.recv() => match msg {
                            Resume::Cancel => {
                                if let Err(e) = cancel.cancel().await {
                                    self.error("Cancel", e);
                                }
                            }
                            Resume::More => limit = limit.map(|l| l + FETCH_STEP),
                            Resume::All => limit = None,
                        },
                    }
                }
            };
            match started {
                Err(e) => {
                    outcome = self.fail(query, index, dialect, st, e, &mut cancelled);
                    self.record(
                        &conn,
                        st,
                        started_at,
                        st_started.elapsed(),
                        rows_total,
                        &outcome,
                        &tags,
                    )
                    .await;
                    break 'statements;
                }
                Ok(mut stream) => {
                    let mut rows_in_set = 0usize;
                    let mut failed = false;
                    while let Some(ev) = stream.next().await {
                        match ev {
                            Ok(ResultEvent::Columns(c)) => {
                                rows_in_set = 0;
                                self.query_event(query, QueryEvent::Columns(c));
                            }
                            Ok(ResultEvent::Rows(b)) => {
                                rows_in_set += b.len();
                                rows_total += b.len() as i64;
                                self.query_event(query, QueryEvent::Rows(b));
                                if let Some(l) = limit
                                    && rows_in_set >= l
                                {
                                    self.query_event(
                                        query,
                                        QueryEvent::Paused { rows: rows_in_set },
                                    );
                                    match resume_rx.recv().await {
                                        Some(Resume::More) => limit = Some(l + FETCH_STEP),
                                        Some(Resume::All) => limit = None,
                                        Some(Resume::Cancel) | None => {
                                            cancelled = true;
                                            outcome.0 = HistoryStatus::Cancelled;
                                            drop(stream);
                                            self.record(
                                                &conn,
                                                st,
                                                started_at,
                                                st_started.elapsed(),
                                                rows_total,
                                                &outcome,
                                                &tags,
                                            )
                                            .await;
                                            break 'statements;
                                        }
                                    }
                                }
                            }
                            Ok(ResultEvent::Notice(n)) => {
                                self.query_event(query, QueryEvent::Notice(n))
                            }
                            Ok(ResultEvent::NextResultSet) => {
                                self.query_event(query, QueryEvent::NextResultSet)
                            }
                            Ok(ResultEvent::Done(c)) => {
                                outcome.2 = c.affected.map(|a| a as i64);
                                self.query_event(
                                    query,
                                    QueryEvent::StatementDone {
                                        index,
                                        completion: c,
                                    },
                                );
                            }
                            Err(e) => {
                                outcome = self.fail(query, index, dialect, st, e, &mut cancelled);
                                failed = true;
                                break;
                            }
                        }
                    }
                    self.record(
                        &conn,
                        st,
                        started_at,
                        st_started.elapsed(),
                        rows_total,
                        &outcome,
                        &tags,
                    )
                    .await;
                    if failed {
                        break 'statements;
                    }
                }
            }
            // Transaction bookkeeping.
            let open = inner.session.in_transaction();
            inner.txn_statements = if open { inner.txn_statements + 1 } else { 0 };
            self.emit(Event::Transaction {
                session,
                open,
                statements: inner.txn_statements,
            });
            if is_ddl(&st.sql) {
                let id = conn.id.clone();
                let _ = self.with_store(move |s| s.invalidate_schema(&id)).await;
            }
        }
        drop(inner);
        lock(&self.queries).remove(&query);
        self.query_event(
            query,
            QueryEvent::Finished {
                elapsed: started.elapsed(),
                cancelled,
            },
        );
    }

    fn fail(
        &self,
        query: QueryId,
        index: usize,
        dialect: &dyn switchyard_db::Dialect,
        st: &StatementRequest,
        e: DbError,
        cancelled: &mut bool,
    ) -> (HistoryStatus, Option<String>, Option<i64>) {
        if matches!(e, DbError::Cancelled) {
            *cancelled = true;
            return (HistoryStatus::Cancelled, None, None);
        }
        let location = e.as_server().and_then(|s| {
            // Relative to the statement; the UI adds the statement's buffer position.
            s.position.map(|p| dialect.error_line_col(&st.sql, p))
        });
        let message = e.to_string();
        self.query_event(
            query,
            QueryEvent::Failed {
                index,
                error: e,
                location,
            },
        );
        (HistoryStatus::Error, Some(message), None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        conn: &DbConnection,
        st: &StatementRequest,
        started_at: i64,
        elapsed: Duration,
        rows: i64,
        outcome: &(HistoryStatus, Option<String>, Option<i64>),
        tags: &[String],
    ) {
        if !conn.history_enabled {
            return;
        }
        let entry = HistoryEntry {
            id: 0,
            connection_id: Some(conn.id.clone()),
            connection_name: conn.name.clone(),
            sql: st.sql.clone(),
            started_at,
            duration_ms: elapsed.as_millis() as i64,
            rows: Some(rows),
            affected: outcome.2,
            status: outcome.0,
            error: outcome.1.clone(),
            tags: tags.to_vec(),
            has_plan: false,
        };
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "history write failed");
        }
    }
}

enum TxnOp {
    Begin,
    Commit,
    Rollback,
}

fn is_ddl(sql: &str) -> bool {
    let first: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "CREATE" | "ALTER" | "DROP" | "COMMENT" | "GRANT" | "REVOKE"
    )
}
