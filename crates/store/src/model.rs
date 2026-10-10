//! The saved-profile domain model: Hosts, database and file connections, terminal profiles,
//! environment labels and workspaces. Secrets are referenced by [`SecretRef`], never inline.

use serde::{Deserialize, Serialize};
use switchyard_db::{DbAuthMethod, Engine, SslMode};

use crate::random::random_hex;

/// Stable identifier of a saved profile.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub String);

impl ProfileId {
    /// A new random id.
    pub fn new() -> Self {
        Self(random_hex(8))
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Key of a secret in the keychain or vault. Contains no secret material.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(pub String);

impl SecretRef {
    /// The conventional key for a profile's primary secret.
    pub fn for_profile(id: &ProfileId, purpose: &str) -> Self {
        Self(format!("{}:{purpose}", id.0))
    }
}

/// Environment label. Drives color and safety rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentLabel {
    /// Production: red accent, destructive-statement confirmation, optional read-only lock.
    Production,
    /// Staging: amber.
    Staging,
    /// Development: green.
    Development,
    /// Local: neutral gray.
    #[default]
    Local,
}

impl EnvironmentLabel {
    /// All labels in UI order.
    pub const ALL: [EnvironmentLabel; 4] = [
        EnvironmentLabel::Production,
        EnvironmentLabel::Staging,
        EnvironmentLabel::Development,
        EnvironmentLabel::Local,
    ];

    /// Title-case name.
    pub fn name(self) -> &'static str {
        match self {
            EnvironmentLabel::Production => "Production",
            EnvironmentLabel::Staging => "Staging",
            EnvironmentLabel::Development => "Development",
            EnvironmentLabel::Local => "Local",
        }
    }

    /// Short badge (`PROD`, `STG`, `DEV`, `LOCAL`).
    pub fn badge(self) -> &'static str {
        match self {
            EnvironmentLabel::Production => "PROD",
            EnvironmentLabel::Staging => "STG",
            EnvironmentLabel::Development => "DEV",
            EnvironmentLabel::Local => "LOCAL",
        }
    }

    /// Whether destructive statements need confirmation.
    pub fn is_production(self) -> bool {
        self == EnvironmentLabel::Production
    }
}

/// How to authenticate to an SSH host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum SshAuth {
    /// Password stored in the secret store.
    #[default]
    Password,
    /// Private key read in place (never copied); passphrase in the secret store if any.
    PublicKey {
        /// Path to the private key.
        key_path: String,
    },
    /// Keyboard-interactive (MFA prompts).
    KeyboardInteractive,
    /// SSH agent.
    Agent,
}

impl SshAuth {
    /// Label for selects.
    pub fn label(&self) -> &'static str {
        match self {
            SshAuth::Password => "Password",
            SshAuth::PublicKey { .. } => "Public key",
            SshAuth::KeyboardInteractive => "Keyboard-interactive",
            SshAuth::Agent => "SSH agent",
        }
    }
}

/// A machine reached over SSH.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Host {
    /// Id.
    pub id: ProfileId,
    /// Display name (alias).
    pub name: String,
    /// Address or host name.
    pub address: String,
    /// SSH port.
    pub port: u16,
    /// Login user.
    pub user: String,
    /// Authentication method.
    pub auth: SshAuth,
    /// Jump hosts, outermost first (ids of other Hosts).
    #[serde(default)]
    pub jump_hosts: Vec<ProfileId>,
    /// Environment label.
    pub environment: EnvironmentLabel,
    /// Optional custom color (hex).
    #[serde(default)]
    pub color: Option<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Keepalive interval in seconds (0 = off).
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u32,
    /// Folder for grouping in the sidebar.
    #[serde(default)]
    pub folder: Option<String>,
    /// Password or key passphrase.
    #[serde(default)]
    pub secret: Option<SecretRef>,
    /// Agent socket for agent auth (OpenSSH `IdentityAgent`, e.g. 1Password's
    /// `~/.1password/agent.sock`, a Windows pipe, or `pageant`); `None` = `SSH_AUTH_SOCK`,
    /// then 1Password (Windows: the OpenSSH agent service, then Pageant).
    #[serde(default)]
    pub identity_agent: Option<String>,
    /// Public key file picking which agent key to offer.
    #[serde(default)]
    pub agent_key: Option<String>,
    /// Saved port forwards (local, remote, dynamic).
    #[serde(default)]
    pub forwards: Vec<PortForward>,
    /// Forward this machine's SSH agent to the Host (`ssh -A`).
    #[serde(default)]
    pub forward_agent: bool,
    /// Forward X11 to the local display (`ssh -X`).
    #[serde(default)]
    pub forward_x11: bool,
    /// X display to forward to instead of `DISPLAY` (`:1`, `localhost:0`).
    #[serde(default)]
    pub x11_display: Option<String>,
    /// Coding agents may run commands here; each one waits for the user's approval in the
    /// app. Off by default.
    #[serde(default)]
    pub agent_access: bool,
    /// Terminal macro (its id) typed into each new shell on this Host (MX-5).
    #[serde(default)]
    pub connect_macro: Option<String>,
    /// Shown in the sidebar's Favorites (MX-6).
    #[serde(default)]
    pub favorite: bool,
    /// Command typed into each new shell after login.
    #[serde(default)]
    pub startup_command: Option<String>,
    /// Remote folder each new shell starts in (`cd` before the startup command).
    #[serde(default)]
    pub start_directory: Option<String>,
    /// Environment variables sent with each shell (the server must accept them,
    /// OpenSSH `AcceptEnv`).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Terminal colors for this Host instead of the theme's.
    #[serde(default)]
    pub terminal_colors: Option<TerminalColors>,
}

/// Terminal colors of a Host (`#rrggbb`); `None` keeps the theme's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalColors {
    /// Text color.
    #[serde(default)]
    pub foreground: Option<String>,
    /// Background color.
    #[serde(default)]
    pub background: Option<String>,
}

impl TerminalColors {
    /// Whether neither color is set.
    pub fn is_empty(&self) -> bool {
        self.foreground.is_none() && self.background.is_none()
    }
}

/// `#rrggbb` as RGB.
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// Whether `name` can be an environment variable name (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Changes applied to several Hosts at once (sidebar bulk edit); `None` fields are left
/// as they are.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPatch {
    /// Folder (`Some(None)` moves out of any folder).
    pub folder: Option<Option<String>>,
    /// Environment label.
    pub environment: Option<EnvironmentLabel>,
    /// Login user.
    pub user: Option<String>,
    /// Favorite.
    pub favorite: Option<bool>,
    /// Startup command (`Some(None)` clears it).
    pub startup_command: Option<Option<String>>,
    /// Start folder (`Some(None)` clears it).
    pub start_directory: Option<Option<String>>,
    /// Keepalive interval in seconds.
    pub keepalive_secs: Option<u32>,
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl HostPatch {
    /// Whether it changes nothing.
    pub fn is_empty(&self) -> bool {
        self == &HostPatch::default()
    }

    /// Apply the set fields to `h`.
    pub fn apply(&self, h: &mut Host) {
        if let Some(f) = &self.folder {
            h.folder = non_empty(f);
        }
        if let Some(e) = self.environment {
            h.environment = e;
        }
        if let Some(u) = &self.user {
            h.user = u.trim().to_owned();
        }
        if let Some(f) = self.favorite {
            h.favorite = f;
        }
        if let Some(c) = &self.startup_command {
            h.startup_command = non_empty(c);
        }
        if let Some(d) = &self.start_directory {
            h.start_directory = non_empty(d);
        }
        if let Some(k) = self.keepalive_secs {
            h.keepalive_secs = k;
        }
    }
}

/// Direction of a saved port forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardDirection {
    /// `-L`: listen on this machine, reach the target from the Host.
    Local,
    /// `-R`: listen on the Host, reach the target from this machine.
    Remote,
    /// `-D`: SOCKS proxy on this machine, targets reached from the Host.
    Dynamic,
}

impl ForwardDirection {
    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            ForwardDirection::Local => "Local",
            ForwardDirection::Remote => "Remote",
            ForwardDirection::Dynamic => "Dynamic (SOCKS)",
        }
    }
}

/// A port forward saved on a Host (MobaXterm's "SSH tunnel").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    /// Id, unique within the Host.
    pub id: String,
    /// Display name; empty shows the forward itself.
    #[serde(default)]
    pub name: String,
    /// Direction.
    pub direction: ForwardDirection,
    /// Address listened on: here for local and dynamic forwards, on the Host for remote
    /// ones. Empty = loopback.
    #[serde(default)]
    pub bind_address: String,
    /// Port listened on; 0 = any free port.
    pub bind_port: u16,
    /// Target host (not used by dynamic forwards).
    #[serde(default)]
    pub target_host: String,
    /// Target port (not used by dynamic forwards).
    #[serde(default)]
    pub target_port: u16,
    /// Start whenever a terminal or the Files tab connects to the Host.
    #[serde(default)]
    pub auto_start: bool,
}

impl PortForward {
    /// A new forward with a fresh id.
    pub fn new(direction: ForwardDirection) -> Self {
        Self {
            id: ProfileId::new().0,
            name: String::new(),
            direction,
            bind_address: String::new(),
            bind_port: 0,
            target_host: String::new(),
            target_port: 0,
            auto_start: false,
        }
    }

    /// `L 8080 → db:5432`, `R 9000 ← localhost:3000`, `D 1080`.
    pub fn summary(&self) -> String {
        let bind = if self.bind_address.trim().is_empty() {
            self.bind_port.to_string()
        } else {
            format!("{}:{}", self.bind_address.trim(), self.bind_port)
        };
        match self.direction {
            ForwardDirection::Local => {
                format!("L {bind} → {}:{}", self.target_host, self.target_port)
            }
            ForwardDirection::Remote => {
                format!("R {bind} ← {}:{}", self.target_host, self.target_port)
            }
            ForwardDirection::Dynamic => format!("D {bind} (SOCKS)"),
        }
    }

    /// Why the forward cannot start, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        if self.direction != ForwardDirection::Dynamic {
            if self.target_host.trim().is_empty() {
                return Err("enter the target host".into());
            }
            if self.target_port == 0 {
                return Err("enter the target port".into());
            }
        }
        if self.direction != ForwardDirection::Remote && self.bind_port == 0 && self.auto_start {
            // Allowed, but a random port each time is rarely what an auto-start wants.
            return Err("give an auto-start forward a fixed port".into());
        }
        Ok(())
    }
}

fn default_keepalive() -> u32 {
    30
}

impl Host {
    /// A new Host with defaults.
    pub fn new(
        name: impl Into<String>,
        address: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            address: address.into(),
            port: 22,
            user: user.into(),
            auth: SshAuth::Password,
            jump_hosts: Vec::new(),
            environment: EnvironmentLabel::Local,
            color: None,
            tags: Vec::new(),
            keepalive_secs: 30,
            folder: None,
            secret: None,
            identity_agent: None,
            agent_key: None,
            forwards: Vec::new(),
            forward_agent: false,
            forward_x11: false,
            x11_display: None,
            agent_access: false,
            connect_macro: None,
            favorite: false,
            startup_command: None,
            start_directory: None,
            env: Vec::new(),
            terminal_colors: None,
        }
    }

    /// A copy under a new id named "<name> copy", without a stored secret (the caller
    /// copies it under the new id).
    pub fn duplicate(&self) -> Host {
        Host {
            id: ProfileId::new(),
            name: format!("{} copy", self.name),
            secret: None,
            ..self.clone()
        }
    }
}

/// A database endpoint, optionally reached through a Host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DbConnection {
    /// Id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
    /// Engine.
    pub engine: Engine,
    /// Server host (as seen from the tunnel Host when `via_host` is set).
    pub server: String,
    /// Server port.
    pub port: u16,
    /// Database.
    pub database: String,
    /// User.
    pub user: String,
    /// Authentication method.
    #[serde(default)]
    pub auth: DbAuthMethod,
    /// TLS policy.
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Tunnel through this Host.
    #[serde(default)]
    pub via_host: Option<ProfileId>,
    /// Environment label.
    pub environment: EnvironmentLabel,
    /// Lock the connection read-only.
    #[serde(default)]
    pub read_only: bool,
    /// Record executed statements in history.
    #[serde(default = "yes")]
    pub history_enabled: bool,
    /// Allow coding agents (MCP) to use this connection.
    #[serde(default)]
    pub agent_access: bool,
    /// The assistant's coding CLI for this connection (`claude-code`, `codex`, `gemini`,
    /// `custom`); `None` uses the default from Settings → Assistant.
    #[serde(default)]
    pub assistant_agent: Option<String>,
    /// Rows fetched before pausing (None = app default).
    #[serde(default)]
    pub fetch_limit: Option<u64>,
    /// Folder for grouping in the sidebar.
    #[serde(default)]
    pub folder: Option<String>,
    /// Password.
    #[serde(default)]
    pub secret: Option<SecretRef>,
    /// Microsoft Entra tenant (directory id or domain) for the `Entra*` methods; empty
    /// means any work or school account.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Entra application (client) id replacing Switchyard's built-in one, for organizations
    /// that require their own app registration.
    #[serde(default)]
    pub entra_client_id: Option<String>,
    /// Engine-specific settings (Snowflake `warehouse`, `role`, `schema`,
    /// `private_key_path`). Never secrets.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub options: std::collections::BTreeMap<String, String>,
}

fn yes() -> bool {
    true
}

impl DbConnection {
    /// A new connection with engine defaults.
    pub fn new(name: impl Into<String>, engine: Engine) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            engine,
            server: "localhost".into(),
            port: engine.default_port(),
            database: String::new(),
            user: String::new(),
            auth: DbAuthMethod::Password,
            ssl_mode: SslMode::Prefer,
            via_host: None,
            environment: EnvironmentLabel::Local,
            read_only: false,
            history_enabled: true,
            agent_access: false,
            assistant_agent: None,
            fetch_limit: None,
            folder: None,
            secret: None,
            tenant: None,
            entra_client_id: None,
            options: std::collections::BTreeMap::new(),
        }
    }

    /// An engine-specific option, trimmed; `None` when unset or blank.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }
}

/// FTP TLS mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FtpTls {
    /// Plain FTP.
    None,
    /// Explicit TLS (FTPES, `AUTH TLS`).
    #[default]
    Explicit,
    /// Implicit TLS (FTPS on 990).
    Implicit,
}

/// FTP data connection mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FtpMode {
    /// Passive.
    #[default]
    Passive,
    /// Active.
    Active,
}

/// Protocol of a file connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "kebab-case")]
pub enum FileProtocol {
    /// SFTP over a Host's SSH session.
    Sftp {
        /// The Host.
        host_id: ProfileId,
    },
    /// FTP or FTPS with its own credentials.
    Ftp {
        /// Server.
        server: String,
        /// Port.
        port: u16,
        /// TLS mode.
        tls: FtpTls,
        /// Data connection mode.
        mode: FtpMode,
        /// User.
        user: String,
    },
    /// AWS S3 or S3-compatible object storage.
    S3 {
        /// AWS Region (e.g. "us-east-1").
        region: String,
        /// Specific bucket to scope to, or None to list all accessible buckets.
        #[serde(default)]
        bucket: Option<String>,
        /// Optional custom endpoint URL (e.g. MinIO, LocalStack, Cloudflare R2).
        #[serde(default)]
        endpoint: Option<String>,
        /// Force path-style addressing (`http://endpoint/bucket/key`).
        #[serde(default)]
        force_path_style: bool,
        /// Authentication method: AWS profile or Access Key credentials.
        auth: S3Auth,
    },
}

/// Authentication method for S3 connections.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum S3Auth {
    /// AWS named profile from ~/.aws/credentials and ~/.aws/config.
    Profile {
        /// Profile name (e.g. "default").
        profile: String,
    },
    /// Static AWS credentials (Access Key ID and optional Session Token).
    /// The Secret Access Key is stored in `FileConnection.secret`.
    Credentials {
        /// AWS Access Key ID.
        access_key_id: String,
        /// Optional session token for temporary credentials.
        #[serde(default)]
        session_token: Option<String>,
    },
}

/// A file-transfer endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileConnection {
    /// Id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
    /// Protocol.
    pub protocol: FileProtocol,
    /// Remote directory opened first.
    #[serde(default)]
    pub default_path: Option<String>,
    /// Environment label.
    pub environment: EnvironmentLabel,
    /// Folder for grouping in the sidebar.
    #[serde(default)]
    pub folder: Option<String>,
    /// FTP password.
    #[serde(default)]
    pub secret: Option<SecretRef>,
}

/// A shell to open on a Host or locally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalProfile {
    /// Id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
    /// Host, or `None` for a local shell.
    #[serde(default)]
    pub host_id: Option<ProfileId>,
    /// Shell command (empty = login shell).
    #[serde(default)]
    pub shell: String,
    /// Command run after login.
    #[serde(default)]
    pub startup_command: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// An editor buffer kept across restarts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BufferState {
    /// Buffer id.
    pub id: String,
    /// Tab title.
    pub title: String,
    /// Bound connection.
    pub connection_id: Option<ProfileId>,
    /// Text.
    pub text: String,
    /// Cursor byte offset.
    pub cursor: usize,
}

/// A saved layout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// Name.
    pub name: String,
    /// Open editor buffers (autosaved).
    #[serde(default)]
    pub buffers: Vec<BufferState>,
    /// Pinned connections.
    #[serde(default)]
    pub pinned: Vec<ProfileId>,
    /// Sidebar collapsed.
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Active buffer id.
    #[serde(default)]
    pub active_buffer: Option<String>,
}

/// Any saved profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Profile {
    /// SSH Host.
    Host(Host),
    /// Database connection.
    Db(DbConnection),
    /// File connection.
    File(FileConnection),
    /// Terminal profile.
    Terminal(TerminalProfile),
}

impl Profile {
    /// Id.
    pub fn id(&self) -> &ProfileId {
        match self {
            Profile::Host(p) => &p.id,
            Profile::Db(p) => &p.id,
            Profile::File(p) => &p.id,
            Profile::Terminal(p) => &p.id,
        }
    }

    /// Display name.
    pub fn name(&self) -> &str {
        match self {
            Profile::Host(p) => &p.name,
            Profile::Db(p) => &p.name,
            Profile::File(p) => &p.name,
            Profile::Terminal(p) => &p.name,
        }
    }

    /// Kind key stored in SQLite.
    pub fn kind(&self) -> &'static str {
        match self {
            Profile::Host(_) => "host",
            Profile::Db(_) => "db",
            Profile::File(_) => "file",
            Profile::Terminal(_) => "terminal",
        }
    }

    /// The secret reference, if any.
    pub fn secret(&self) -> Option<&SecretRef> {
        match self {
            Profile::Host(p) => p.secret.as_ref(),
            Profile::Db(p) => p.secret.as_ref(),
            Profile::File(p) => p.secret.as_ref(),
            Profile::Terminal(_) => None,
        }
    }

    /// A copy without any secret reference (for export).
    pub fn without_secret(&self) -> Profile {
        let mut p = self.clone();
        match &mut p {
            Profile::Host(h) => h.secret = None,
            Profile::Db(d) => d.secret = None,
            Profile::File(f) => f.secret = None,
            Profile::Terminal(_) => {}
        }
        p
    }

    /// Validate fields that do not need other profiles.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let name = self.name().trim();
        if name.is_empty() {
            return Err(ValidationError::new("name", "Name is required"));
        }
        if name.len() > 120 {
            return Err(ValidationError::new("name", "Name is too long"));
        }
        match self {
            Profile::Host(h) => {
                if h.address.trim().is_empty() {
                    return Err(ValidationError::new("address", "Address is required"));
                }
                if h.port == 0 {
                    return Err(ValidationError::new("port", "Port must be 1–65535"));
                }
                if h.user.trim().is_empty() {
                    return Err(ValidationError::new("user", "User is required"));
                }
                if let SshAuth::PublicKey { key_path } = &h.auth
                    && key_path.trim().is_empty()
                {
                    return Err(ValidationError::new("key_path", "Key file is required"));
                }
                if h.jump_hosts.contains(&h.id) {
                    return Err(ValidationError::new(
                        "jump_hosts",
                        "A Host cannot jump through itself",
                    ));
                }
                if let Some((name, _)) = h.env.iter().find(|(n, _)| !is_env_name(n)) {
                    return Err(ValidationError::new(
                        "env",
                        &format!("\"{name}\" is not a variable name (letters, digits, _)"),
                    ));
                }
                if let Some(c) = &h.terminal_colors {
                    for (field, v) in [("fg", &c.foreground), ("bg", &c.background)] {
                        if let Some(v) = v
                            && parse_hex_color(v).is_none()
                        {
                            return Err(ValidationError::new(field, "Use a #rrggbb color"));
                        }
                    }
                }
            }
            Profile::Db(d) if d.engine == Engine::Snowflake => {
                if d.server.trim().is_empty() {
                    return Err(ValidationError::new(
                        "server",
                        "Account identifier is required (orgname-accountname)",
                    ));
                }
                if d.user.trim().is_empty() {
                    return Err(ValidationError::new("user", "User is required"));
                }
                match d.auth {
                    DbAuthMethod::KeyPair if d.option("private_key_path").is_none() => {
                        return Err(ValidationError::new(
                            "private_key_path",
                            "Choose the private key file",
                        ));
                    }
                    DbAuthMethod::KeyPair | DbAuthMethod::AccessToken => {}
                    _ => {
                        return Err(ValidationError::new(
                            "auth",
                            "Snowflake signs in with a key pair or a programmatic access token",
                        ));
                    }
                }
                if d.via_host.is_some() {
                    return Err(ValidationError::new(
                        "via_host",
                        "Cloud databases are reached over HTTPS, not through a Host",
                    ));
                }
                if d.fetch_limit == Some(0) {
                    return Err(ValidationError::new(
                        "fetch_limit",
                        "Fetch limit must be positive",
                    ));
                }
            }
            Profile::Db(d) if d.engine == Engine::Oracle => {
                if d.server.trim().is_empty() && d.database.trim().is_empty() {
                    return Err(ValidationError::new(
                        "server",
                        "Host is required (or a TNS alias as the service name)",
                    ));
                }
                if !d.server.trim().is_empty() && d.port == 0 {
                    return Err(ValidationError::new("port", "Port must be 1–65535"));
                }
                if d.user.trim().is_empty() {
                    return Err(ValidationError::new("user", "User is required"));
                }
                if d.auth != DbAuthMethod::Password {
                    return Err(ValidationError::new(
                        "auth",
                        "Oracle connections sign in with a user and password",
                    ));
                }
                if d.fetch_limit == Some(0) {
                    return Err(ValidationError::new(
                        "fetch_limit",
                        "Fetch limit must be positive",
                    ));
                }
            }
            Profile::Db(d) if d.engine == Engine::MongoDb => {
                if d.server.trim().is_empty() {
                    return Err(ValidationError::new("server", "Host is required"));
                }
                if d.port == 0 {
                    return Err(ValidationError::new("port", "Port must be 1–65535"));
                }
                if d.auth != DbAuthMethod::Password {
                    return Err(ValidationError::new(
                        "auth",
                        "MongoDB connections sign in with a user and password (or none)",
                    ));
                }
                if d.option("srv").is_some() && d.via_host.is_some() {
                    return Err(ValidationError::new(
                        "via_host",
                        "An SRV connection finds its members through DNS, not through a Host",
                    ));
                }
                if d.fetch_limit == Some(0) {
                    return Err(ValidationError::new(
                        "fetch_limit",
                        "Fetch limit must be positive",
                    ));
                }
            }
            Profile::Db(d) if d.engine.is_local_file() => {
                if d.database.trim().is_empty() {
                    return Err(ValidationError::new("database", "Choose a database file"));
                }
                if d.via_host.is_some() {
                    return Err(ValidationError::new(
                        "via_host",
                        "A SQLite file is opened on this computer, not through a Host",
                    ));
                }
                if d.fetch_limit == Some(0) {
                    return Err(ValidationError::new(
                        "fetch_limit",
                        "Fetch limit must be positive",
                    ));
                }
            }
            Profile::Db(d) if d.engine == Engine::Redis => {
                if d.server.trim().is_empty() {
                    return Err(ValidationError::new("server", "Host is required"));
                }
                if d.port == 0 {
                    return Err(ValidationError::new("port", "Port must be 1–65535"));
                }
                if switchyard_db::redis::client::database_index(&d.database).is_err() {
                    return Err(ValidationError::new(
                        "database",
                        "Database must be a number (0, 1, 2…)",
                    ));
                }
                if d.auth != DbAuthMethod::Password {
                    return Err(ValidationError::new(
                        "auth",
                        "Redis signs in with a password (and an optional ACL user)",
                    ));
                }
            }
            Profile::Db(d) if d.engine.is_cloud_api() => {
                if d.server.trim().is_empty() {
                    return Err(ValidationError::new("server", "Account ID is required"));
                }
                if d.database.trim().is_empty() {
                    return Err(ValidationError::new("database", "Database ID is required"));
                }
                if d.via_host.is_some() {
                    return Err(ValidationError::new(
                        "via_host",
                        "Cloud databases are reached over HTTPS, not through a Host",
                    ));
                }
                if d.fetch_limit == Some(0) {
                    return Err(ValidationError::new(
                        "fetch_limit",
                        "Fetch limit must be positive",
                    ));
                }
            }
            Profile::Db(d) => {
                if d.server.trim().is_empty() {
                    return Err(ValidationError::new("server", "Server is required"));
                }
                if d.port == 0 {
                    return Err(ValidationError::new("port", "Port must be 1–65535"));
                }
                if d.auth == DbAuthMethod::Password && d.user.trim().is_empty() {
                    return Err(ValidationError::new("user", "User is required"));
                }
                if d.auth == DbAuthMethod::WindowsPassword && d.user.trim().is_empty() {
                    return Err(ValidationError::new(
                        "user",
                        "Windows account is required (DOMAIN\\user)",
                    ));
                }
                if matches!(
                    d.auth,
                    DbAuthMethod::Integrated | DbAuthMethod::WindowsPassword
                ) && d.engine != Engine::SqlServer
                {
                    return Err(ValidationError::new(
                        "auth",
                        "Windows and Kerberos sign-in are for SQL Server",
                    ));
                }
                if d.auth.is_entra() && d.engine != Engine::SqlServer {
                    return Err(ValidationError::new(
                        "auth",
                        "Microsoft Entra sign-in is for SQL Server and Azure SQL",
                    ));
                }
                if matches!(
                    d.auth,
                    DbAuthMethod::EntraPassword | DbAuthMethod::EntraServicePrincipal
                ) && d.user.trim().is_empty()
                {
                    let msg = if d.auth == DbAuthMethod::EntraPassword {
                        "User (name@company.com) is required"
                    } else {
                        "Application (client) id is required"
                    };
                    return Err(ValidationError::new("user", msg));
                }
                if d.auth == DbAuthMethod::EntraServicePrincipal
                    && d.tenant.as_deref().is_none_or(|t| t.trim().is_empty())
                {
                    return Err(ValidationError::new(
                        "tenant",
                        "A service principal needs its tenant (directory) id",
                    ));
                }
                if d.fetch_limit == Some(0) {
                    return Err(ValidationError::new(
                        "fetch_limit",
                        "Fetch limit must be positive",
                    ));
                }
            }
            Profile::File(f) => match &f.protocol {
                FileProtocol::Ftp {
                    server, port, user, ..
                } => {
                    if server.trim().is_empty() {
                        return Err(ValidationError::new("server", "Server is required"));
                    }
                    if *port == 0 {
                        return Err(ValidationError::new("port", "Port must be 1–65535"));
                    }
                    if user.trim().is_empty() {
                        return Err(ValidationError::new("user", "User is required"));
                    }
                }
                FileProtocol::S3 { region, auth, .. } => {
                    if region.trim().is_empty() {
                        return Err(ValidationError::new("region", "Region is required"));
                    }
                    match auth {
                        S3Auth::Profile { profile } => {
                            if profile.trim().is_empty() {
                                return Err(ValidationError::new(
                                    "profile",
                                    "AWS profile name is required",
                                ));
                            }
                        }
                        S3Auth::Credentials { access_key_id, .. } => {
                            if access_key_id.trim().is_empty() {
                                return Err(ValidationError::new(
                                    "access_key_id",
                                    "AWS Access Key ID is required",
                                ));
                            }
                        }
                    }
                }
                FileProtocol::Sftp { .. } => {}
            },
            Profile::Terminal(_) => {}
        }
        Ok(())
    }

    /// Ids of other profiles this one references.
    pub fn references(&self) -> Vec<&ProfileId> {
        match self {
            Profile::Host(h) => h.jump_hosts.iter().collect(),
            Profile::Db(d) => d.via_host.iter().collect(),
            Profile::File(f) => match &f.protocol {
                FileProtocol::Sftp { host_id } => vec![host_id],
                FileProtocol::Ftp { .. } | FileProtocol::S3 { .. } => vec![],
            },
            Profile::Terminal(t) => t.host_id.iter().collect(),
        }
    }

    /// Environment label (terminal profiles inherit Local).
    pub fn environment(&self) -> EnvironmentLabel {
        match self {
            Profile::Host(p) => p.environment,
            Profile::Db(p) => p.environment,
            Profile::File(p) => p.environment,
            Profile::Terminal(_) => EnvironmentLabel::Local,
        }
    }
}

/// A field-level validation failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ValidationError {
    /// Field name.
    pub field: &'static str,
    /// Message for the user.
    pub message: String,
}

impl ValidationError {
    fn new(field: &'static str, message: &str) -> Self {
        Self {
            field,
            message: message.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_validation() {
        let mut r = DbConnection::new("cache", Engine::Redis);
        assert_eq!(r.port, 6379);
        assert!(Profile::Db(r.clone()).validate().is_ok(), "no user needed");
        r.database = "cache".into();
        assert_eq!(
            Profile::Db(r.clone()).validate().unwrap_err().field,
            "database"
        );
        r.database = "2".into();
        // Agents read Redis through read-only commands.
        r.agent_access = true;
        assert!(Profile::Db(r.clone()).validate().is_ok());
        let back: DbConnection = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.engine, Engine::Redis);
    }

    #[test]
    fn every_datasource_allows_agents() {
        let mut m = DbConnection::new("docs", Engine::MongoDb);
        m.server = "localhost".into();
        m.agent_access = true;
        assert!(Profile::Db(m).validate().is_ok());
        // Hosts saved before agent access existed load with it off.
        let h = Host::new("web", "web.example", "deploy");
        let mut v = serde_json::to_value(&h).unwrap();
        v.as_object_mut().unwrap().remove("agent_access");
        let back: Host = serde_json::from_value(v).unwrap();
        assert!(!back.agent_access);
    }

    #[test]
    fn port_forwards() {
        // Hosts saved before forwards existed still load.
        let mut v = serde_json::to_value(Host::new("web", "10.0.0.1", "deploy")).unwrap();
        v.as_object_mut().unwrap().remove("forwards");
        let h: Host = serde_json::from_value(v).unwrap();
        assert!(h.forwards.is_empty());

        let mut l = PortForward::new(ForwardDirection::Local);
        l.bind_port = 15432;
        l.target_host = "db.internal".into();
        l.target_port = 5432;
        assert_eq!(l.summary(), "L 15432 → db.internal:5432");
        assert!(l.validate().is_ok());
        let mut r = l.clone();
        r.direction = ForwardDirection::Remote;
        r.bind_address = "0.0.0.0".into();
        assert_eq!(r.summary(), "R 0.0.0.0:15432 ← db.internal:5432");
        let mut d = PortForward::new(ForwardDirection::Dynamic);
        d.bind_port = 1080;
        assert_eq!(d.summary(), "D 1080 (SOCKS)");
        assert!(d.validate().is_ok(), "SOCKS needs no target");

        let mut bad = PortForward::new(ForwardDirection::Local);
        bad.target_host = "db".into();
        assert!(bad.validate().unwrap_err().contains("target port"));
        bad.target_port = 5432;
        bad.auto_start = true;
        assert!(bad.validate().unwrap_err().contains("fixed port"));
        let back: PortForward = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn validation() {
        let mut h = Host::new("prod-db-01", "10.0.4.12", "deploy");
        assert!(Profile::Host(h.clone()).validate().is_ok());
        h.address.clear();
        assert_eq!(
            Profile::Host(h.clone()).validate().unwrap_err().field,
            "address"
        );
        h.address = "x".into();
        h.auth = SshAuth::PublicKey {
            key_path: " ".into(),
        };
        assert_eq!(
            Profile::Host(h.clone()).validate().unwrap_err().field,
            "key_path"
        );
        h.auth = SshAuth::Agent;
        h.jump_hosts = vec![h.id.clone()];
        assert_eq!(Profile::Host(h).validate().unwrap_err().field, "jump_hosts");

        let mut ad = DbConnection::new("ad", Engine::SqlServer);
        ad.server = "db.corp.example.com".into();
        ad.auth = DbAuthMethod::Integrated;
        assert!(Profile::Db(ad.clone()).validate().is_ok(), "no user needed");
        ad.auth = DbAuthMethod::WindowsPassword;
        assert_eq!(
            Profile::Db(ad.clone()).validate().unwrap_err().field,
            "user"
        );
        ad.user = "CORP\\ana".into();
        assert!(Profile::Db(ad.clone()).validate().is_ok());
        ad.engine = Engine::Postgres;
        assert_eq!(Profile::Db(ad).validate().unwrap_err().field, "auth");

        let mut az = DbConnection::new("azure", Engine::SqlServer);
        az.server = "contoso.database.windows.net".into();
        az.auth = DbAuthMethod::EntraInteractive;
        assert!(
            Profile::Db(az.clone()).validate().is_ok(),
            "account is optional"
        );
        az.auth = DbAuthMethod::EntraServicePrincipal;
        assert_eq!(
            Profile::Db(az.clone()).validate().unwrap_err().field,
            "user"
        );
        az.user = "app-id".into();
        assert_eq!(
            Profile::Db(az.clone()).validate().unwrap_err().field,
            "tenant"
        );
        az.tenant = Some("contoso.com".into());
        assert!(Profile::Db(az.clone()).validate().is_ok());
        az.engine = Engine::Postgres;
        assert_eq!(Profile::Db(az).validate().unwrap_err().field, "auth");
        // Profiles saved before Entra support still load.
        let old = r#"{"id":"p1","name":"x","engine":"sqlserver","server":"s","port":1433,
            "database":"","user":"sa","environment":"local"}"#;
        let d: DbConnection = serde_json::from_str(old).unwrap();
        assert_eq!((d.tenant, d.entra_client_id), (None, None));

        let mut cf = DbConnection::new("edge", Engine::D1);
        cf.server = "0123abcd".into();
        cf.database.clear();
        assert_eq!(
            Profile::Db(cf.clone()).validate().unwrap_err().field,
            "database"
        );
        cf.database = "9f1c-uuid".into();
        assert!(Profile::Db(cf).validate().is_ok(), "D1 needs no user");
        let mut mg = DbConnection::new("docs", Engine::MongoDb);
        assert!(
            Profile::Db(mg.clone()).validate().is_ok(),
            "MongoDB may run without authentication"
        );
        mg.server.clear();
        assert!(Profile::Db(mg).validate().is_err());

        let mut lite = DbConnection::new("local", Engine::Sqlite);
        assert_eq!(
            Profile::Db(lite.clone()).validate().unwrap_err().field,
            "database"
        );
        lite.database = "~/data/app.db".into();
        lite.server.clear();
        assert!(
            Profile::Db(lite.clone()).validate().is_ok(),
            "SQLite needs no server, port or user"
        );
        lite.via_host = Some(ProfileId("h1".into()));
        assert_eq!(Profile::Db(lite).validate().unwrap_err().field, "via_host");

        let mut sf = DbConnection::new("wh", Engine::Snowflake);
        sf.server = "myorg-acct".into();
        sf.user = "reader".into();
        sf.auth = DbAuthMethod::KeyPair;
        assert_eq!(
            Profile::Db(sf.clone()).validate().unwrap_err().field,
            "private_key_path"
        );
        sf.options
            .insert("private_key_path".into(), "/keys/rsa_key.p8".into());
        assert!(Profile::Db(sf.clone()).validate().is_ok());
        sf.auth = DbAuthMethod::Password;
        assert_eq!(
            Profile::Db(sf.clone()).validate().unwrap_err().field,
            "auth"
        );
        let json = serde_json::to_string(&sf).unwrap();
        let back: DbConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(back.option("private_key_path"), Some("/keys/rsa_key.p8"));

        let mut d = DbConnection::new("shop", Engine::Postgres);
        d.user = "app".into();
        assert!(Profile::Db(d.clone()).validate().is_ok());
        d.port = 0;
        assert_eq!(Profile::Db(d.clone()).validate().unwrap_err().field, "port");
        d.port = 5432;
        d.name = "  ".into();
        assert_eq!(Profile::Db(d).validate().unwrap_err().field, "name");

        let f = FileConnection {
            id: ProfileId::new(),
            name: "assets".into(),
            protocol: FileProtocol::Ftp {
                server: "".into(),
                port: 21,
                tls: FtpTls::Explicit,
                mode: FtpMode::Passive,
                user: "u".into(),
            },
            default_path: None,
            environment: EnvironmentLabel::Local,
            folder: None,
            secret: None,
        };
        assert_eq!(Profile::File(f).validate().unwrap_err().field, "server");
    }

    #[test]
    fn serde_round_trip() {
        let mut h = Host::new("bastion", "bastion.acme.dev", "deploy");
        h.auth = SshAuth::PublicKey {
            key_path: "~/.ssh/id_ed25519".into(),
        };
        h.environment = EnvironmentLabel::Production;
        let mut d = DbConnection::new("shop_prod", Engine::Postgres);
        d.via_host = Some(h.id.clone());
        d.secret = Some(SecretRef::for_profile(&d.id, "password"));
        let t = TerminalProfile {
            id: ProfileId::new(),
            name: "zsh".into(),
            host_id: None,
            shell: "/bin/zsh".into(),
            startup_command: None,
            env: vec![("TERM".into(), "xterm-256color".into())],
        };
        for p in [Profile::Host(h), Profile::Db(d), Profile::Terminal(t)] {
            let json = serde_json::to_string(&p).unwrap();
            let back: Profile = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p);
        }
        let w = Workspace {
            name: "acme-ops".into(),
            ..Default::default()
        };
        let back: Workspace = serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        assert_eq!(back, w);
    }

    #[test]
    fn export_copy_has_no_secret() {
        let mut d = DbConnection::new("shop", Engine::Postgres);
        d.secret = Some(SecretRef::for_profile(&d.id, "password"));
        assert!(Profile::Db(d).without_secret().secret().is_none());
    }

    #[test]
    fn host_session_settings_round_trip_and_validate() {
        let mut h = Host::new("web", "10.0.0.1", "deploy");
        h.favorite = true;
        h.folder = Some("Prod".into());
        h.startup_command = Some("tmux attach".into());
        h.start_directory = Some("/srv/app".into());
        h.env = vec![("LANG".into(), "C.UTF-8".into())];
        h.terminal_colors = Some(TerminalColors {
            foreground: Some("#e0e0e0".into()),
            background: Some("#101820".into()),
        });
        let p = Profile::Host(h.clone());
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Profile>(&json).unwrap(), p);
        assert!(p.validate().is_ok());
        // Older stores have none of the fields.
        let mut v = serde_json::to_value(&p).unwrap();
        for k in [
            "favorite",
            "startup_command",
            "start_directory",
            "env",
            "terminal_colors",
        ] {
            v.as_object_mut().unwrap().remove(k);
        }
        let Profile::Host(o) = serde_json::from_value::<Profile>(v).unwrap() else {
            panic!("not a host")
        };
        assert!(!o.favorite && o.env.is_empty() && o.terminal_colors.is_none());
        let mut bad = h.clone();
        bad.env.push(("1X".into(), "v".into()));
        assert_eq!(Profile::Host(bad).validate().unwrap_err().field, "env");
        let mut bad = h;
        bad.terminal_colors = Some(TerminalColors {
            foreground: Some("red".into()),
            background: None,
        });
        assert_eq!(Profile::Host(bad).validate().unwrap_err().field, "fg");
        assert_eq!(parse_hex_color("#0aFf10"), Some((10, 255, 16)));
        assert!(is_env_name("_A1") && !is_env_name("") && !is_env_name("A-B"));
    }

    #[test]
    fn host_patch_changes_only_what_is_set() {
        let mut h = Host::new("web", "10.0.0.1", "deploy");
        h.startup_command = Some("uptime".into());
        let before = h.clone();
        HostPatch::default().apply(&mut h);
        assert_eq!(h, before);
        let patch = HostPatch {
            folder: Some(Some("  Staging ".into())),
            environment: Some(EnvironmentLabel::Staging),
            favorite: Some(true),
            startup_command: Some(None),
            ..HostPatch::default()
        };
        assert!(!patch.is_empty());
        patch.apply(&mut h);
        assert_eq!(h.folder.as_deref(), Some("Staging"));
        assert_eq!(h.environment, EnvironmentLabel::Staging);
        assert!(h.favorite);
        assert_eq!(h.startup_command, None);
        assert_eq!(h.user, "deploy");
        HostPatch {
            folder: Some(Some(" ".into())),
            ..HostPatch::default()
        }
        .apply(&mut h);
        assert_eq!(h.folder, None);
        let d = h.duplicate();
        assert_ne!(d.id, h.id);
        assert_eq!(d.name, "web copy");
        assert!(d.secret.is_none() && d.favorite);
    }
}
