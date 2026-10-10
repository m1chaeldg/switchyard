//! Connection editor dialog: a rail of connection types (Database, SSH Host, SFTP, FTP), a
//! database engine picker with a form per engine (`engines/`), environment label, Driver
//! Manager card, Test connection and Save.

use std::collections::HashMap;

use crate::appearance::{rpx, ts};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, deferred, div, px,
};
use secrecy::SecretString;
use switchyard_core::db::Engine;
use switchyard_core::drivers::{Component, ComponentStatus};
use switchyard_core::store::{
    EnvironmentLabel, FileConnection, FileProtocol, FtpMode, FtpTls, Host, Profile, ProfileId,
    S3Auth, SshAuth, TerminalColors,
};
use switchyard_core::{Command, RequestId, RuntimeHandle};

mod driver_card;
mod engines;
mod form;

use form::{Field, FieldError, FieldSet, Values};

use crate::app_state::{Profiles, next_id};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// Connection types offered by the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnKind {
    /// A database; the engine's form comes from [`engines::form`].
    Db(Engine),
    /// SSH Host.
    Ssh,
    /// SFTP over a Host.
    Sftp,
    /// FTP / FTPS.
    Ftp,
    /// Amazon S3 / S3-compatible object storage.
    S3,
}

impl ConnKind {
    /// The rail's entries: one for every database engine, then the remote kinds.
    const RAIL: [ConnKind; 5] = [
        ConnKind::Db(Engine::Postgres),
        ConnKind::Ssh,
        ConnKind::Sftp,
        ConnKind::Ftp,
        ConnKind::S3,
    ];

    /// Whether `self` and `other` share a rail entry.
    fn same_rail(self, other: ConnKind) -> bool {
        match (self, other) {
            (ConnKind::Db(_), ConnKind::Db(_)) => true,
            _ => self == other,
        }
    }

    fn badge(self) -> &'static str {
        match self {
            ConnKind::Db(_) => "DB",
            ConnKind::Ssh => "SSH",
            ConnKind::Sftp => "SFTP",
            ConnKind::Ftp => "FTP",
            ConnKind::S3 => "S3",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ConnKind::Db(e) => e.display_name(),
            ConnKind::Ssh => "SSH Host",
            ConnKind::Sftp => "SFTP",
            ConnKind::Ftp => "FTP / FTPS",
            ConnKind::S3 => "Amazon S3",
        }
    }

    fn rail_label(self) -> &'static str {
        match self {
            ConnKind::Db(_) => "Database",
            k => k.label(),
        }
    }

    fn is_db(self) -> bool {
        matches!(self, ConnKind::Db(_))
    }

    /// A database with a query editor (history of statements and coding agents apply).
    fn is_sql(self) -> bool {
        matches!(self, ConnKind::Db(e) if e.is_sql())
    }

    /// Whether coding agents may use it: every database, and Hosts (each command approved).
    fn agents_apply(self) -> bool {
        matches!(self, ConnKind::Db(_) | ConnKind::Ssh)
    }

    fn sub(self) -> &'static str {
        match self {
            ConnKind::Db(e) => e.display_name(),
            ConnKind::Ssh => "Terminal + tunnels",
            ConnKind::Sftp => "Files over a Host",
            ConnKind::Ftp => "Files, own login",
            ConnKind::S3 => "Buckets & objects",
        }
    }
}

/// Events for the workspace.
pub enum ConnEditorEvent {
    /// Close the dialog.
    Close,
    /// Show a toast.
    Toast(String),
}

#[derive(Clone, Debug, PartialEq)]
enum TestState {
    Idle,
    Testing(RequestId),
    Passed(String),
    Failed(String),
    Missing,
}

/// A select field: options (label, value) and the chosen index.
struct Select {
    options: Vec<(String, String)>,
    chosen: usize,
}

/// The editor.
pub struct ConnEditor {
    core: RuntimeHandle,
    profiles: Profiles,
    kind: ConnKind,
    /// The database engine last picked, restored when the rail goes back to Database.
    engine: Engine,
    existing_id: Option<ProfileId>,
    inputs: HashMap<&'static str, Entity<InputState>>,
    selects: HashMap<&'static str, Select>,
    open_select: Option<&'static str>,
    env: EnvironmentLabel,
    read_only: bool,
    history: bool,
    /// Coding agents (`swy mcp`) may use this connection.
    agents: bool,
    test: TestState,
    card: Option<driver_card::DriverCard>,
    driver_path: Entity<InputState>,
    components: Vec<Component>,
    error: Option<(Option<&'static str>, String)>,
    request: Option<RequestId>,
    connect_after: bool,
    /// A Host's port forwards.
    forwards: Vec<crate::forwards_editor::ForwardRow>,
    /// A Host's agent forwarding (`ssh -A`).
    forward_agent: bool,
    /// A Host's X11 forwarding (`ssh -X`).
    forward_x11: bool,
    /// A Host shown in the sidebar's Favorites.
    favorite: bool,
}

impl EventEmitter<ConnEditorEvent> for ConnEditor {}

fn text_input(
    window: &mut Window,
    cx: &mut Context<ConnEditor>,
    value: &str,
    placeholder: &str,
    masked: bool,
) -> Entity<InputState> {
    let v = value.to_owned();
    let ph = placeholder.to_owned();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(ph)
            .masked(masked)
            .default_value(v)
    })
}

impl ConnEditor {
    /// A new editor; `existing` edits a saved profile.
    pub fn new(
        core: RuntimeHandle,
        profiles: Profiles,
        kind: ConnKind,
        existing: Option<Profile>,
        components: Vec<Component>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = match &existing {
            Some(Profile::Db(d)) => ConnKind::Db(d.engine),
            Some(Profile::Host(_)) => ConnKind::Ssh,
            Some(Profile::File(f)) => match f.protocol {
                FileProtocol::Sftp { .. } => ConnKind::Sftp,
                FileProtocol::Ftp { .. } => ConnKind::Ftp,
                FileProtocol::S3 { .. } => ConnKind::S3,
            },
            _ => kind,
        };
        let mut this = Self {
            core,
            profiles,
            kind,
            engine: match kind {
                ConnKind::Db(e) => e,
                _ => Engine::Postgres,
            },
            existing_id: existing.as_ref().map(|p| p.id().clone()),
            inputs: HashMap::new(),
            selects: HashMap::new(),
            open_select: None,
            env: existing
                .as_ref()
                .map(|p| p.environment())
                .unwrap_or(EnvironmentLabel::Development),
            read_only: matches!(&existing, Some(Profile::Db(d)) if d.read_only),
            history: !matches!(&existing, Some(Profile::Db(d)) if !d.history_enabled),
            agents: match &existing {
                Some(Profile::Db(d)) => d.agent_access,
                Some(Profile::Host(h)) => h.agent_access,
                _ => false,
            },
            test: TestState::Idle,
            card: None,
            driver_path: driver_card::path_input(window, cx),
            components,
            error: None,
            request: None,
            connect_after: true,
            forwards: Vec::new(),
            forward_agent: matches!(&existing, Some(Profile::Host(h)) if h.forward_agent),
            forward_x11: matches!(&existing, Some(Profile::Host(h)) if h.forward_x11),
            favorite: matches!(&existing, Some(Profile::Host(h)) if h.favorite),
        };
        this.build_fields(existing.as_ref(), window, cx);
        this
    }

    /// The pending save request.
    pub fn request(&self) -> Option<RequestId> {
        self.request
    }

    /// Whether to open the connection after saving.
    pub fn connect_after_save(&self) -> bool {
        self.connect_after && self.kind.is_db()
    }

    fn host_options(&self, none_label: &str) -> Vec<(String, String)> {
        let mut v = vec![(none_label.to_owned(), String::new())];
        v.extend(
            self.profiles
                .hosts()
                .map(|h| (format!("{} · SSH tunnel", h.name), h.id.0.clone())),
        );
        v
    }

    fn build_fields(
        &mut self,
        existing: Option<&Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inputs.clear();
        self.selects.clear();
        self.open_select = None;
        self.error = None;
        self.test = TestState::Idle;
        let mut add = |this: &mut Self, key: &'static str, value: &str, ph: &str, masked: bool| {
            let i = text_input(window, cx, value, ph, masked);
            this.inputs.insert(key, i);
        };
        let sel = |options: Vec<(String, String)>, value: &str| {
            let chosen = options.iter().position(|(_, v)| v == value).unwrap_or(0);
            Select { options, chosen }
        };
        match self.kind {
            ConnKind::Db(engine) => {
                let form = engines::form(engine);
                let d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => form.new_profile(),
                };
                add(self, "name", &d.name, form.name_placeholder(), false);
                form.init(
                    &d,
                    &mut FieldSet {
                        editor: self,
                        window,
                        cx,
                    },
                );
            }
            ConnKind::Ssh => {
                let h = match existing {
                    Some(Profile::Host(h)) => h.clone(),
                    _ => Host::new("", "", std::env::var("USER").unwrap_or_default()),
                };
                add(self, "name", &h.name, "prod-db-01", false);
                add(self, "address", &h.address, "10.0.4.12", false);
                add(self, "port", &h.port.to_string(), "", false);
                add(self, "user", &h.user, "deploy", false);
                let key = match &h.auth {
                    SshAuth::PublicKey { key_path } => key_path.clone(),
                    _ => "~/.ssh/id_ed25519".into(),
                };
                add(self, "key", &key, "~/.ssh/id_ed25519", false);
                // Agents: which socket (1Password's or another) and which key.
                let agent_hint = match self
                    .components
                    .iter()
                    .find(|c| c.id == "ssh-agent")
                    .map(|c| &c.status)
                {
                    Some(ComponentStatus::Installed { location, .. })
                        if location.to_lowercase().contains("1password") =>
                    {
                        "Automatic · 1Password found".to_owned()
                    }
                    _ if cfg!(windows) => {
                        "Automatic · OpenSSH agent service, then Pageant".to_owned()
                    }
                    _ => "Automatic · SSH_AUTH_SOCK, then 1Password".to_owned(),
                };
                add(
                    self,
                    "agent_socket",
                    h.identity_agent.as_deref().unwrap_or_default(),
                    &agent_hint,
                    false,
                );
                add(
                    self,
                    "agent_key",
                    h.agent_key.as_deref().unwrap_or_default(),
                    "Any key the agent holds",
                    false,
                );
                add(
                    self,
                    "keepalive",
                    &h.keepalive_secs.to_string(),
                    "30",
                    false,
                );
                // Per-session settings (MX-6).
                add(
                    self,
                    "folder",
                    h.folder.as_deref().unwrap_or_default(),
                    "None",
                    false,
                );
                add(
                    self,
                    "startup_command",
                    h.startup_command.as_deref().unwrap_or_default(),
                    "tmux new -A -s main",
                    false,
                );
                add(
                    self,
                    "start_directory",
                    h.start_directory.as_deref().unwrap_or_default(),
                    "Home",
                    false,
                );
                add(
                    self,
                    "env",
                    &format_env(&h.env),
                    "LANG=C.UTF-8; TZ=UTC",
                    false,
                );
                let colors = h.terminal_colors.clone().unwrap_or_default();
                add(
                    self,
                    "fg",
                    colors.foreground.as_deref().unwrap_or_default(),
                    "Theme",
                    false,
                );
                add(
                    self,
                    "bg",
                    colors.background.as_deref().unwrap_or_default(),
                    "Theme",
                    false,
                );
                add(
                    self,
                    "x11_display",
                    h.x11_display.as_deref().unwrap_or_default(),
                    if cfg!(windows) {
                        "localhost:0"
                    } else {
                        "$DISPLAY"
                    },
                    false,
                );
                add(
                    self,
                    "password",
                    "",
                    if h.secret.is_some() {
                        "•••••••• (stored)"
                    } else {
                        ""
                    },
                    true,
                );
                let auth = match h.auth {
                    SshAuth::Password => "password",
                    SshAuth::PublicKey { .. } => "key",
                    SshAuth::KeyboardInteractive => "kbd",
                    SshAuth::Agent => "agent",
                };
                self.selects.insert(
                    "auth",
                    sel(
                        vec![
                            ("Public key".into(), "key".into()),
                            ("Password".into(), "password".into()),
                            ("Keyboard-interactive".into(), "kbd".into()),
                            ("SSH agent (1Password, OpenSSH)".into(), "agent".into()),
                        ],
                        if existing.is_some() { auth } else { "key" },
                    ),
                );
                let mut jumps = vec![("None".to_owned(), String::new())];
                jumps.extend(
                    self.profiles
                        .hosts()
                        .filter(|o| o.id != h.id)
                        .map(|o| (o.name.clone(), o.id.0.clone())),
                );
                self.selects.insert(
                    "jump",
                    sel(jumps, h.jump_hosts.first().map_or("", |j| j.0.as_str())),
                );
                let mut macros = vec![("None".to_owned(), String::new())];
                macros.extend(
                    crate::terminal_settings::macros(cx)
                        .into_iter()
                        .map(|m| (m.name, m.id)),
                );
                self.selects.insert(
                    "connect_macro",
                    sel(macros, h.connect_macro.as_deref().unwrap_or("")),
                );
            }
            ConnKind::Sftp => {
                let (name, host, path) = match existing {
                    Some(Profile::File(f)) => (
                        f.name.clone(),
                        match &f.protocol {
                            FileProtocol::Sftp { host_id } => host_id.0.clone(),
                            _ => String::new(),
                        },
                        f.default_path.clone().unwrap_or_default(),
                    ),
                    _ => (String::new(), String::new(), String::new()),
                };
                add(self, "name", &name, "prod-db-01 files", false);
                add(self, "path", &path, "/var/www/shop", false);
                let mut hosts: Vec<(String, String)> = self
                    .profiles
                    .hosts()
                    .map(|h| (format!("{} · reuses SSH session", h.name), h.id.0.clone()))
                    .collect();
                if hosts.is_empty() {
                    hosts.push(("No Hosts saved yet".into(), String::new()));
                }
                self.selects.insert("host", sel(hosts, &host));
            }
            ConnKind::Ftp => {
                let (name, server, port, tls, mode, user, path) = match existing {
                    Some(Profile::File(FileConnection {
                        name,
                        protocol:
                            FileProtocol::Ftp {
                                server,
                                port,
                                tls,
                                mode,
                                user,
                            },
                        default_path,
                        ..
                    })) => (
                        name.clone(),
                        server.clone(),
                        *port,
                        *tls,
                        *mode,
                        user.clone(),
                        default_path.clone().unwrap_or_default(),
                    ),
                    _ => (
                        String::new(),
                        String::new(),
                        21,
                        FtpTls::Explicit,
                        FtpMode::Passive,
                        String::new(),
                        String::new(),
                    ),
                };
                add(self, "name", &name, "assets.acme.dev", false);
                add(self, "server", &server, "ftp.assets.acme.dev", false);
                add(self, "port", &port.to_string(), "21", false);
                add(self, "user", &user, "deploy-assets", false);
                add(self, "password", "", "", true);
                add(self, "path", &path, "/public_html", false);
                self.selects.insert(
                    "tls",
                    sel(
                        vec![
                            ("Explicit (FTPES)".into(), "explicit".into()),
                            ("Implicit (FTPS)".into(), "implicit".into()),
                            ("None (plain FTP)".into(), "none".into()),
                        ],
                        match tls {
                            FtpTls::Explicit => "explicit",
                            FtpTls::Implicit => "implicit",
                            FtpTls::None => "none",
                        },
                    ),
                );
                self.selects.insert(
                    "mode",
                    sel(
                        vec![
                            ("Passive".into(), "passive".into()),
                            ("Active".into(), "active".into()),
                        ],
                        if mode == FtpMode::Active {
                            "active"
                        } else {
                            "passive"
                        },
                    ),
                );
            }
            ConnKind::S3 => {
                let (name, region, bucket, endpoint, path_style, auth_type, profile, access_key, path) = match existing {
                    Some(Profile::File(FileConnection {
                        name,
                        protocol:
                            FileProtocol::S3 {
                                region,
                                bucket,
                                endpoint,
                                force_path_style,
                                auth,
                            },
                        default_path,
                        ..
                    })) => {
                        let (auth_type, prof, key) = match auth {
                            S3Auth::Profile { profile } => ("profile", profile.clone(), String::new()),
                            S3Auth::Credentials { access_key_id, .. } => ("credentials", "default".to_string(), access_key_id.clone()),
                        };
                        (
                            name.clone(),
                            region.clone(),
                            bucket.clone().unwrap_or_default(),
                            endpoint.clone().unwrap_or_default(),
                            *force_path_style,
                            auth_type,
                            prof,
                            key,
                            default_path.clone().unwrap_or_default(),
                        )
                    }
                    _ => (
                        String::new(),
                        "us-east-1".to_string(),
                        String::new(),
                        String::new(),
                        false,
                        "profile",
                        "default".to_string(),
                        String::new(),
                        String::new(),
                    ),
                };
                add(self, "name", &name, "prod-s3-storage", false);
                add(self, "region", &region, "us-east-1", false);
                add(self, "bucket", &bucket, "my-bucket (empty for all)", false);
                add(self, "endpoint", &endpoint, "https://s3.amazonaws.com (optional)", false);
                add(self, "profile", &profile, "default", false);
                add(self, "access_key_id", &access_key, "AKIAIOSFODNN7EXAMPLE", false);
                add(self, "password", "", "Secret Access Key", true);
                add(self, "path", &path, "/prefix or empty", false);
                self.selects.insert(
                    "auth_type",
                    sel(
                        vec![
                            ("AWS Profile (~/.aws/)".into(), "profile".into()),
                            ("Access Key & Secret".into(), "credentials".into()),
                        ],
                        auth_type,
                    ),
                );
                self.selects.insert(
                    "path_style",
                    sel(
                        vec![
                            ("Virtual-host style (bucket.s3...)".into(), "virtual".into()),
                            ("Path style (MinIO / R2 / LocalStack)".into(), "path".into()),
                        ],
                        if path_style { "path" } else { "virtual" },
                    ),
                );
            }
        }
        if self.kind.is_sql() {
            let chosen = match existing {
                Some(Profile::Db(d)) => d.assistant_agent.clone().unwrap_or_default(),
                _ => String::new(),
            };
            let mut options = vec![("Default (Settings → Assistant)".to_owned(), String::new())];
            options.extend(
                crate::assistant_settings::AGENTS
                    .into_iter()
                    .map(|k| (k.display_name().to_owned(), k.id().to_owned())),
            );
            self.selects.insert("assistant", sel(options, &chosen));
        }
        self.forwards = match existing {
            Some(Profile::Host(h)) if self.kind == ConnKind::Ssh => h
                .forwards
                .iter()
                .map(|f| crate::forwards_editor::ForwardRow::new(f, window, cx))
                .collect(),
            _ => Vec::new(),
        };
    }

    /// A port forward row changed.
    fn forward_action(
        &mut self,
        action: crate::forwards_editor::RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::forwards_editor::{ForwardRow, RowAction};
        use switchyard_core::store::{ForwardDirection, PortForward};
        match action {
            RowAction::Add => {
                let f = PortForward::new(ForwardDirection::Local);
                self.forwards.push(ForwardRow::new(&f, window, cx));
            }
            RowAction::Direction(i, d) => {
                if let Some(r) = self.forwards.get_mut(i) {
                    r.set_direction(d);
                }
            }
            RowAction::ToggleAuto(i) => {
                if let Some(r) = self.forwards.get_mut(i) {
                    r.toggle_auto();
                }
            }
            RowAction::Remove(i) => {
                if i < self.forwards.len() {
                    self.forwards.remove(i);
                }
            }
        }
        cx.notify();
    }

    fn value(&self, key: &str, cx: &Context<Self>) -> String {
        self.inputs
            .get(key)
            .map(|i| i.read(cx).value().trim().to_owned())
            .unwrap_or_default()
    }

    fn chosen(&self, key: &str) -> String {
        self.selects
            .get(key)
            .and_then(|s| s.options.get(s.chosen))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn secret(&self, cx: &Context<Self>) -> Option<SecretString> {
        let pw = self
            .inputs
            .get("password")
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        (!pw.is_empty()).then(|| SecretString::from(pw))
    }

    fn port(&self, cx: &Context<Self>, default: u16) -> Result<u16, String> {
        let v = self.value("port", cx);
        if v.is_empty() {
            return Ok(default);
        }
        v.parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| "Port must be 1–65535".to_owned())
    }

    fn build(&self, cx: &Context<Self>) -> Result<Profile, FieldError> {
        let id = self.existing_id.clone().unwrap_or_default();
        let existing = self
            .existing_id
            .as_ref()
            .and_then(|i| self.profiles.all.iter().find(|p| p.id() == i));
        Ok(match self.kind {
            ConnKind::Db(engine) => {
                let form = engines::form(engine);
                let mut d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => form.new_profile(),
                };
                d.id = id;
                d.engine = engine;
                d.name = self.value("name", cx);
                form.apply(&Values { editor: self, cx }, &mut d)?;
                d.environment = self.env;
                d.read_only = self.read_only;
                d.history_enabled = self.history;
                d.agent_access = self.agents && self.kind.agents_apply();
                d.assistant_agent = Some(self.chosen("assistant")).filter(|a| !a.is_empty());
                Profile::Db(d)
            }
            ConnKind::Ssh => {
                let mut h = match existing {
                    Some(Profile::Host(h)) => h.clone(),
                    _ => Host::new("", "", ""),
                };
                h.id = id;
                h.name = self.value("name", cx);
                h.address = self.value("address", cx);
                h.port = self.port(cx, 22).map_err(|m| (Some("port"), m))?;
                h.user = self.value("user", cx);
                h.auth = match self.chosen("auth").as_str() {
                    "password" => SshAuth::Password,
                    "kbd" => SshAuth::KeyboardInteractive,
                    "agent" => SshAuth::Agent,
                    _ => SshAuth::PublicKey {
                        key_path: self.value("key", cx),
                    },
                };
                let opt = |v: String| (!v.is_empty()).then_some(v);
                h.identity_agent = opt(self.value("agent_socket", cx));
                h.agent_key = opt(self.value("agent_key", cx));
                h.keepalive_secs = self.value("keepalive", cx).parse().unwrap_or(30);
                h.forward_agent = self.forward_agent;
                h.forward_x11 = self.forward_x11;
                h.x11_display = opt(self.value("x11_display", cx));
                let jump = self.chosen("jump");
                h.connect_macro = Some(self.chosen("connect_macro")).filter(|m| !m.is_empty());
                h.favorite = self.favorite;
                h.folder = opt(self.value("folder", cx));
                h.startup_command = opt(self.value("startup_command", cx));
                h.start_directory = opt(self.value("start_directory", cx));
                h.env = parse_env(&self.value("env", cx)).map_err(|m| (Some("env"), m))?;
                let colors = TerminalColors {
                    foreground: opt(self.value("fg", cx)),
                    background: opt(self.value("bg", cx)),
                };
                h.terminal_colors = (!colors.is_empty()).then_some(colors);
                h.jump_hosts = if jump.is_empty() {
                    vec![]
                } else {
                    vec![ProfileId(jump)]
                };
                h.environment = self.env;
                h.agent_access = self.agents;
                h.forwards = self
                    .forwards
                    .iter()
                    .map(|r| r.read(cx))
                    .collect::<Result<_, _>>()
                    .map_err(|m| (None, m))?;
                Profile::Host(h)
            }
            ConnKind::Sftp => {
                let host = self.chosen("host");
                if host.is_empty() {
                    return Err((
                        Some("host"),
                        "Save a Host first; SFTP reuses its SSH session".into(),
                    ));
                }
                let path = self.value("path", cx);
                Profile::File(FileConnection {
                    id,
                    name: self.value("name", cx),
                    protocol: FileProtocol::Sftp {
                        host_id: ProfileId(host),
                    },
                    default_path: (!path.is_empty()).then_some(path),
                    environment: self.env,
                    folder: None,
                    secret: existing.and_then(|p| p.secret().cloned()),
                })
            }
            ConnKind::Ftp => Profile::File(FileConnection {
                id,
                name: self.value("name", cx),
                protocol: FileProtocol::Ftp {
                    server: self.value("server", cx),
                    port: self.port(cx, 21).map_err(|m| (Some("port"), m))?,
                    tls: match self.chosen("tls").as_str() {
                        "implicit" => FtpTls::Implicit,
                        "none" => FtpTls::None,
                        _ => FtpTls::Explicit,
                    },
                    mode: if self.chosen("mode") == "active" {
                        FtpMode::Active
                    } else {
                        FtpMode::Passive
                    },
                    user: self.value("user", cx),
                },
                default_path: Some(self.value("path", cx)).filter(|p| !p.is_empty()),
                environment: self.env,
                folder: None,
                secret: existing.and_then(|p| p.secret().cloned()),
            }),
            ConnKind::S3 => Profile::File(FileConnection {
                id,
                name: self.value("name", cx),
                protocol: FileProtocol::S3 {
                    region: {
                        let r = self.value("region", cx);
                        if r.is_empty() {
                            "us-east-1".to_string()
                        } else {
                            r
                        }
                    },
                    bucket: Some(self.value("bucket", cx)).filter(|b| !b.is_empty()),
                    endpoint: Some(self.value("endpoint", cx)).filter(|e| !e.is_empty()),
                    force_path_style: self.chosen("path_style") == "path",
                    auth: if self.chosen("auth_type") == "credentials" {
                        S3Auth::Credentials {
                            access_key_id: self.value("access_key_id", cx),
                            session_token: None,
                        }
                    } else {
                        let prof = self.value("profile", cx);
                        S3Auth::Profile {
                            profile: if prof.is_empty() {
                                "default".to_string()
                            } else {
                                prof
                            },
                        }
                    },
                },
                default_path: Some(self.value("path", cx)).filter(|p| !p.is_empty()),
                environment: self.env,
                folder: None,
                secret: existing.and_then(|p| p.secret().cloned()),
            }),
        })
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.build(cx) {
            Ok(profile) => {
                if let Err(v) = profile.validate() {
                    self.error = Some((Some(v.field), v.message));
                    cx.notify();
                    return;
                }
                let request = next_id();
                self.request = Some(request);
                self.error = None;
                self.core.send(Command::SaveProfile {
                    request,
                    profile,
                    secret: self.secret(cx),
                });
            }
            Err((field, msg)) => self.error = Some((field, msg)),
        }
        cx.notify();
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        match (self.kind, self.build(cx)) {
            (ConnKind::Db(engine), Ok(Profile::Db(d))) => {
                let missing = engines::form(engine).required_component(&d).filter(|id| {
                    self.components
                        .iter()
                        .any(|c| c.id == *id && !c.status.is_installed())
                });
                if let Some(id) = missing {
                    self.test = TestState::Missing;
                    if self.card.as_ref().is_none_or(|c| c.id != id) {
                        self.card = Some(driver_card::DriverCard::new(id));
                    }
                } else {
                    let request = next_id();
                    self.test = TestState::Testing(request);
                    self.core.send(Command::TestConnection {
                        request,
                        connection: d,
                        secret: self.secret(cx),
                    });
                }
            }
            (ConnKind::Ssh, Ok(Profile::Host(host))) => {
                let request = next_id();
                self.test = TestState::Testing(request);
                self.core.send(Command::TestHost {
                    request,
                    host,
                    secret: self.secret(cx),
                });
            }
            (ConnKind::Sftp, Ok(_)) => {
                self.test = TestState::Failed("SFTP ships in milestone M4 (russh-sftp)".into())
            }
            (ConnKind::Ftp | ConnKind::S3, Ok(Profile::File(connection))) => {
                let request = next_id();
                self.test = TestState::Testing(request);
                self.core.send(Command::TestFiles {
                    request,
                    connection,
                    secret: self.secret(cx),
                });
            }
            (_, Err((f, m))) => self.error = Some((f, m)),
            _ => {}
        }
        cx.notify();
    }

    /// A test result arrived.
    pub fn on_test_result(
        &mut self,
        request: RequestId,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.test == TestState::Testing(request) {
            self.test = match result {
                Ok(s) => TestState::Passed(s),
                Err(e) => TestState::Failed(e),
            };
            cx.notify();
        }
    }

    /// A save failed.
    pub fn set_error(
        &mut self,
        field: Option<&'static str>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.request = None;
        self.error = Some((field, message));
        cx.notify();
    }

    fn set_kind(&mut self, kind: ConnKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.existing_id.is_some() || kind == self.kind {
            return;
        }
        // Moving between engines keeps what was typed into fields they share.
        let carried: Vec<(&str, String)> = if kind.is_db() && self.kind.is_db() {
            ["name", "host", "user"]
                .into_iter()
                .map(|k| (k, self.value(k, cx)))
                .filter(|(_, v)| !v.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        if let ConnKind::Db(e) = kind {
            self.engine = e;
        }
        self.kind = kind;
        self.build_fields(None, window, cx);
        for (k, v) in carried {
            if let Some(i) = self.inputs.get(k) {
                i.update(cx, |i, cx| i.set_value(v, window, cx));
            }
        }
        cx.notify();
    }

    fn field(&self, f: &Field, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Field {
            key,
            label,
            span,
            mono,
            hint,
            browse,
        } = *f;
        let err = self
            .error
            .as_ref()
            .filter(|(f, _)| *f == Some(key))
            .map(|(_, m)| m.clone());
        let body: AnyElement = if let Some(input) = self.inputs.get(key) {
            div()
                .h(rpx(28.))
                .flex()
                .items_center()
                .px(rpx(9.))
                .border_1()
                .border_color(if err.is_some() { p.prod } else { p.bd2 })
                .rounded(px(6.))
                .bg(p.bg)
                .text_size(ts::UI)
                .when(mono, |d| d.font_family(MONO))
                .child(Input::new(input).appearance(false).text_size(ts::UI))
                .when(browse, |d| d.flex_1().min_w_0())
                .into_any_element()
        } else if let Some(sel) = self.selects.get(key) {
            let label = sel
                .options
                .get(sel.chosen)
                .map(|(l, _)| l.clone())
                .unwrap_or_default();
            let open = self.open_select == Some(key);
            let options: Vec<(usize, String)> = sel
                .options
                .iter()
                .enumerate()
                .map(|(i, (l, _))| (i, l.clone()))
                .collect();
            let chosen = sel.chosen;
            div()
                .relative()
                .child(
                    div()
                        .id(SharedString::from(format!("sel-{key}")))
                        .h(rpx(28.))
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .px(rpx(9.))
                        .border_1()
                        .border_color(if open { p.acc } else { p.bd2 })
                        .rounded(px(6.))
                        .bg(p.bg)
                        .text_size(ts::UI)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_select = if this.open_select == Some(key) {
                                None
                            } else {
                                Some(key)
                            };
                            cx.notify();
                        }))
                        .child(div().flex_1().truncate().child(label))
                        .child(div().text_color(p.fg3).text_size(ts::CAPTION).child("▾")),
                )
                .when(open, |d| {
                    d.child(deferred(
                        div()
                            .id(SharedString::from(format!("sel-menu-{key}")))
                            .absolute()
                            .top(rpx(30.))
                            .left_0()
                            .right_0()
                            .p(rpx(4.))
                            .bg(p.elev)
                            .rounded(px(7.))
                            .shadow(ui::shadow(p))
                            .occlude()
                            .children(options.into_iter().map(|(i, l)| {
                                div()
                                    .id(SharedString::from(format!("opt-{key}-{i}")))
                                    .h(rpx(26.))
                                    .flex()
                                    .items_center()
                                    .px(rpx(8.))
                                    .rounded(px(4.))
                                    .text_size(ts::UI)
                                    .when(i == chosen, |d| d.bg(p.sel))
                                    .hover(|s| s.bg(p.sel))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(s) = this.selects.get_mut(key) {
                                            s.chosen = i;
                                        }
                                        this.open_select = None;
                                        this.test = TestState::Idle;
                                        cx.notify();
                                    }))
                                    .child(l)
                            })),
                    ))
                })
                .into_any_element()
        } else {
            div().into_any_element()
        };
        let body = if browse {
            div()
                .flex()
                .gap(rpx(6.))
                .child(body)
                .child(
                    ui::button(
                        SharedString::from(format!("browse-{key}")),
                        "Browse…",
                        Kind::Secondary,
                        p,
                    )
                    .h(rpx(28.))
                    .on_click(cx.listener(move |this, _, w, cx| this.browse_file(key, w, cx))),
                )
                .into_any_element()
        } else {
            body
        };
        div()
            .col_span(span)
            .flex()
            .flex_col()
            .gap(rpx(5.))
            .min_w_0()
            .child(
                div()
                    .text_size(ts::LABEL)
                    .text_color(p.fg2)
                    .font_weight(FontWeight::MEDIUM)
                    .child(label.to_owned()),
            )
            .child(body)
            .when_some(err.or(hint.map(str::to_owned)), |d, h| {
                let is_err = self.error.as_ref().is_some_and(|(f, _)| *f == Some(key));
                d.child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(if is_err { p.prod } else { p.fg3 })
                        .child(h),
                )
            })
            .into_any_element()
    }

    /// Fills the `key` input with a local file the user picks.
    fn browse_file(&mut self, key: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.pop() else { return };
            let path = path.to_string_lossy().into_owned();
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(input) = this.inputs.get(key) {
                    input.update(cx, |i, cx| i.set_value(path, window, cx));
                }
                this.test = TestState::Idle;
                cx.notify();
            });
        })
        .detach();
    }

    /// The database engines, as a grid of tiles above the form.
    fn render_engine_picker(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(rpx(6.))
            .child(
                div()
                    .text_size(ts::LABEL)
                    .text_color(p.fg2)
                    .font_weight(FontWeight::MEDIUM)
                    .child("Database type"),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(4)
                    .gap(rpx(6.))
                    .children(engines::ALL.iter().map(|e| {
                        let e = *e;
                        let active = self.kind == ConnKind::Db(e);
                        div()
                            .id(SharedString::from(format!("engine-{}", e.badge())))
                            .min_w_0()
                            .h(rpx(32.))
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .px(rpx(8.))
                            .border_1()
                            .border_color(if active { p.acc } else { p.bd2 })
                            .rounded(px(6.))
                            .bg(if active { p.sel } else { p.bg })
                            .when(!active, |d| d.hover(|s| s.bg(p.hover)))
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.set_kind(ConnKind::Db(e), w, cx)
                            }))
                            .child(
                                div()
                                    .w(rpx(24.))
                                    .flex_none()
                                    .flex()
                                    .justify_center()
                                    .border_1()
                                    .border_color(if active { p.acc } else { p.bd2 })
                                    .rounded(px(3.))
                                    .text_color(if active { p.acc } else { p.fg2 })
                                    .font_family(MONO)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(ts::MICRO)
                                    .line_height(rpx(16.))
                                    .child(e.badge()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(ts::UI)
                                    .font_weight(FontWeight::MEDIUM)
                                    .truncate()
                                    .child(e.display_name()),
                            )
                    })),
            )
            .into_any_element()
    }

    /// The form's fields, in grid order.
    fn layout(&self, cx: &Context<Self>) -> Vec<Field> {
        let mut v = Vec::new();
        match self.kind {
            ConnKind::Db(engine) => {
                v.push(Field::new("name", "Name"));
                v.extend(engines::form(engine).layout(&Values { editor: self, cx }));
            }
            ConnKind::Ssh => {
                v.push(Field::new("name", "Name"));
                v.push(Field::new("address", "Address").span(4).mono());
                v.push(Field::new("port", "Port").span(2).mono());
                v.push(Field::new("user", "User").span(3));
                v.push(Field::new("auth", "Auth").span(3));
                match self.chosen("auth").as_str() {
                    "agent" => {
                        v.push(
                            Field::new("agent_socket", "Agent socket")
                                .span(3)
                                .mono()
                                .hint(if cfg!(windows) {
                                    "A named pipe (\\\\.\\pipe\\…), or pageant for PuTTY's Pageant"
                                } else {
                                    "1Password: ~/.1password/agent.sock (macOS: the Group Containers path)"
                                }),
                        );
                        v.push(
                            Field::new("agent_key", "Public key (optional)")
                                .span(3)
                                .mono()
                                .hint("Offer only this key, e.g. ~/.ssh/prod.pub; 1Password asks you to approve"),
                        );
                    }
                    "key" => {
                        v.push(
                            Field::new("key", "Key file")
                                .mono()
                                .hint("Read in place — never copied"),
                        );
                    }
                    "password" => {
                        v.push(Field::password("Password").span(6));
                    }
                    _ => {}
                }
                v.push(Field::new("jump", "Jump host").span(4).mono());
                v.push(Field::new("keepalive", "Keepalive (s)").span(2));
                v.push(
                    Field::new("folder", "Folder")
                        .span(3)
                        .hint("Groups Hosts in the sidebar"),
                );
                v.push(
                    Field::new("connect_macro", "Macro on connect")
                        .span(3)
                        .hint("Typed into each new shell (record one in a terminal)"),
                );
                v.push(Field::new("start_directory", "Start folder").span(3).mono());
                v.push(
                    Field::new("startup_command", "Startup command")
                        .span(3)
                        .mono(),
                );
                v.push(
                    Field::new("env", "Environment variables")
                        .span(6)
                        .mono()
                        .hint("NAME=value; separated by semicolons. The server must accept them (AcceptEnv)"),
                );
                v.push(
                    Field::new("fg", "Terminal text color")
                        .span(3)
                        .mono()
                        .hint("#rrggbb"),
                );
                v.push(
                    Field::new("bg", "Terminal background")
                        .span(3)
                        .mono()
                        .hint("#rrggbb"),
                );
                if self.forward_x11 {
                    v.push(
                        Field::new("x11_display", "X display")
                            .span(3)
                            .mono()
                            .hint("Empty: DISPLAY (Windows: VcXsrv/X410 on localhost:0)"),
                    );
                }
            }
            ConnKind::Sftp => {
                v.push(Field::new("name", "Name"));
                v.push(Field::new("host", "Host").hint("No second login"));
                v.push(Field::new("path", "Default remote path").mono());
            }
            ConnKind::Ftp => {
                v.push(Field::new("name", "Name"));
                v.push(Field::new("server", "Server").span(4).mono());
                v.push(Field::new("port", "Port").span(2).mono());
                v.push(Field::new("tls", "TLS").span(3));
                v.push(Field::new("mode", "Mode").span(3));
                v.push(Field::new("user", "User").span(3));
                v.push(Field::new("password", "Password").span(3));
                v.push(
                    Field::new("path", "Default remote path")
                        .mono()
                        .hint("Empty: the login folder"),
                );
            }
            ConnKind::S3 => {
                v.push(Field::new("name", "Name"));
                v.push(Field::new("auth_type", "Authentication").span(3));
                v.push(Field::new("region", "Region").span(3).mono().hint("e.g. us-east-1"));
                let is_profile = self.chosen("auth_type") != "credentials";
                if is_profile {
                    v.push(Field::new("profile", "AWS Profile").span(6).hint("Profile name from ~/.aws/credentials"));
                } else {
                    v.push(Field::new("access_key_id", "Access Key ID").span(3).mono());
                    v.push(Field::new("password", "Secret Access Key").span(3));
                }
                v.push(Field::new("bucket", "Bucket").span(3).mono().hint("Leave empty to list all accessible buckets"));
                v.push(Field::new("path_style", "Addressing Style").span(3));
                v.push(Field::new("endpoint", "Endpoint URL").span(6).mono().hint("Optional: e.g. http://localhost:9000 or Cloudflare R2"));
                v.push(
                    Field::new("path", "Default Prefix / Path")
                        .mono()
                        .hint("Optional initial folder or prefix"),
                );
            }
        }
        v
    }
}

impl Render for ConnEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let title = if self.existing_id.is_some() {
            format!("Edit {}", self.kind.label())
        } else if self.kind == ConnKind::Ssh {
            "New host".into()
        } else {
            "New connection".into()
        };
        let layout = self.layout(cx);
        let fields: Vec<AnyElement> = layout.iter().map(|f| self.field(f, &p, cx)).collect();
        let assistant_field = (self.kind.is_db() && self.agents).then(|| {
            div()
                .w(rpx(300.))
                .child(self.field(&Field::new("assistant", "Assistant CLI"), &p, cx))
        });
        let engine_picker = (self.kind.is_db() && self.existing_id.is_none())
            .then(|| self.render_engine_picker(&p, cx));
        let forwards = (self.kind == ConnKind::Ssh).then(|| {
            crate::forwards_editor::render(&self.forwards, &p, cx, |this: &mut Self, a, w, cx| {
                this.forward_action(a, w, cx)
            })
        });
        let (test_label, test_color, testing) = match &self.test {
            TestState::Idle => (String::new(), gpui_kit::transparent_black(), false),
            TestState::Testing(_) => ("Connecting…".into(), p.acc, true),
            TestState::Passed(s) => (s.clone(), p.dev, false),
            TestState::Failed(s) => (s.clone(), p.prod, false),
            TestState::Missing => ("A required component is missing".into(), p.stg, false),
        };
        let general_error = self
            .error
            .as_ref()
            .filter(|(f, _)| {
                f.is_none()
                    || !self.inputs.contains_key(f.unwrap_or(""))
                        && !self.selects.contains_key(f.unwrap_or(""))
            })
            .map(|(_, m)| m.clone());
        let is_db = self.kind.is_db();
        let _ = window;
        div()
            .id("conn-editor")
            .w(rpx(780.))
            .max_h(rpx(640.))
            .flex()
            .flex_col()
            .bg(p.elev)
            .rounded(px(10.))
            .shadow(ui::shadow(&p))
            .overflow_hidden()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(
                div()
                    .h(rpx(46.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(rpx(16.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::TITLE)
                            .child(title),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("ce-close")
                            .px(rpx(8.))
                            .py(rpx(2.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnEditorEvent::Close)))
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .w(rpx(190.))
                            .flex_none()
                            .p(rpx(8.))
                            .flex()
                            .flex_col()
                            .gap(rpx(2.))
                            .bg(p.panel)
                            .border_r_1()
                            .border_color(p.bd)
                            .children(ConnKind::RAIL.iter().map(|k| {
                                let active = k.same_rail(self.kind);
                                let k = match k {
                                    ConnKind::Db(_) if active => self.kind,
                                    ConnKind::Db(_) => ConnKind::Db(self.engine),
                                    k => *k,
                                };
                                let disabled = self.existing_id.is_some() && !active;
                                div()
                                    .id(SharedString::from(format!("ctype-{}", k.badge())))
                                    .flex()
                                    .items_center()
                                    .gap(rpx(9.))
                                    .px(rpx(8.))
                                    .py(rpx(7.))
                                    .rounded(px(6.))
                                    .when(active, |d| d.bg(p.sel))
                                    .when(!active && !disabled, |d| d.hover(|s| s.bg(p.hover)))
                                    .when(disabled, |d| d.opacity(0.4))
                                    .on_click(
                                        cx.listener(move |this, _, w, cx| this.set_kind(k, w, cx)),
                                    )
                                    .child(
                                        div()
                                            .w(rpx(32.))
                                            .flex_none()
                                            .flex()
                                            .justify_center()
                                            .border_1()
                                            .border_color(p.bd2)
                                            .rounded(px(3.))
                                            .text_color(p.fg2)
                                            .font_family(MONO)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_size(ts::MICRO)
                                            .line_height(rpx(16.))
                                            .child(k.badge()),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .min_w_0()
                                            .child(
                                                div()
                                                    .text_size(ts::UI)
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .whitespace_nowrap()
                                                    .child(k.rail_label()),
                                            )
                                            .child(
                                                div()
                                                    .text_size(ts::SMALL)
                                                    .text_color(p.fg3)
                                                    .truncate()
                                                    .child(k.sub()),
                                            ),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .id("ce-form")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .px(rpx(18.))
                            .py(rpx(16.))
                            .flex()
                            .flex_col()
                            .gap(rpx(14.))
                            .children(engine_picker)
                            .child(div().grid().grid_cols(6).gap(rpx(12.)).children(fields))
                            .children(forwards)
                            .when(self.kind == ConnKind::Ssh, |d| {
                                d.child(
                                    ui::checkbox(
                                        "fwd-agent",
                                        self.forward_agent,
                                        "Forward SSH agent (ssh -A): the Host can use your agent's keys while you are connected; only for Hosts you trust",
                                        &p,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.forward_agent = !this.forward_agent;
                                        cx.notify();
                                    })),
                                )
                                .child(
                                    ui::checkbox(
                                        "host-fav",
                                        self.favorite,
                                        "Show in Favorites at the top of the sidebar",
                                        &p,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.favorite = !this.favorite;
                                        cx.notify();
                                    })),
                                )
                                .child(
                                    ui::checkbox(
                                        "fwd-x11",
                                        self.forward_x11,
                                        "Forward X11 (ssh -X): graphical programs on the Host open windows here",
                                        &p,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.forward_x11 = !this.forward_x11;
                                        cx.notify();
                                    })),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(rpx(6.))
                                    .child(
                                        div()
                                            .text_size(ts::LABEL)
                                            .text_color(p.fg2)
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("Environment"),
                                    )
                                    .child(div().flex().gap(rpx(6.)).children(
                                        EnvironmentLabel::ALL.iter().map(|e| {
                                            let e = *e;
                                            let active = e == self.env;
                                            div()
                                                .id(SharedString::from(format!(
                                                    "env-{}",
                                                    e.badge()
                                                )))
                                                .flex_1()
                                                .h(rpx(30.))
                                                .flex()
                                                .items_center()
                                                .gap(rpx(8.))
                                                .px(rpx(10.))
                                                .border_1()
                                                .border_color(if active { p.env(e) } else { p.bd2 })
                                                .rounded(px(6.))
                                                .bg(if active { p.env_bg(e) } else { p.bg })
                                                .text_size(ts::UI)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.env = e;
                                                    cx.notify();
                                                }))
                                                .child(ui::dot(p.env(e), 8.))
                                                .child(e.name())
                                        }),
                                    ))
                                    .when(self.env == EnvironmentLabel::Production && is_db, |d| {
                                        d.child(
                                            div()
                                                .id("lock-ro")
                                                .flex()
                                                .items_center()
                                                .gap(rpx(8.))
                                                .text_size(ts::LABEL)
                                                .text_color(p.fg2)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.read_only = !this.read_only;
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .size(rpx(14.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .border_1()
                                                        .border_color(if self.read_only {
                                                            p.acc
                                                        } else {
                                                            p.bd2
                                                        })
                                                        .rounded(px(3.))
                                                        .bg(if self.read_only {
                                                            p.acc
                                                        } else {
                                                            p.surface
                                                        })
                                                        .text_color(p.acc_fg)
                                                        .text_size(ts::CAPTION)
                                                        .child(if self.read_only {
                                                            "✓"
                                                        } else {
                                                            ""
                                                        }),
                                                )
                                                .child(
                                                    "Destructive statements ask for confirmation.",
                                                )
                                                .child(
                                                    div().text_color(p.fg).child("Lock read-only"),
                                                ),
                                        )
                                    }),
                            )
                            .when(is_db, |d| {
                                d.child(
                                    ui::checkbox(
                                        "history",
                                        self.history,
                                        if self.kind == ConnKind::Db(Engine::Redis) {
                                            "Record console commands in query history (passwords are masked)"
                                        } else {
                                            "Record executed statements in query history"
                                        },
                                        &p,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.history = !this.history;
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .when(self.kind.agents_apply(), |d| {
                                let label = if self.kind == ConnKind::Ssh {
                                    "Allow coding agents (each command waits for your approval \
                                     in the assistant panel; every command is recorded in history)"
                                } else if self.kind == ConnKind::Db(Engine::Redis) {
                                    "Allow coding agents (read-only commands; every call is \
                                     recorded in history)"
                                } else if self.env.is_production() {
                                    "Allow coding agents (Production: read-only queries and \
                                     estimated plans; every call is recorded in history)"
                                } else {
                                    "Allow coding agents (read-only queries, plans, statistics; \
                                     every call is recorded in history)"
                                };
                                d.child(ui::checkbox("agents", self.agents, label, &p).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.agents = !this.agents;
                                        cx.notify();
                                    }),
                                ))
                            })
                            .children(assistant_field)
                            .when(self.test == TestState::Missing, |d| {
                                d.child(self.render_driver_card(&p, cx))
                            })
                            .when_some(general_error, |d, e| {
                                d.child(
                                    div()
                                        .px(rpx(10.))
                                        .py(rpx(6.))
                                        .rounded(px(6.))
                                        .bg(p.prod_bg)
                                        .text_size(ts::BODY)
                                        .text_color(p.fg)
                                        .child(e),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .h(rpx(52.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(10.))
                    .px(rpx(16.))
                    .border_t_1()
                    .border_color(p.bd)
                    .child(
                        ui::button("ce-test", "Test connection", Kind::Secondary, &p)
                            .h(rpx(28.))
                            .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(7.))
                            .min_w_0()
                            .text_size(ts::BODY)
                            .text_color(test_color)
                            .child(if testing {
                                ui::pulse_dot("test-dot", test_color, 6.)
                            } else {
                                ui::dot(test_color, 6.).into_any_element()
                            })
                            .child(div().truncate().child(test_label)),
                    )
                    .child(div().flex_1())
                    .child(
                        ui::button("ce-cancel", "Cancel", Kind::Ghost, &p)
                            .h(rpx(28.))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnEditorEvent::Close))),
                    )
                    .child(
                        ui::button(
                            "ce-save",
                            if is_db { "Save and connect" } else { "Save" },
                            Kind::Primary,
                            &p,
                        )
                        .h(rpx(28.))
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

/// Environment variables as `NAME=value; NAME2=value2`.
fn format_env(env: &[(String, String)]) -> String {
    env.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Parse [`format_env`]'s format; empty entries are skipped.
fn parse_env(s: &str) -> Result<Vec<(String, String)>, String> {
    s.split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| match e.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_owned(), v.trim().to_owned())),
            _ => Err(format!("\"{e}\" is not NAME=value")),
        })
        .collect()
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn env_round_trips_through_the_field() {
        let env = vec![
            ("LANG".to_owned(), "C.UTF-8".to_owned()),
            ("OPTS".to_owned(), "a=b c".to_owned()),
        ];
        assert_eq!(parse_env(&format_env(&env)).unwrap(), env);
        assert!(parse_env(" ; ").unwrap().is_empty());
        assert!(parse_env("NOVALUE").is_err());
        assert!(parse_env("=x").is_err());
    }
}
