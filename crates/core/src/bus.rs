//! Commands the UI sends to the runtime, and events it receives back.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use switchyard_db::guard::Destructive;
use switchyard_db::{
    CatalogChunk, ColumnMeta, Completion, DbError, IntrospectScope, Notice, RowBatch, Value,
};
use switchyard_drivers::{Component, InstallProgress};
use switchyard_remote::FileEntry;
use switchyard_remote::ssh::{HostKeyDecision, HostKeyRequest, InteractiveRequest, TunnelInfo};
use switchyard_store::{
    BufferState, DbConnection, Favorite, FileConnection, HistoryEntry, Host, HostPatch, Macro,
    Profile, ProfileId, Snippet, Workspace,
};
use switchyard_term::{TermSize, Terminal};

/// Identifies one UI request so its answer can be matched.
pub type RequestId = u64;

/// Which file system a path belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FsRef {
    /// This computer.
    Local,
    /// A saved Host, over SFTP on its shared SSH session.
    Host(ProfileId),
    /// A saved FTP / FTPS file connection, with its own login.
    Ftp(ProfileId),
    /// A saved AWS S3 or S3-compatible file connection.
    S3(ProfileId),
}

impl FsRef {
    /// Whether this is a remote file system (SFTP or FTP): its paths are POSIX.
    pub fn is_remote(&self) -> bool {
        !matches!(self, FsRef::Local)
    }
}

/// A file operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsOp {
    /// Create a folder.
    Mkdir(PathBuf),
    /// Rename or move.
    Rename(PathBuf, PathBuf),
    /// Delete a file, or a folder with everything in it.
    Delete(PathBuf),
}

/// What to do when a transfer's target already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnConflict {
    /// Stop and report [`TransferError::Exists`].
    Ask,
    /// Replace it.
    Replace,
    /// Keep both: `name (1).ext`.
    KeepBoth,
}

/// Why a transfer did not finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferError {
    /// The target exists (with [`OnConflict::Ask`]).
    Exists(String),
    /// An interrupted copy of this file (bytes so far) waits on the target, e.g. after the
    /// app was closed mid-transfer: send `resume: true` to continue, or
    /// [`OnConflict::Replace`] to start over.
    Partial(u64),
    /// Cancelled by the user (the partial file was deleted).
    Cancelled,
    /// Paused by the user (the partial file is kept; resume continues from it).
    Paused,
    /// Anything else.
    Failed(String),
}

/// A text file opened for editing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFile {
    /// Contents.
    pub content: String,
    /// Modification time when read (ms since epoch), for the conflict check on save.
    pub modified_ms: Option<i64>,
}

/// Why a save did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The file changed since it was opened (its new modification time).
    Conflict(Option<i64>),
    /// Anything else.
    Failed(String),
}
/// An open database session.
pub type SessionId = u64;
/// A running query (one Run click; may contain several statements).
pub type QueryId = u64;
/// An open terminal (local shell or SSH channel).
pub type TermId = u64;

/// An assistant run, chosen by the UI.
pub type AgentRunId = u64;

/// The user's answer to a runtime prompt.
#[derive(Debug)]
pub enum PromptAnswer {
    /// Unknown host key.
    HostKey(HostKeyDecision),
    /// Password or passphrase (`None` = cancelled).
    Secret(Option<SecretString>),
    /// Keyboard-interactive answers (`None` = cancelled).
    Interactive(Option<Vec<SecretString>>),
    /// Stop waiting (a Microsoft Entra sign-in dialog was cancelled).
    Cancel,
}

/// Session logging state of a terminal (MX-3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermLogState {
    /// Output is being written to this file.
    Started(std::path::PathBuf),
    /// Logging stopped.
    Stopped,
    /// Logging could not start or stopped after a write error.
    Failed(String),
}

/// Connection state of an SSH terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermStatus {
    /// Connecting (or waiting on a prompt).
    Connecting,
    /// Connected; `description` is `user@host · method · via …`.
    Connected {
        /// Description.
        description: String,
    },
    /// The connection dropped; retrying.
    Reconnecting {
        /// Attempt number (1-based).
        attempt: u32,
        /// Attempts before giving up.
        of: u32,
        /// Seconds until this attempt.
        in_secs: u64,
    },
}

/// What a terminal connects to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermTarget {
    /// A local shell; `profile` picks a saved terminal profile.
    Local {
        /// Terminal profile (shell, environment), or the login shell.
        profile: Option<ProfileId>,
    },
    /// A shell on a saved Host over SSH.
    Host(ProfileId),
}

/// A session's current database and schema after [`Command::SetSessionContext`].
/// `None` means the connection's default (the profile's database, the login's schema).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionContext {
    /// Current database, when switched.
    pub database: Option<String>,
    /// Current schema, when switched.
    pub schema: Option<String>,
}

/// One statement to execute.
#[derive(Clone, Debug)]
pub struct StatementRequest {
    /// SQL text.
    pub sql: String,
    /// Parameter values in binding order.
    pub params: Vec<Value>,
    /// Byte offset of the statement in the editor buffer (for error positions).
    pub offset: usize,
}

/// How many rows to fetch before pausing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchLimit {
    /// Stop reading after this many rows; `FetchMore` resumes.
    Rows(usize),
    /// Read everything.
    All,
}

/// Commands from the UI. Every command is handled on the runtime; none blocks the UI.
#[derive(Debug)]
pub enum Command {
    /// Test command: wait, then answer with [`Event::Pong`].
    Ping {
        /// Echoed back.
        id: RequestId,
        /// Delay before answering.
        delay: Duration,
    },
    /// Load every saved profile.
    LoadProfiles,
    /// Validate and save a profile; store `secret` in the secret store when given.
    SaveProfile {
        /// Request id.
        request: RequestId,
        /// The profile.
        profile: Profile,
        /// New password or passphrase.
        secret: Option<SecretString>,
    },
    /// Delete a profile and its secret.
    DeleteProfile {
        /// Profile id.
        id: ProfileId,
    },
    /// Save a copy of a profile (and its stored secret) named "<name> copy".
    DuplicateProfile {
        /// Profile id.
        id: ProfileId,
    },
    /// Apply one change to several Hosts (sidebar bulk edit).
    UpdateHosts {
        /// Hosts.
        ids: Vec<ProfileId>,
        /// Fields to change.
        patch: HostPatch,
    },
    /// Persist sidebar order.
    ReorderProfiles {
        /// Ids in order.
        ids: Vec<ProfileId>,
    },
    /// Export profiles (no secrets) to a JSON file.
    ExportProfiles {
        /// Destination.
        path: PathBuf,
    },
    /// Import profiles from a JSON file.
    ImportProfiles {
        /// Source.
        path: PathBuf,
    },
    /// Unlock the fallback vault with the master password.
    UnlockVault {
        /// Master password.
        password: SecretString,
    },
    /// Try connecting with an unsaved profile.
    TestConnection {
        /// Request id.
        request: RequestId,
        /// Profile under edit.
        connection: DbConnection,
        /// Password typed in the form (falls back to the stored one).
        secret: Option<SecretString>,
    },
    /// Log in to a Host under edit (not necessarily saved) and report the result as
    /// [`Event::TestResult`].
    TestHost {
        /// Request id.
        request: RequestId,
        /// Host under edit.
        host: Host,
        /// Password or passphrase typed in the form (falls back to the stored one).
        secret: Option<SecretString>,
    },
    /// Log in to an FTP / FTPS file connection under edit (not necessarily saved) and
    /// report the result as [`Event::TestResult`].
    TestFiles {
        /// Request id.
        request: RequestId,
        /// File connection under edit.
        connection: FileConnection,
        /// Password typed in the form (falls back to the stored one).
        secret: Option<SecretString>,
    },
    /// Open a session for a saved connection.
    OpenSession {
        /// New session id chosen by the UI.
        session: SessionId,
        /// Connection.
        connection: ProfileId,
    },
    /// Close a session.
    CloseSession {
        /// Session.
        session: SessionId,
    },
    /// Open a Redis key browser session ([`Event::RedisOpened`]); closed with
    /// [`Command::CloseSession`].
    RedisOpen {
        /// New session id chosen by the UI.
        session: SessionId,
        /// Redis connection.
        connection: ProfileId,
    },
    /// One `SCAN` page of keys matching `pattern` ([`Event::RedisKeys`]).
    RedisScan {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// Glob pattern (`user:*`); empty for all keys.
        pattern: String,
        /// Only keys of this type (`SCAN … TYPE`); `None` for every type.
        kind: Option<switchyard_db::redis::KeyKind>,
        /// Cursor from the previous page; 0 starts over.
        cursor: u64,
    },
    /// A key's type, TTL and value ([`Event::RedisKey`]).
    RedisLoad {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// Key bytes.
        key: Vec<u8>,
    },
    /// Change a key from the browser ([`Event::RedisEdited`]).
    RedisEdit {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// Key bytes.
        key: Vec<u8>,
        /// The change.
        edit: switchyard_db::redis::KeyEdit,
        /// A new key: refused when the key exists.
        create: bool,
    },
    /// Run one console command line ([`Event::RedisReply`]).
    RedisRun {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// The line, `redis-cli` quoting.
        line: String,
        /// The user confirmed a destructive command on Production.
        confirmed: bool,
    },
    /// Switch the session's current database and/or schema ([`Event::SessionContext`]).
    /// Runs the dialect's `USE` statement, or reconnects the same session id to the other
    /// database (through the same tunnel) where the engine needs a new connection.
    SetSessionContext {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// Database to make current.
        database: Option<String>,
        /// Schema to make current (applied after the database).
        schema: Option<String>,
    },
    /// Execute statements in order, streaming results.
    Execute {
        /// Session.
        session: SessionId,
        /// New query id chosen by the UI.
        query: QueryId,
        /// Statements.
        statements: Vec<StatementRequest>,
        /// History tags.
        tags: Vec<String>,
        /// The user confirmed destructive statements on a Production connection.
        confirmed_destructive: bool,
        /// Fetch limit for each result set.
        fetch_limit: FetchLimit,
    },
    /// Continue a query paused at its fetch limit.
    FetchMore {
        /// Query.
        query: QueryId,
        /// Fetch everything that remains.
        all: bool,
    },
    /// Cancel a running query.
    Cancel {
        /// Query.
        query: QueryId,
    },
    /// Begin a manual transaction.
    Begin {
        /// Session.
        session: SessionId,
    },
    /// Commit the open transaction.
    Commit {
        /// Session.
        session: SessionId,
    },
    /// Roll back the open transaction.
    Rollback {
        /// Session.
        session: SessionId,
    },
    /// Load a catalog scope (from cache unless `refresh`).
    Introspect {
        /// Session.
        session: SessionId,
        /// Scope.
        scope: IntrospectScope,
        /// Bypass the cache.
        refresh: bool,
    },
    /// Capture the query plan of one statement. An actual plan (`analyze`) executes the
    /// statement inside a transaction that is rolled back.
    Explain {
        /// Session.
        session: SessionId,
        /// New query id chosen by the UI (cancel with [`Command::Cancel`]).
        query: QueryId,
        /// The statement.
        sql: String,
        /// Actual plan (`EXPLAIN ANALYZE`, `STATISTICS XML`) instead of an estimate.
        analyze: bool,
        /// The user confirmed an actual plan of a writing statement on Production.
        confirmed: bool,
        /// History tags.
        tags: Vec<String>,
    },
    /// Run a query for a coding agent: a single SELECT/WITH in a read-only transaction that
    /// is rolled back, with a row cap and a timeout; always recorded in history.
    AgentQuery {
        /// Session.
        session: SessionId,
        /// New query id.
        query: QueryId,
        /// The statement.
        sql: String,
        /// Rows kept at most.
        row_cap: usize,
        /// Stop and cancel after this long.
        timeout: Duration,
        /// History tags (`agent` is always added).
        tags: Vec<String>,
    },
    /// Record an agent tool call that is not a query (catalog, workload, what-if).
    RecordAgentCall {
        /// Session (names the connection).
        session: SessionId,
        /// What was called, as shown in history.
        summary: String,
        /// The error, when the call failed.
        error: Option<String>,
        /// History tags (`agent` is always added).
        tags: Vec<String>,
    },
    /// Listen for requests from `swy` (`--open`); writes the handoff file in `data_dir`.
    StartHandoff {
        /// The app's data directory.
        data_dir: std::path::PathBuf,
    },
    /// Read the access statistics (workload) of the session's database.
    Workload {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
    },
    /// List the server's sessions and running queries (activity monitor, DBX-5b) on the
    /// monitor's own session, opening it on `connection` first when needed. App only:
    /// never sent by the MCP server or `swy`.
    Activity {
        /// The monitor's session.
        session: SessionId,
        /// Connection to open it on.
        connection: ProfileId,
        /// Request id.
        request: RequestId,
    },
    /// Cancel a query or end a session from the activity monitor; recorded in history.
    /// App only: never sent by the MCP server or `swy`.
    SessionAction {
        /// The monitor's session (never the target).
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// What to do.
        action: switchyard_db::activity::ActivityAction,
        /// Whom to do it to (validated ids from the listing).
        target: switchyard_db::activity::SessionTarget,
        /// The user typed the second confirmation (required on Production).
        confirmed: bool,
    },
    /// Plan a statement with hypothetical indexes (PostgreSQL + HypoPG). Nothing is created.
    WhatIf {
        /// Session.
        session: SessionId,
        /// New query id chosen by the UI (cancel with [`Command::Cancel`]).
        query: QueryId,
        /// The statement.
        sql: String,
        /// `CREATE INDEX` statements to simulate.
        indexes: Vec<String>,
    },
    /// Load the plan stored with a history entry.
    LoadPlan {
        /// Request id.
        request: RequestId,
        /// History entry.
        history_id: i64,
    },
    /// Load the user's SQL snippets ([`Event::Snippets`]).
    LoadSnippets,
    /// Save a user snippet (new when its id is empty or built-in), then reload.
    SaveSnippet(Snippet),
    /// Delete a user snippet, then reload.
    DeleteSnippet {
        /// Snippet id.
        id: String,
    },
    /// Load the terminal macros ([`Event::Macros`]).
    LoadMacros,
    /// Save a terminal macro (new when its id is empty), then reload.
    SaveMacro(Macro),
    /// Delete a terminal macro, then reload.
    DeleteMacro {
        /// Macro id.
        id: String,
    },
    /// Load the pinned schema-tree objects ([`Event::Favorites`]).
    LoadFavorites,
    /// Pin an object at the end of the Favorites (a pin it already has is kept), then reload.
    AddFavorite(Favorite),
    /// Unpin, then reload.
    RemoveFavorite {
        /// Pin id.
        id: i64,
    },
    /// Put the pins in this order (unlisted ones follow), then reload.
    ReorderFavorites {
        /// Pin ids.
        ids: Vec<i64>,
    },
    /// Search query history.
    SearchHistory {
        /// Request id.
        request: RequestId,
        /// Search text.
        query: String,
        /// Restrict to a connection.
        connection: Option<ProfileId>,
    },
    /// Autosave an editor buffer.
    SaveBuffer {
        /// Buffer.
        buffer: BufferState,
        /// Tab position.
        position: i64,
    },
    /// Forget a closed buffer.
    DeleteBuffer {
        /// Buffer id.
        id: String,
    },
    /// Load the workspace (layout and buffers).
    LoadWorkspace,
    /// Save the workspace layout.
    SaveWorkspace(Workspace),
    /// Save a UI setting.
    SetSetting {
        /// Key.
        key: String,
        /// JSON value.
        value: serde_json::Value,
    },
    /// Read a UI setting ([`Event::Setting`]).
    LoadSetting {
        /// Key.
        key: String,
    },
    /// List a directory on any file system ([`Event::FsListing`]); `None` = home.
    ListDir {
        /// Request id.
        request: RequestId,
        /// File system.
        fs: FsRef,
        /// Directory.
        path: Option<PathBuf>,
    },
    /// Copy a file or folder (recursively) into a folder, possibly across file systems.
    Transfer {
        /// Transfer id, for progress and cancel.
        id: u64,
        /// Source file system.
        from: FsRef,
        /// Source path.
        path: PathBuf,
        /// Target file system.
        to: FsRef,
        /// Target folder; `None` = the local Downloads folder.
        dir: Option<PathBuf>,
        /// Existing target policy.
        on_conflict: OnConflict,
        /// Continue an earlier, interrupted attempt from its last byte.
        resume: bool,
    },
    /// Stop a transfer and delete its partial file.
    CancelTransfer {
        /// Transfer id.
        id: u64,
    },
    /// Stop a transfer, keeping its partial file; send the same `Transfer` with
    /// `resume: true` to continue.
    PauseTransfer {
        /// Transfer id.
        id: u64,
    },
    /// Synchronize a folder from one file system to another.
    SyncFolder {
        /// Operation id, for progress and cancellation.
        id: u64,
        /// Source file system.
        from: FsRef,
        /// Source directory.
        src: PathBuf,
        /// Target file system.
        to: FsRef,
        /// Target directory.
        dst: PathBuf,
    },
    /// Create, rename or delete ([`Event::FsOpDone`]).
    FsOp {
        /// Request id.
        request: RequestId,
        /// File system.
        fs: FsRef,
        /// Operation.
        op: FsOp,
    },
    /// Open a text file for editing ([`Event::TextFileRead`]).
    ReadTextFile {
        /// Request id.
        request: RequestId,
        /// File system.
        fs: FsRef,
        /// File.
        path: PathBuf,
    },
    /// Save an edited text file ([`Event::TextFileSaved`]). Refused with
    /// [`SaveError::Conflict`] when the file changed since `expect_modified`, unless `force`.
    WriteTextFile {
        /// Request id.
        request: RequestId,
        /// File system.
        fs: FsRef,
        /// File.
        path: PathBuf,
        /// New contents.
        content: String,
        /// Modification time when it was opened.
        expect_modified: Option<i64>,
        /// Save even if it changed.
        force: bool,
    },
    /// List a local directory.
    ListLocalDir {
        /// Request id.
        request: RequestId,
        /// Directory.
        path: PathBuf,
    },
    /// Detect optional native components ([`Event::Components`]).
    DetectComponents,
    /// Install a component with the strategy its manifest gives for this machine.
    InstallComponent {
        /// Component id.
        id: String,
        /// The user accepted the component's click-through license.
        accept_license: bool,
    },
    /// Install a component from a pre-downloaded archive (checked against the manifest).
    InstallComponentFromFile {
        /// Component id.
        id: String,
        /// Archive.
        path: PathBuf,
    },
    /// Use a library the user already has.
    UseComponentPath {
        /// Component id.
        id: String,
        /// Library file or folder.
        path: PathBuf,
    },
    /// Remove what Switchyard installed (or forget a chosen path).
    RemoveComponent {
        /// Component id.
        id: String,
    },
    /// Download component archives from an internal mirror (`None` = vendor URLs).
    SetDriverMirror {
        /// Base URL.
        url: Option<String>,
    },
    /// Write a text file (exports).
    WriteFile {
        /// Destination.
        path: PathBuf,
        /// Contents.
        contents: String,
    },
    /// Read `~/.ssh/config` and answer with [`Event::SshConfigPreview`]; nothing is saved.
    PreviewSshConfig,
    /// Import Hosts from `~/.ssh/config`: the entries named in `only`, or every new one.
    ImportSshConfig {
        /// Aliases to import; `None` imports every entry not saved yet.
        only: Option<Vec<String>>,
    },
    /// Apply staged inline edits in one transaction. Each statement must change exactly
    /// one row; otherwise everything is rolled back. Sessions without transactions
    /// (MongoDB) apply them in order and stop at the first failure.
    ApplyEdits {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// UPDATE statements.
        statements: Vec<String>,
    },
    /// Open a terminal.
    OpenTerminal {
        /// Terminal id chosen by the UI.
        term: TermId,
        /// Where it connects.
        target: TermTarget,
        /// Initial size in cells.
        size: TermSize,
    },
    /// Send bytes to a terminal's program.
    TerminalInput {
        /// Terminal.
        term: TermId,
        /// Bytes.
        bytes: Vec<u8>,
    },
    /// The terminal view changed size.
    TerminalResize {
        /// Terminal.
        term: TermId,
        /// New size in cells.
        size: TermSize,
    },
    /// Close a terminal.
    CloseTerminal {
        /// Terminal.
        term: TermId,
    },
    /// Retry a dropped SSH terminal now instead of waiting for the backoff.
    ReconnectTerminal {
        /// Terminal.
        term: TermId,
    },
    /// Log a terminal's output to a new file ([`Event::TerminalLog`]).
    StartTerminalLog {
        /// Terminal.
        term: TermId,
        /// Format, folder and file name.
        settings: crate::term_settings::LogSettings,
    },
    /// Stop logging a terminal ([`Event::TerminalLog`]).
    StopTerminalLog {
        /// Terminal.
        term: TermId,
    },
    /// Stop a tunnel; sessions using it end.
    StopTunnel {
        /// Tunnel id.
        id: u64,
    },
    /// Report the live tunnels ([`Event::Tunnels`]).
    ListTunnels,
    /// Ask a coding CLI (the assistant). Events arrive as [`Event::Agent`], ending with
    /// `Exited`. The run may use `connection` only (all agent-enabled connections when
    /// `None`); `agent` `None` uses the connection's override or the default from Settings →
    /// Assistant.
    RunAgent {
        /// Run id.
        run: AgentRunId,
        /// The CLI, or `None` for the configured one.
        agent: Option<switchyard_agents::AgentKind>,
        /// The connection the question is about.
        connection: Option<ProfileId>,
        /// The request.
        prompt: String,
        /// Conversation to continue.
        resume: Option<String>,
        /// Whether the run may use database connections at all. `false` (an API Workbench
        /// question) gives the run no connection, whatever `connection` says.
        databases: bool,
    },
    /// Stop a run (its CLI and everything it started).
    CancelAgent {
        /// Run id.
        run: AgentRunId,
    },
    /// The user's answer to [`Event::AgentApproval`]: run the command, or refuse it.
    AnswerAgentApproval {
        /// The approval's id.
        id: u64,
        /// Run it.
        approve: bool,
    },
    /// Run one read-only Redis command for a coding agent ([`Event::RedisReply`]). Only
    /// commands that read (not `KEYS`, which blocks the server) are run; every call is
    /// recorded in history with `tags`, whatever the connection's history setting.
    AgentRedis {
        /// A Redis session opened with [`Command::RedisOpen`].
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// The line, `redis-cli` quoting.
        line: String,
        /// History tags (`agent` is always added).
        tags: Vec<String>,
    },
    /// Start the CLI interactively in a local terminal with Switchyard's tools attached
    /// ("Open in terminal"); answers like [`Command::OpenTerminal`].
    OpenAgentTerminal {
        /// Terminal id chosen by the UI.
        term: TermId,
        /// The CLI, or `None` for the configured one.
        agent: Option<switchyard_agents::AgentKind>,
        /// The connection it may use (all agent-enabled ones when `None`).
        connection: Option<ProfileId>,
        /// Initial size in cells.
        size: TermSize,
    },
    /// Start one of a Host's saved port forwards (no-op if it is running). Failures come
    /// back as [`Event::Error`] with context "Port forward".
    StartForward {
        /// Host.
        host: ProfileId,
        /// [`switchyard_store::PortForward::id`].
        forward: String,
    },
    /// Answer a prompt the runtime raised.
    AnswerPrompt {
        /// The prompt's request id.
        request: RequestId,
        /// The answer.
        answer: PromptAnswer,
    },
    /// After a changed-key warning: trust exactly `fingerprint` for Host `host` on its next
    /// connection (stored in Switchyard's known_hosts, never the user's file).
    AcceptChangedHostKey {
        /// Host profile.
        host: ProfileId,
        /// The new key's fingerprint, as shown to the user.
        fingerprint: String,
    },
    /// Ask GitHub Releases for a newer version ([`Event::UpdateStatus`]).
    CheckForUpdates {
        /// The user asked (show "up to date" and failures, not only a newer version).
        manual: bool,
    },
}

/// Events from a running query.
#[derive(Clone, Debug)]
pub enum QueryEvent {
    /// A statement started.
    StatementStarted {
        /// Index in the request.
        index: usize,
    },
    /// Columns of a new result set.
    Columns(Arc<[ColumnMeta]>),
    /// Rows.
    Rows(RowBatch),
    /// Server notice.
    Notice(Notice),
    /// A further result set follows.
    NextResultSet,
    /// Reading paused at the fetch limit.
    Paused {
        /// Rows loaded in the current result set.
        rows: usize,
    },
    /// A statement finished.
    StatementDone {
        /// Index.
        index: usize,
        /// Completion info.
        completion: Completion,
    },
    /// The statement needs confirmation on Production; nothing ran.
    NeedsConfirmation {
        /// Index.
        index: usize,
        /// Findings.
        destructive: Vec<Destructive>,
    },
    /// A statement failed; later statements did not run.
    Failed {
        /// Index.
        index: usize,
        /// Error.
        error: DbError,
        /// Absolute (line, column) in the buffer, when known.
        location: Option<(u32, u32)>,
    },
    /// The query ended (after success, failure or cancel).
    Finished {
        /// Total time.
        elapsed: Duration,
        /// Whether it was cancelled.
        cancelled: bool,
    },
}

/// Events for the UI.
#[derive(Clone, Debug)]
pub enum Event {
    /// Answer to [`Command::CheckForUpdates`].
    UpdateStatus {
        /// Echoes the command's `manual`.
        manual: bool,
        /// What the check found.
        status: crate::update::UpdateStatus,
    },
    /// What importing `~/.ssh/config` would add (answer to `PreviewSshConfig`).
    SshConfigPreview {
        /// The file read.
        path: PathBuf,
        /// Every usable `Host` entry, saved ones marked.
        hosts: Vec<crate::ssh_import::SshImportCandidate>,
    },
    /// Answer to [`Command::Ping`].
    Pong {
        /// Request id.
        id: RequestId,
    },
    /// All profiles, in sidebar order.
    Profiles(Vec<Profile>),
    /// A profile was saved.
    ProfileSaved {
        /// Request id.
        request: RequestId,
        /// Profile id.
        id: ProfileId,
    },
    /// Saving failed.
    ProfileError {
        /// Request id.
        request: RequestId,
        /// Field, when a validation error.
        field: Option<&'static str>,
        /// Message.
        message: String,
    },
    /// Secret backend state.
    SecretBackend {
        /// Backend name.
        name: &'static str,
        /// Whether it needs unlocking.
        locked: bool,
    },
    /// Result of a connection test.
    TestResult {
        /// Request id.
        request: RequestId,
        /// Summary on success, message on failure.
        result: Result<String, String>,
    },
    /// A session opened.
    SessionOpened {
        /// Session.
        session: SessionId,
        /// Server version.
        server_version: String,
    },
    /// A session failed to open.
    SessionFailed {
        /// Session.
        session: SessionId,
        /// Message.
        message: String,
    },
    /// Result of [`Command::RedisOpen`].
    RedisOpened {
        /// Session.
        session: SessionId,
        /// Server details, or why it failed.
        result: Result<RedisInfo, String>,
    },
    /// Result of [`Command::RedisScan`].
    RedisKeys {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// The page, or why it failed.
        result: Result<switchyard_db::redis::ScanPage, String>,
    },
    /// Result of [`Command::RedisLoad`].
    RedisKey {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// The key, or why it failed.
        result: Result<Arc<switchyard_db::redis::KeyDetails>, String>,
    },
    /// Result of [`Command::RedisEdit`].
    RedisEdited {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// The key that changed (its new name after a rename).
        key: Vec<u8>,
        /// What was done, or why it failed.
        result: Result<String, String>,
    },
    /// Result of [`Command::RedisRun`].
    RedisReply {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// What happened.
        outcome: RedisOutcome,
    },
    /// Result of [`Command::SetSessionContext`].
    SessionContext {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// The session's database and schema now, or why the switch failed.
        result: Result<SessionContext, String>,
    },
    /// A query event.
    Query {
        /// Query.
        query: QueryId,
        /// Event.
        event: QueryEvent,
    },
    /// Transaction state changed.
    Transaction {
        /// Session.
        session: SessionId,
        /// Whether a transaction is open.
        open: bool,
        /// Statements run in it.
        statements: u32,
    },
    /// A catalog scope loaded.
    Catalog {
        /// Session.
        session: SessionId,
        /// Scope.
        scope: IntrospectScope,
        /// Data or error.
        result: Result<CatalogChunk, String>,
        /// When the data was fetched (ms since epoch).
        cached_at: i64,
    },
    /// A captured or loaded plan, with its findings.
    Plan {
        /// The [`Command::Explain`] query id or [`Command::LoadPlan`] request id.
        request: u64,
        /// History entry the plan is stored with, if history is on.
        history_id: Option<i64>,
        /// The plan.
        plan: Arc<switchyard_plan::Plan>,
        /// Findings, ranked.
        findings: Vec<switchyard_plan::Finding>,
    },
    /// A request from `swy` (see [`crate::handoff`]).
    Handoff(crate::handoff::Handoff),
    /// Result of [`Command::AgentQuery`].
    AgentRows {
        /// The query id.
        query: QueryId,
        /// Rows, or why nothing ran.
        result: Result<crate::service::agent::AgentRows, String>,
    },
    /// Access statistics for [`Command::Workload`].
    Workload {
        /// Request id.
        request: RequestId,
        /// The workload, or what went wrong.
        result: Result<Arc<switchyard_plan::access::Workload>, String>,
    },
    /// Result of [`Command::Activity`].
    Activity {
        /// Request id.
        request: RequestId,
        /// Sessions, or what went wrong.
        result: Result<Arc<switchyard_db::activity::Activity>, String>,
    },
    /// Result of [`Command::SessionAction`].
    SessionAction {
        /// Request id.
        request: RequestId,
        /// What happened, or why nothing was done.
        result: Result<String, String>,
    },
    /// Result of [`Command::WhatIf`].
    WhatIf {
        /// The query id.
        request: QueryId,
        /// Plans before and after, or what went wrong.
        result: Result<Arc<switchyard_plan::whatif::WhatIf>, String>,
    },
    /// A plan could not be captured or loaded.
    PlanFailed {
        /// The [`Command::Explain`] query id or [`Command::LoadPlan`] request id.
        request: u64,
        /// What went wrong.
        error: String,
        /// An actual plan of a writing statement on Production needs confirmation first.
        needs_confirmation: bool,
    },
    /// The user's SQL snippets (built-ins excluded), answering the snippet commands.
    Snippets(Vec<Snippet>),
    /// The pinned schema-tree objects in order, answering the favorite commands.
    Favorites(Vec<Favorite>),
    /// The terminal macros by name, answering the macro commands.
    Macros(Vec<Macro>),
    /// History search results.
    History {
        /// Request id.
        request: RequestId,
        /// Entries, newest first.
        entries: Vec<HistoryEntry>,
    },
    /// The workspace.
    Workspace(Workspace),
    /// A directory listing.
    DirListing {
        /// Request id.
        request: RequestId,
        /// Directory.
        path: PathBuf,
        /// Entries or error.
        result: Result<Vec<FileEntry>, String>,
    },
    /// A directory listing for [`Command::ListDir`].
    FsListing {
        /// Request id.
        request: RequestId,
        /// File system.
        fs: FsRef,
        /// The directory listed (home resolved).
        path: PathBuf,
        /// Entries or error.
        result: Result<Vec<FileEntry>, String>,
    },
    /// A transfer is waiting for a free slot (four run at once).
    TransferQueued {
        /// Transfer id.
        id: u64,
    },
    /// Transfer progress.
    TransferProgress {
        /// Transfer id.
        id: u64,
        /// What is being copied (top-level name).
        name: String,
        /// Bytes so far.
        done: u64,
        /// Total bytes, when known.
        total: Option<u64>,
    },
    /// A transfer ended.
    TransferDone {
        /// Transfer id.
        id: u64,
        /// Where it went, or why not.
        result: Result<PathBuf, TransferError>,
    },
    /// A folder sync operation finished ([`Command::SyncFolder`]).
    FolderSynced {
        /// Operation id.
        id: u64,
        /// Number of files evaluated.
        files_checked: usize,
        /// Number of files transferred.
        files_transferred: usize,
        /// Total bytes transferred.
        bytes_transferred: u64,
        /// Error message, if failed.
        error: Option<String>,
    },
    /// Result of [`Command::FsOp`].
    FsOpDone {
        /// Request id.
        request: RequestId,
        /// File system.
        fs: FsRef,
        /// Error, if any.
        result: Result<(), String>,
    },
    /// Result of [`Command::ReadTextFile`].
    TextFileRead {
        /// Request id.
        request: RequestId,
        /// Contents or error.
        result: Result<TextFile, String>,
    },
    /// Result of [`Command::WriteTextFile`]: the new modification time.
    TextFileSaved {
        /// Request id.
        request: RequestId,
        /// New modification time, or why not.
        result: Result<Option<i64>, SaveError>,
    },
    /// A UI setting, answering [`Command::LoadSetting`].
    Setting {
        /// Key.
        key: String,
        /// JSON value, if set.
        value: Option<serde_json::Value>,
    },
    /// Native component status.
    Components(Vec<Component>),
    /// An install is under way.
    ComponentProgress {
        /// Component id.
        id: String,
        /// Step.
        progress: InstallProgress,
    },
    /// A component is ready (installed, found at a chosen path).
    ComponentInstalled {
        /// Its new state.
        component: Component,
    },
    /// Installing, using a path, or removing failed.
    ComponentFailed {
        /// Component id.
        id: String,
        /// Why.
        message: String,
        /// The command to run in a terminal, when elevation is not available here.
        command: Option<String>,
    },
    /// Result of [`Command::ApplyEdits`]: rows changed, or why nothing was.
    EditsApplied {
        /// Request id.
        request: RequestId,
        /// Rows changed or error.
        result: Result<u64, String>,
        /// Time taken.
        elapsed: Duration,
    },
    /// A terminal is ready; the view draws from `terminal`.
    TerminalOpened {
        /// Terminal.
        term: TermId,
        /// Shared terminal state.
        terminal: Terminal,
        /// Short description (`deploy@10.0.4.12 · ed25519`, `/bin/zsh`).
        description: String,
    },
    /// An SSH terminal's connection state changed.
    TerminalStatus {
        /// Terminal.
        term: TermId,
        /// State.
        status: TermStatus,
    },
    /// The server's host key differs from the stored one; the connection is blocked.
    HostKeyChanged {
        /// Terminal that hit it, if any.
        term: Option<TermId>,
        /// Host profile.
        host_id: ProfileId,
        /// Host label.
        host: String,
        /// `address:port`.
        address: String,
        /// Stored fingerprint.
        stored: String,
        /// Received fingerprint.
        received: String,
        /// Where the stored key is (`file:line`).
        location: String,
    },
    /// Unknown host key: trust it?
    HostKeyPrompt {
        /// Answer with this id.
        request: RequestId,
        /// The key.
        key: HostKeyRequest,
    },
    /// A password or key passphrase is needed.
    SecretPrompt {
        /// Answer with this id.
        request: RequestId,
        /// Host label.
        host: String,
        /// Prompt text.
        prompt: String,
    },
    /// Keyboard-interactive questions (MFA).
    InteractivePrompt {
        /// Answer with this id.
        request: RequestId,
        /// The questions.
        req: InteractiveRequest,
    },
    /// Microsoft Entra sign-in in the browser: open `url`, then wait. Answer with
    /// [`PromptAnswer::Cancel`] to give up; [`Event::PromptClosed`] ends the wait.
    EntraSignIn {
        /// Request id.
        request: RequestId,
        /// Connection name.
        connection: String,
        /// Microsoft's sign-in page.
        url: String,
    },
    /// Microsoft Entra device-code sign-in: show the code and where to enter it.
    EntraDeviceCode {
        /// Request id.
        request: RequestId,
        /// Connection name.
        connection: String,
        /// The code to enter.
        code: String,
        /// Where to enter it.
        url: String,
        /// Microsoft's instruction text.
        message: String,
    },
    /// A prompt the runtime raised is no longer needed (sign-in finished or failed).
    PromptClosed {
        /// Request id.
        request: RequestId,
    },
    /// A coding agent asks to run a command on a Host, or for an actual plan: show it and
    /// answer with [`Command::AnswerAgentApproval`]. Nothing runs until the user approves.
    AgentApproval(AgentApproval),
    /// An approval is no longer waiting (answered, timed out, or the agent went away).
    AgentApprovalClosed {
        /// The approval's id.
        id: u64,
    },
    /// What an assistant run did ([`Command::RunAgent`]); `Exited` is its last event.
    Agent {
        /// Run id.
        run: AgentRunId,
        /// The CLI that ran.
        agent: switchyard_agents::AgentKind,
        /// What happened.
        event: switchyard_agents::AgentEvent,
    },
    /// Live tunnels (sent when they open, stop, or their counters change).
    Tunnels(Vec<TunnelInfo>),
    /// A terminal could not be opened.
    TerminalFailed {
        /// Terminal.
        term: TermId,
        /// Why.
        message: String,
    },
    /// New output to draw (coalesced until the next snapshot).
    TerminalWake {
        /// Terminal.
        term: TermId,
    },
    /// The program set the title.
    TerminalTitle {
        /// Terminal.
        term: TermId,
        /// Title (empty = reset).
        title: String,
    },
    /// Bell.
    TerminalBell {
        /// Terminal.
        term: TermId,
    },
    /// A terminal's session log started, stopped or failed.
    TerminalLog {
        /// Terminal.
        term: TermId,
        /// New state.
        state: TermLogState,
    },
    /// The program asked to copy text (OSC 52).
    TerminalClipboard {
        /// Terminal.
        term: TermId,
        /// Text.
        text: String,
    },
    /// The program ended or the connection closed.
    TerminalExited {
        /// Terminal.
        term: TermId,
        /// Exit code, when known.
        code: Option<u32>,
        /// Why, when the connection failed rather than the program exiting.
        message: Option<String>,
    },
    /// A short confirmation for a toast.
    Toast(String),
    /// A background failure.
    Error {
        /// What was being done.
        context: String,
        /// Message.
        message: String,
    },
}

/// Something a coding agent wants to run that needs the user's approval
/// ([`Event::AgentApproval`]): a shell command on a Host, or an actual plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentApproval {
    /// Id to answer with.
    pub id: u64,
    /// The CLI asking.
    pub agent: switchyard_agents::AgentKind,
    /// What it asks for.
    pub kind: ApprovalKind,
    /// The Host or connection.
    pub target: ProfileId,
    /// The Host's or connection's name.
    pub target_name: String,
    /// Its environment label.
    pub environment: switchyard_store::EnvironmentLabel,
    /// The shell command or the statement, exactly as it would run.
    pub text: String,
}

/// What an [`AgentApproval`] asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    /// A shell command on a Host's SSH session.
    SshCommand,
    /// An actual plan (`EXPLAIN ANALYZE`, `STATISTICS XML`): the statement runs, then is
    /// rolled back.
    ActualPlan {
        /// The statement writes (rolled back, but triggers and sequences still fire).
        writes: bool,
    },
}

/// A connected Redis server ([`Event::RedisOpened`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedisInfo {
    /// `Redis 7.2.4`.
    pub version: String,
    /// Selected logical database.
    pub db: u32,
    /// Keys in that database.
    pub keys: u64,
    /// The connection is locked read-only.
    pub read_only: bool,
}

/// What a console command did ([`Event::RedisReply`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedisOutcome {
    /// The server answered (an error reply is `error: true`; the connection is fine).
    Output {
        /// The reply as `redis-cli` prints it.
        text: String,
        /// The reply was a server error.
        error: bool,
        /// Round trip in ms.
        ms: u64,
    },
    /// A destructive command on Production: run again with `confirmed` to proceed.
    NeedsConfirmation {
        /// What would happen.
        reason: String,
    },
    /// The command was refused or the connection failed.
    Failed(String),
}
