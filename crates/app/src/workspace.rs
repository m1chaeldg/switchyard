//! The main window: title bar, sidebar, tabbed work area, inspector, status bar and
//! overlays. Owns UI state and routes runtime events to the views that need them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::component::TitleBar;
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    px,
};
use switchyard_core::drivers::Component;
use switchyard_core::handoff::Handoff;
use switchyard_core::store::{
    BufferState, DbConnection, EnvironmentLabel, HistoryEntry, Profile, ProfileId,
    Workspace as SavedWorkspace,
};
use switchyard_core::{Command, Event, EventReceiver, FsRef, RuntimeHandle, TermId};

use crate::actions::{self, CommandId};
use crate::app_state::{Profiles, SessionState, badge_of, describe, next_id};
use crate::appearance::{rpx, ts};
use crate::conn_editor::{ConnEditor, ConnEditorEvent};
use crate::editor_tab::EditorTab;
use crate::explorer::Explorer;
use crate::files_tab::{FilesTab, FilesTabEvent};
use crate::overlays::{Overlay, SettingsPage};
use crate::palette::{PaletteEvent, PaletteMode, PaletteView};
use crate::remote_files::{RemoteFiles, RemoteFilesEvent};
use crate::sidebar::SideTab;
use crate::sql_tab::{SqlTab, SqlTabEvent};
use crate::terminal_tab::TerminalTab;
use crate::theme::{self, MONO, Palette, SANS, ThemeId, palette};
use crate::transfers::Transfers;

/// Tab colors, one per open database (hues; red is left to Production).
const DB_HUES: [f32; 8] = [212.0, 145.0, 38.0, 275.0, 178.0, 322.0, 85.0, 24.0];
use crate::ui;

/// A tab in the work area.
pub enum Tab {
    /// Welcome page.
    Welcome,
    /// SQL editor.
    Sql(Entity<SqlTab>),
    /// Terminal.
    Terminal(Entity<TerminalTab>),
    /// File browser.
    Files(Entity<FilesTab>),
    /// A text file (usually on an SSH Host).
    Editor(Entity<EditorTab>),
    /// Query and index statistics of a database.
    Workload(Entity<crate::workload_tab::WorkloadTab>),
    /// Read-only DDL of a schema object.
    Ddl(Entity<crate::ddl_tab::DdlTab>),
    /// Properties of a table, view or materialized view (DBX-2b).
    Object(Entity<crate::object_tab::ObjectTab>),
    /// Sessions and running queries of a connection (DBX-5b).
    Activity(Entity<crate::activity_tab::ActivityTab>),
    /// ER diagram of a schema (DBX-5d).
    Er(Entity<crate::er_tab::ErTab>),
    /// Redis key browser and console.
    Redis(Entity<crate::redis_tab::RedisTab>),
}

/// The root view.
/// Which workspace the window shows (the title bar's "Default ▾" menu).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppMode {
    /// Databases, terminals and files.
    Default,
    /// The API client: collections, requests, environments.
    Api,
}

pub struct Workspace {
    /// Default (databases, terminals, files) or API.
    pub(crate) mode: AppMode,
    /// The workspace menu in the title bar is open.
    mode_menu: bool,
    /// The API workspace, created the first time it is shown.
    api: Option<Entity<crate::api::workbench::WorkbenchPanel>>,
    pub(crate) core: RuntimeHandle,
    pub(crate) profiles: Profiles,
    pub(crate) profiles_loaded: bool,
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    pub(crate) sidebar_open: bool,
    pub(crate) side_tab: SideTab,
    pub(crate) collapsed: HashSet<String>,
    /// The multi-connection schema explorer (DBX-5e).
    pub(crate) explorer: Explorer,
    pub(crate) inspector_open: bool,
    /// Width of the right panel (drag its left edge; saved as `inspector.width`).
    pub(crate) inspector_width: f32,
    /// Dragging the right panel's edge: (mouse x, width) when it started.
    pub(crate) inspector_drag: Option<(f32, f32)>,
    /// Width of the left sidebar (drag its right edge; saved as `sidebar.width`).
    pub(crate) sidebar_width: f32,
    /// Dragging the left sidebar's edge: (mouse x, width) when it started.
    pub(crate) sidebar_drag: Option<(f32, f32)>,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) toast: Option<SharedString>,
    /// Update checks and the newer release, if any (M6-4).
    pub(crate) updates: crate::updates::Updates,
    toast_task: Option<Task<()>>,
    /// Keeps relative times ("Cached 3 min ago") current under retained rendering.
    _clock: Task<()>,
    pub(crate) tunnels_open: bool,
    pub(crate) tunnels: Vec<switchyard_core::remote::ssh::TunnelInfo>,
    pub(crate) workspace_name: String,
    pub(crate) secret_backend: (&'static str, bool),
    pub(crate) components: Vec<Component>,
    /// Settings → Assistant, as saved.
    pub(crate) assistant: switchyard_core::agent_run::AssistantSettings,
    /// The Assistant settings page, once opened.
    pub(crate) assistant_view: Option<Entity<crate::assistant_settings::AssistantSettingsView>>,
    /// The Terminal settings page, once opened.
    pub(crate) terminal_view: Option<Entity<crate::terminal_settings::TerminalSettingsView>>,
    /// The assistant panel (right side).
    pub(crate) assistant_panel: Entity<crate::assistant_panel::AssistantPanel>,
    pub(crate) assistant_open: bool,
    pub(crate) drivers: crate::drivers_page::DriversPage,
    /// The sidebar Files panel per Host (kept while the app runs, so its folder is kept).
    pub(crate) remote_files: HashMap<String, Entity<RemoteFiles>>,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) focus: FocusHandle,
    pub(crate) overlay_focus: FocusHandle,
    pub(crate) ctx: Option<crate::sidebar::CtxMenu>,
    pub(crate) schema_search: Entity<InputState>,
    pub(crate) prompts: std::collections::VecDeque<crate::ssh_prompts::SshPrompt>,
    pending_open: Option<ProfileId>,
    /// A remote file the Files panel asked to open (needs the window).
    pending_editor: Option<(FsRef, PathBuf)>,
    /// The shared transfer queue (Files tab drawer, sidebar panel, status bar).
    pub(crate) transfers: Entity<Transfers>,
    /// Unsaved-changes dialog state (`crate::unsaved`).
    pub(crate) unsaved: crate::unsaved::UnsavedState,
    /// The sidebar asked for the Files tab with this Host.
    pending_files: Option<Option<ProfileId>>,
    rebind: Vec<(Entity<SqlTab>, Option<ProfileId>)>,
    /// Two tabs on screen at once.
    pub(crate) split: Option<crate::split::Split>,
    /// The value viewer's decoded image, kept while the same cell stays selected.
    pub(crate) viewer_image: Option<(u64, std::sync::Arc<gpui_kit::Image>)>,
    _events: Task<()>,
    _subs: Vec<Subscription>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Workspace {
    /// Build the workspace and start listening to runtime events.
    pub fn new(
        core: RuntimeHandle,
        mut events: EventReceiver,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let transfers = {
            let core = core.clone();
            cx.new(|_| Transfers::new(core))
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = events.next().await {
                if this
                    .update_in(cx, |w, window, cx| w.on_event(ev, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        core.send(Command::LoadProfiles);
        core.send(Command::LoadWorkspace);
        core.send(Command::LoadSetting {
            key: "theme".into(),
        });
        core.send(Command::LoadSetting {
            key: crate::appearance::APPEARANCE_KEY.into(),
        });
        core.send(Command::LoadSetting {
            key: "inspector.width".into(),
        });
        core.send(Command::LoadSetting {
            key: "sidebar.width".into(),
        });
        core.send(Command::LoadSetting {
            key: switchyard_core::agent_run::ASSISTANT_SETTINGS_KEY.into(),
        });
        core.send(Command::LoadSetting {
            key: switchyard_core::term_settings::TERMINAL_SETTINGS_KEY.into(),
        });
        core.send(Command::LoadMacros);
        core.send(Command::DetectComponents);
        crate::updates::load_setting(&core);
        // Pins and the explorer's connection nodes (DBX-5e).
        core.send(Command::LoadFavorites);
        core.send(Command::LoadSetting {
            key: crate::explorer::SAVED_NODES_KEY.into(),
        });
        // Closing the window with unsaved files asks first (native close buttons).
        let close_weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            close_weak
                .update(cx, |w, cx| w.request_close_window(window, cx))
                .unwrap_or(true)
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let schema_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search objects"));
        let assistant_core = core.clone();
        let assistant_panel =
            cx.new(|cx| crate::assistant_panel::AssistantPanel::new(assistant_core, window, cx));
        let assistant_sub = cx.subscribe_in(
            &assistant_panel,
            window,
            |this, _, ev: &crate::assistant_panel::AssistantPanelEvent, window, cx| {
                this.on_assistant_event(ev, window, cx)
            },
        );
        let search_sub = cx.subscribe(
            &schema_search,
            |this, input, ev: &gpui_kit::component::input::InputEvent, cx| {
                if let gpui_kit::component::input::InputEvent::Change = ev {
                    let filter = input.read(cx).value().trim().to_owned();
                    let core = this.core.clone();
                    if let Some((conn, ticket)) = this.explorer.filter_changed(filter, &core) {
                        // Debounced server search; a newer keystroke makes the ticket stale.
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(crate::object_search::DEBOUNCE)
                                .await;
                            let _ = this.update(cx, |this, cx| {
                                let core = this.core.clone();
                                this.explorer.search_due(&conn, ticket, &core);
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                    cx.notify();
                }
            },
        );
        // Toasts from the API workspace (queued in a global by its notify shim).
        let toast_sub = cx.observe_global::<crate::api::compat::notify::Toasts>(|this, cx| {
            for text in crate::api::compat::notify::drain(cx) {
                this.toast(text, cx);
            }
        });
        Self {
            mode: AppMode::Default,
            mode_menu: false,
            api: None,
            core,
            profiles: Profiles::default(),
            profiles_loaded: false,
            tabs: vec![Tab::Welcome],
            active: 0,
            sidebar_open: true,
            side_tab: SideTab::Connections,
            collapsed: HashSet::new(),
            explorer: Explorer::default(),
            inspector_open: window.bounds().size.width > px(1280.),
            inspector_width: crate::sidebar::INSPECTOR_WIDTH,
            inspector_drag: None,
            sidebar_width: crate::sidebar::SIDEBAR_WIDTH,
            sidebar_drag: None,
            overlay: None,
            toast: None,
            updates: crate::updates::Updates::default(),
            toast_task: None,
            _clock: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(30))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }),
            tunnels_open: false,
            tunnels: Vec::new(),
            workspace_name: "Default".into(),
            secret_backend: ("", false),
            components: Vec::new(),
            assistant: Default::default(),
            assistant_view: None,
            terminal_view: None,
            assistant_panel,
            assistant_open: false,
            drivers: Default::default(),
            remote_files: HashMap::new(),
            history: Vec::new(),
            focus,
            overlay_focus: cx.focus_handle(),
            ctx: None,
            schema_search,
            prompts: std::collections::VecDeque::new(),
            pending_open: None,
            pending_editor: None,
            transfers,
            unsaved: Default::default(),
            pending_files: None,
            rebind: Vec::new(),
            split: None,
            viewer_image: None,
            _events: task,
            _subs: vec![search_sub, toast_sub, assistant_sub],
        }
    }

    // ---------------------------------------------------------------- events

    fn terminal_tab(&self, term: TermId, cx: &App) -> Option<Entity<TerminalTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Terminal(t) if t.read(cx).owns(term) => Some(t.clone()),
            // The terminal under an editor.
            Tab::Editor(e) => e
                .read(cx)
                .terminal
                .clone()
                .filter(|t| t.read(cx).owns(term)),
            _ => None,
        })
    }

    fn on_event(&mut self, ev: Event, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            Event::Pong { .. } => {}
            ev @ (Event::RedisOpened { .. }
            | Event::RedisKeys { .. }
            | Event::RedisKey { .. }
            | Event::RedisEdited { .. }
            | Event::RedisReply { .. }) => self.on_redis_event(ev, window, cx),
            Event::TerminalOpened {
                term,
                terminal,
                description,
            } => match self.terminal_tab(term, cx) {
                Some(t) => t.update(cx, |t, cx| {
                    t.on_opened(term, terminal, description, window, cx)
                }),
                None => self.core.send(Command::CloseTerminal { term }),
            },
            Event::TerminalFailed { term, message } => {
                if let Some(t) = self.terminal_tab(term, cx) {
                    t.update(cx, |t, cx| t.on_failed(term, message, cx));
                }
            }
            Event::TerminalWake { term } => {
                if let Some(t) = self.terminal_tab(term, cx) {
                    t.update(cx, |t, cx| t.on_wake(term, cx));
                }
            }
            Event::TerminalTitle { term, title } => {
                if let Some(t) = self.terminal_tab(term, cx) {
                    t.update(cx, |t, cx| t.on_title(term, title, cx));
                }
            }
            Event::TerminalBell { .. } => {}
            Event::Tunnels(list) => {
                self.tunnels = list;
                cx.notify();
            }
            Event::TerminalStatus { term, status } => {
                if let Some(t) = self.terminal_tab(term, cx) {
                    t.update(cx, |t, cx| t.on_status(term, status, cx));
                }
            }
            Event::HostKeyChanged {
                term,
                host_id,
                host,
                address,
                stored,
                received,
                location,
            } => {
                let tab = term.and_then(|t| self.terminal_tab(t, cx).map(|tab| (t, tab)));
                match tab {
                    Some((term, tab)) => tab.update(cx, |t, cx| {
                        t.on_host_key_changed(
                            term,
                            crate::terminal_tab::ChangedKey {
                                host_id,
                                host,
                                address,
                                stored,
                                received,
                                location,
                            },
                            cx,
                        )
                    }),
                    None => self.toast(format!("Blocked: the host key for {host} has changed"), cx),
                }
            }
            Event::HostKeyPrompt { request, key } => self.push_host_key_prompt(request, key, cx),
            Event::SecretPrompt {
                request,
                host,
                prompt,
            } => self.push_secret_prompt(request, host, prompt, window, cx),
            Event::InteractivePrompt { request, req } => {
                self.push_interactive_prompt(request, req, window, cx)
            }
            Event::EntraSignIn {
                request,
                connection,
                url,
            } => self.push_entra_prompt(request, connection, url, None, cx),
            Event::EntraDeviceCode {
                request,
                connection,
                code,
                url,
                message,
            } => self.push_entra_prompt(request, connection, url, Some((code, message)), cx),
            Event::PromptClosed { request } => self.close_prompt(request, window, cx),
            Event::TerminalClipboard { text, .. } => {
                // OSC 52 copy: allowed (it only writes); reading the clipboard is never offered.
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
            }
            Event::Macros(list) => {
                crate::terminal_settings::set_macros(list, cx);
                cx.notify();
            }
            Event::TerminalLog { term, state } => {
                if let switchyard_core::TermLogState::Failed(m) = &state {
                    self.toast(format!("Terminal log: {m}"), cx);
                }
                if let Some(t) = self.terminal_tab(term, cx) {
                    t.update(cx, |t, cx| t.on_log(term, state, cx));
                }
            }
            Event::TerminalExited {
                term,
                code,
                message,
            } => {
                if let Some(t) = self.terminal_tab(term, cx) {
                    t.update(cx, |t, cx| t.on_exited(term, code, message, cx));
                }
            }
            Event::Profiles(list) => {
                self.profiles = Profiles { all: list };
                self.profiles_loaded = true;
                // The schema explorer lists SQL databases; Redis opens its own browser.
                let dbs: Vec<_> = self
                    .profiles
                    .dbs()
                    .filter(|d| d.engine.is_sql())
                    .cloned()
                    .collect();
                let core = self.core.clone();
                self.explorer.set_profiles(dbs, &core);
                let sources = self.file_sources();
                for t in &self.tabs {
                    if let Tab::Files(f) = t {
                        f.update(cx, |f, cx| f.set_sources(sources.clone(), cx));
                    }
                }
                // Refresh connection details held by tabs.
                for t in &self.tabs {
                    if let Tab::Sql(tab) = t {
                        let id = tab.read(cx).connection.as_ref().map(|c| c.id.clone());
                        if let Some(id) = id {
                            let fresh = self.profiles.db(&id).cloned();
                            tab.update(cx, |t, cx| {
                                if fresh.is_none() {
                                    t.set_connection(None, cx);
                                } else {
                                    t.connection = fresh;
                                    cx.notify();
                                }
                            });
                        }
                    }
                }
                if let Some(Overlay::Palette(p)) = &self.overlay {
                    let profiles = self.profiles.clone();
                    p.update(cx, |p, cx| p.set_profiles(profiles, cx));
                }
            }
            Event::ProfileSaved { request, id } => {
                self.reconnect_failed(&id, cx);
                if let Some(Overlay::ConnEditor(ed)) = &self.overlay
                    && ed.read(cx).request() == Some(request)
                {
                    let connect = ed.read(cx).connect_after_save();
                    self.overlay = None;
                    self.toast("Saved · secrets stored in the secret store", cx);
                    if connect {
                        self.pending_open = Some(id);
                    }
                }
            }
            Event::ProfileError {
                request,
                field,
                message,
            } => {
                if let Some(Overlay::ConnEditor(ed)) = &self.overlay
                    && ed.read(cx).request() == Some(request)
                {
                    ed.update(cx, |ed, cx| ed.set_error(field, message, cx));
                }
            }
            Event::Setting { key, value } if key == "inspector.width" => {
                if let Some(w) = value.as_ref().and_then(|v| v.as_f64()) {
                    self.inspector_width = (w as f32).max(crate::sidebar::INSPECTOR_MIN);
                }
            }
            Event::Setting { key, value } if key == crate::explorer::SAVED_NODES_KEY => {
                let ids: Vec<ProfileId> = value
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(ProfileId)
                    .collect();
                self.explorer.restore(ids);
            }
            Event::Setting { key, value } if key == crate::appearance::APPEARANCE_KEY => {
                if let Some(mut s) = value.and_then(|v| {
                    serde_json::from_value::<crate::appearance::AppearanceSettings>(v).ok()
                }) {
                    if let Some(z) = crate::appearance::env_zoom() {
                        s.zoom = z;
                    }
                    crate::appearance::set(s, cx);
                    theme::apply_fonts(Some(window), cx);
                }
            }
            Event::Setting { key, value } if key == switchyard_core::update::CHECK_SETTING => {
                self.on_update_setting(value);
            }
            Event::UpdateStatus { manual, status } => self.on_update_status(manual, status, cx),
            Event::Setting { key, value } if key == "sidebar.width" => {
                if let Some(w) = value.as_ref().and_then(|v| v.as_f64()) {
                    self.sidebar_width = (w as f32).max(crate::sidebar::SIDEBAR_MIN);
                }
            }
            Event::Setting { key, value }
                if key == switchyard_core::term_settings::TERMINAL_SETTINGS_KEY =>
            {
                let s = value
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                crate::terminal_settings::apply(s, cx);
            }
            Event::Setting { key, value }
                if key == switchyard_core::agent_run::ASSISTANT_SETTINGS_KEY =>
            {
                if let Some(s) = value.and_then(|v| {
                    serde_json::from_value::<switchyard_core::agent_run::AssistantSettings>(v).ok()
                }) {
                    self.assistant = s.clone();
                    self.assistant_panel
                        .update(cx, |p, cx| p.set_settings(s, cx));
                }
            }
            Event::Setting { key, value } => {
                // SWITCHYARD_THEME (tests, screenshots) wins over the saved choice.
                if key == "theme"
                    && std::env::var_os("SWITCHYARD_THEME").is_none()
                    && let Some(v) = value.as_ref().and_then(|v| v.as_str())
                {
                    let id = ThemeId::from_key(v);
                    if id != palette(cx).id {
                        theme::apply(id.palette(), Some(window), cx);
                    }
                }
            }
            Event::SecretBackend { name, locked } => {
                self.secret_backend = (name, locked);
            }
            Event::TestResult { request, result } => {
                if let Some(Overlay::ConnEditor(ed)) = &self.overlay {
                    ed.update(cx, |ed, cx| ed.on_test_result(request, result, cx));
                }
            }
            Event::SessionOpened {
                session,
                server_version,
            } => {
                let core = self.core.clone();
                if self
                    .explorer
                    .on_open(session, server_version.clone(), &core)
                    .is_some()
                {
                    self.explorer.try_reveal();
                }
                if let Some(o) = self.object_tab_for_session(session, cx) {
                    let version = server_version.clone();
                    o.update(cx, |o, cx| o.on_session(SessionState::Open { version }, cx));
                }
                if let Some(e) = self.er_tab_for_session(session, cx) {
                    let version = server_version.clone();
                    e.update(cx, |e, cx| e.on_session(SessionState::Open { version }, cx));
                }
                self.for_sql_session(session, cx, |t, cx| {
                    t.on_session(
                        SessionState::Open {
                            version: server_version.clone(),
                        },
                        cx,
                    )
                });
            }
            Event::SessionFailed { session, message } => {
                if let Some(id) = self.explorer.conn_of_session(session)
                    && let Some(s) = self.explorer.state_mut(&id)
                {
                    s.on_failed(message.clone());
                    self.explorer.try_reveal();
                }
                if let Some(o) = self.object_tab_for_session(session, cx) {
                    let failed = SessionState::Failed(message.clone());
                    o.update(cx, |o, cx| o.on_session(failed, cx));
                }
                if let Some(e) = self.er_tab_for_session(session, cx) {
                    let failed = SessionState::Failed(message.clone());
                    e.update(cx, |e, cx| e.on_session(failed, cx));
                }
                self.for_sql_session(session, cx, |t, cx| {
                    t.on_session(SessionState::Failed(message.clone()), cx)
                });
                self.toast(format!("Connection failed: {message}"), cx);
            }
            Event::Query { query, event } => {
                if let Some(o) = self.object_tab_for_query(query, cx) {
                    o.update(cx, |o, cx| o.on_query(event, window, cx));
                } else {
                    for t in &self.tabs {
                        if let Tab::Sql(tab) = t
                            && tab.read(cx).owns_query(query)
                        {
                            let tab = tab.clone();
                            tab.update(cx, |t, cx| t.on_query(event, window, cx));
                            break;
                        }
                    }
                }
            }
            Event::SessionContext {
                session,
                request,
                result,
            } => {
                self.for_sql_session(session, cx, |t, cx| t.on_context(request, result, cx));
            }
            Event::Transaction {
                session,
                open,
                statements,
            } => {
                self.for_sql_session(session, cx, |t, cx| t.on_transaction(open, statements, cx));
            }
            Event::Catalog {
                session,
                scope,
                result,
                cached_at,
            } => {
                if let Some(conn) = self.explorer.conn_of_session(session) {
                    if matches!(
                        scope,
                        switchyard_core::db::IntrospectScope::Detail { .. }
                            | switchyard_core::db::IntrospectScope::RoutineDefinition { .. }
                    ) {
                        // Object detail asked for by a schema-tree action (template, DDL,
                        // Script as) or an expanded relation.
                        self.on_schema_detail(conn, scope, result, window, cx);
                    } else if let Some(s) = self.explorer.state_mut(&conn) {
                        s.on_catalog(scope, result, cached_at);
                        self.explorer.try_reveal();
                    }
                } else if let Some(o) = self.object_tab_for_session(session, cx) {
                    o.update(cx, |o, cx| o.on_catalog(scope, result, window, cx));
                } else if let Some(e) = self.er_tab_for_session(session, cx) {
                    e.update(cx, |e, cx| e.on_catalog(scope, result, cx));
                } else {
                    match result {
                        Ok(chunk) => {
                            self.for_sql_session(session, cx, |t, cx| t.on_catalog(chunk, cx))
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "catalog load failed");
                            self.for_sql_session(session, cx, |t, cx| {
                                t.on_catalog_failed(&scope, cx)
                            })
                        }
                    }
                }
            }
            Event::Snippets(list) => crate::snippets::on_snippets(list, cx),
            Event::Favorites(list) => {
                let core = self.core.clone();
                self.explorer.set_favorites(list, &core);
            }
            Event::History { request, entries } => {
                let redis = self.tabs.iter().find_map(|t| match t {
                    Tab::Redis(r) if r.read(cx).owns_history(request) => Some(r.clone()),
                    _ => None,
                });
                if let Some(tab) = redis {
                    tab.update(cx, |t, _| t.on_history(&entries));
                } else {
                    match self.plan_tab(cx, |v| v.owns_history(request)) {
                        Some(tab) => tab.update(cx, |t, cx| {
                            t.plan_view().update(cx, |v, cx| v.on_history(entries, cx))
                        }),
                        None => self.history = entries,
                    }
                }
            }
            Event::Plan {
                request,
                history_id,
                plan,
                findings,
            } => {
                if let Some(tab) = self.plan_tab(cx, |v| v.owns(request)) {
                    tab.update(cx, |t, cx| {
                        t.plan_view()
                            .update(cx, |v, cx| v.on_plan(history_id, plan, findings, cx));
                        cx.notify();
                    });
                }
            }
            Event::Workload { request, result } => {
                let tab = self.tabs.iter().find_map(|t| match t {
                    Tab::Workload(w) if w.read(cx).owns(request) => Some(w.clone()),
                    _ => None,
                });
                if let Some(tab) = tab {
                    tab.update(cx, |t, cx| t.on_result(result, cx));
                }
            }
            Event::Activity { request, result } => self.on_activity(request, result, window, cx),
            Event::SessionAction { request, result } => self.on_session_action(request, result, cx),
            Event::WhatIf { request, result } => {
                if let Some(tab) = self.plan_tab(cx, |v| v.owns(request)) {
                    tab.update(cx, |t, cx| {
                        t.plan_view().update(cx, |v, cx| v.on_what_if(result, cx));
                        cx.notify();
                    });
                }
            }
            Event::Handoff(Handoff::OpenPlan { history_id, sql }) => {
                self.open_saved_plan(history_id, &sql, window, cx);
                window.activate_window();
            }
            // Agent queries come from `swy mcp`, which reads its own events.
            Event::AgentRows { .. } => {}
            Event::PlanFailed {
                request,
                error,
                needs_confirmation,
            } => {
                if let Some(tab) = self.plan_tab(cx, |v| v.owns(request)) {
                    tab.update(cx, |t, cx| {
                        t.plan_view()
                            .update(cx, |v, cx| v.on_failed(error, needs_confirmation, cx));
                        cx.notify();
                    });
                }
            }
            Event::Workspace(w) => self.restore(w, window, cx),
            Event::FsListing { .. }
            | Event::FsOpDone { .. }
            | Event::TransferQueued { .. }
            | Event::TransferProgress { .. }
            | Event::TransferDone { .. } => {
                self.transfers.update(cx, |t, cx| t.on_event(&ev, cx));
                for panel in self.remote_files.values() {
                    panel.update(cx, |p, cx| p.on_event(&ev, cx));
                }
                for t in &self.tabs {
                    if let Tab::Files(f) = t {
                        f.update(cx, |f, cx| f.on_event(&ev, cx));
                    }
                }
            }
            Event::TextFileRead { .. } | Event::TextFileSaved { .. } => {
                for t in &self.tabs {
                    if let Tab::Editor(e) = t {
                        e.update(cx, |e, cx| e.on_event(&ev, window, cx));
                    }
                }
                self.finish_saved_closes(window, cx);
                // A saved file's size and time changed in the Files panel.
                if matches!(ev, Event::TextFileSaved { result: Ok(_), .. }) {
                    for panel in self.remote_files.values() {
                        panel.update(cx, |p, cx| p.refresh(cx));
                    }
                }
            }
            Event::DirListing { .. } => {}
            Event::Agent { run, agent, event } => {
                self.assistant_panel
                    .update(cx, |p, cx| p.on_agent_event(run, agent, event, cx));
            }
            Event::AgentApproval(approval) => {
                // A command waits for the user: make sure they see it.
                self.assistant_open = true;
                self.assistant_panel
                    .update(cx, |p, cx| p.on_approval(approval, cx));
                cx.notify();
            }
            Event::AgentApprovalClosed { id } => {
                self.assistant_panel
                    .update(cx, |p, cx| p.on_approval_closed(id, cx));
            }
            Event::Components(_)
            | Event::ComponentProgress { .. }
            | Event::ComponentInstalled { .. }
            | Event::ComponentFailed { .. } => self.on_component_event(ev, cx),
            Event::EditsApplied {
                request,
                result,
                elapsed,
            } => {
                for t in &self.tabs {
                    if let Tab::Sql(tab) = t
                        && tab.read(cx).owns_edit_request(request)
                    {
                        let tab = tab.clone();
                        tab.update(cx, |t, cx| t.on_edits_applied(result, elapsed, window, cx));
                        break;
                    }
                }
            }
            Event::Toast(t) => self.toast(t, cx),
            Event::SshConfigPreview { path, hosts } => {
                let chosen = hosts
                    .iter()
                    .filter(|h| !h.exists)
                    .map(|h| h.alias.clone())
                    .collect();
                self.overlay = Some(Overlay::SshImport(crate::overlays::SshImportPreview {
                    path,
                    hosts,
                    chosen,
                }));
                window.focus(&self.overlay_focus, cx);
                cx.notify();
            }
            Event::FolderSynced {
                files_checked,
                files_transferred,
                bytes_transferred,
                error,
                ..
            } => {
                if let Some(err) = error {
                    self.toast(format!("Folder sync error: {err}"), cx);
                } else {
                    self.toast(
                        format!(
                            "Sync complete: {files_transferred} transferred of {files_checked} checked ({bytes_transferred} bytes)"
                        ),
                        cx,
                    );
                }
            }
            Event::Error { context, message } => self.toast(format!("{context}: {message}"), cx),
        }
        if let Some(id) = self.pending_open.clone()
            && self.profiles.db(&id).is_some()
        {
            self.pending_open = None;
            self.open_connection(&id, window, cx);
        }
        cx.notify();
    }

    fn for_sql_session(
        &self,
        session: u64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut SqlTab, &mut Context<SqlTab>),
    ) {
        for t in &self.tabs {
            if let Tab::Sql(tab) = t
                && tab.read(cx).owns_session(session)
            {
                tab.update(cx, f);
                return;
            }
        }
    }

    fn restore(&mut self, w: SavedWorkspace, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_name = w.name.clone();
        self.sidebar_open = !w.sidebar_collapsed;
        let mut active = None;
        for (i, b) in w.buffers.into_iter().enumerate() {
            let conn = b
                .connection_id
                .as_ref()
                .and_then(|id| self.profiles.db(id).cloned());
            let pending = conn.is_none() && b.connection_id.is_some();
            let id = b.id.clone();
            let conn_id = b.connection_id.clone();
            let tab = self.new_sql_tab(b, conn, i as i64, window, cx);
            if pending {
                // Profiles may not be loaded yet; bind once they are.
                self.rebind.push((tab.clone(), conn_id));
            }
            if w.active_buffer.as_deref() == Some(id.as_str()) {
                active = Some(self.tabs.len() - 1);
            }
        }
        if let Some(a) = active {
            self.active = a;
        } else if self.tabs.len() > 1 {
            self.active = 1;
        }
        self.sync_schema(cx);
    }

    // ------------------------------------------------------------------ tabs

    fn new_sql_tab(
        &mut self,
        buffer: BufferState,
        conn: Option<DbConnection>,
        position: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SqlTab> {
        let core = self.core.clone();
        let tab = cx.new(|cx| SqlTab::new(core, buffer, conn, position, window, cx));
        let sub = cx.subscribe_in(&tab, window, |this, tab, ev: &SqlTabEvent, window, cx| {
            this.on_tab_event(tab.clone(), ev, window, cx)
        });
        self._subs.push(sub);
        self.tabs.push(Tab::Sql(tab.clone()));
        tab
    }

    fn on_tab_event(
        &mut self,
        tab: Entity<SqlTab>,
        ev: &SqlTabEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            SqlTabEvent::PickConnection => self.open_palette(PaletteMode::Connections, window, cx),
            SqlTabEvent::Toast(t) if t == "export-file" => self.export_to_file(tab, window, cx),
            SqlTabEvent::Toast(t) => self.toast(t.clone(), cx),
            SqlTabEvent::ConfirmDestructive(p) => {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder("type the object name"));
                input.update(cx, |i, cx| i.focus(window, cx));
                self.overlay = Some(Overlay::Safety {
                    tab,
                    pending: p.clone(),
                    input,
                });
            }
            SqlTabEvent::PromptParams(p) => {
                let mut names: Vec<String> = Vec::new();
                for n in p.params.iter().flatten() {
                    if !names.contains(n) {
                        names.push(n.clone());
                    }
                }
                let inputs: Vec<(String, Entity<InputState>)> = names
                    .into_iter()
                    .map(|n| {
                        let i = cx.new(|cx| {
                            InputState::new(window, cx).placeholder("value (NULL for null)")
                        });
                        (n, i)
                    })
                    .collect();
                if let Some((_, first)) = inputs.first() {
                    first.update(cx, |i, cx| i.focus(window, cx));
                }
                self.overlay = Some(Overlay::Params {
                    tab,
                    pending: p.clone(),
                    inputs,
                });
            }
            SqlTabEvent::Changed => {
                self.sync_schema(cx);
                self.sync_assistant(cx);
            }
            SqlTabEvent::OpenData {
                schema,
                name,
                filter,
            } => {
                if let Some(conn) = tab.read(cx).connection.clone() {
                    let (s, n, f) = (schema.clone(), name.clone(), Some(filter.clone()));
                    let table = switchyard_core::db::ObjectKind::Table;
                    self.open_table_data(conn, s, n, table, f, window, cx);
                }
            }
            SqlTabEvent::Optimize { sql, findings } => {
                self.assistant_open = true;
                self.sync_assistant(cx);
                let (sql, findings) = (sql.clone(), findings.clone());
                self.assistant_panel
                    .update(cx, |p, cx| p.optimize(sql, findings, cx));
                cx.notify();
            }
        }
        cx.notify();
    }

    fn export_to_file(
        &mut self,
        tab: Entity<SqlTab>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = tab
            .read(cx)
            .export_all(crate::sql_tab::ExportFormat::Csv, cx)
        else {
            return self.toast("Nothing to export", cx);
        };
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let name = format!("result.{}", crate::sql_tab::ExportFormat::Csv.extension());
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                core.send(Command::WriteFile {
                    path,
                    contents: text,
                });
            } else {
                let _ = this.update(cx, |w, cx| w.toast("Export cancelled", cx));
            }
        })
        .detach();
    }

    /// Show the plan stored with a history entry in the active SQL tab, or in a new tab
    /// holding its statement.
    pub(crate) fn open_saved_plan(
        &mut self,
        history_id: i64,
        sql: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = match self.active_sql() {
            Some(t) => t,
            None => {
                self.new_query_tab(window, cx);
                let Some(t) = self.active_sql() else { return };
                t.update(cx, |t, cx| t.insert_text(sql, window, cx));
                t
            }
        };
        tab.update(cx, |t, cx| t.open_saved_plan(history_id, cx));
    }

    /// The SQL tab whose plan view matches `f` (a pending capture, load or search).
    fn plan_tab(
        &self,
        cx: &App,
        f: impl Fn(&crate::plan_view::PlanView) -> bool,
    ) -> Option<Entity<SqlTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Sql(tab) if f(tab.read(cx).plan_view().read(cx)) => Some(tab.clone()),
            _ => None,
        })
    }

    pub(crate) fn active_sql(&self) -> Option<Entity<SqlTab>> {
        match self.tabs.get(self.active) {
            Some(Tab::Sql(t)) => Some(t.clone()),
            _ => None,
        }
    }

    pub(crate) fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            // A tab already showing in the other pane: focus that pane instead.
            if let Some(sp) = &mut self.split
                && sp.other == ix
            {
                sp.other = self.active;
                sp.second_focused = !sp.second_focused;
            }
            self.active = ix;
            self.save_layout(cx);
            self.sync_schema(cx);
            cx.notify();
        }
    }

    /// Show a second tab next to (or under) the active one.
    pub(crate) fn split_view(
        &mut self,
        dir: crate::split::SplitDir,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(sp) = &mut self.split {
            sp.dir = dir;
            cx.notify();
            return;
        }
        let showable = |i: usize| !matches!(self.tabs.get(i), Some(Tab::Welcome) | None);
        match crate::split::partner(self.active, self.tabs.len(), showable) {
            Some(other) => {
                // The active tab moves to the new (second) pane, its neighbour fills the first.
                self.split = Some(crate::split::Split::new(dir, other, true));
            }
            None => {
                // Only one tab: the new pane gets a fresh SQL tab.
                let first = self.active;
                self.new_query_tab(window, cx);
                if self.active != first {
                    self.split = Some(crate::split::Split::new(dir, first, true));
                }
            }
        }
        self.fix_split();
        cx.notify();
    }

    /// Keep the split's other pane on a real tab that isn't the active one.
    fn fix_split(&mut self) {
        let Some(other) = self.split.as_ref().map(|s| s.other) else {
            return;
        };
        let tabs = &self.tabs;
        let showable = |i: usize| !matches!(tabs.get(i), Some(Tab::Welcome) | None);
        match crate::split::fix_other(other, self.active, tabs.len(), showable) {
            Some(o) => {
                if let Some(sp) = &mut self.split {
                    sp.other = o;
                }
            }
            None => self.split = None,
        }
    }

    /// The view of tab `ix`.
    fn tab_view(&self, ix: usize, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        match self.tabs.get(ix) {
            Some(Tab::Sql(t)) => t.clone().into_any_element(),
            Some(Tab::Terminal(t)) => t.clone().into_any_element(),
            Some(Tab::Files(f)) => f.clone().into_any_element(),
            Some(Tab::Editor(e)) => e.clone().into_any_element(),
            Some(Tab::Workload(w)) => w.clone().into_any_element(),
            Some(Tab::Ddl(d)) => d.clone().into_any_element(),
            Some(Tab::Object(o)) => o.clone().into_any_element(),
            Some(Tab::Activity(a)) => a.clone().into_any_element(),
            Some(Tab::Er(e)) => e.clone().into_any_element(),
            Some(Tab::Redis(r)) => r.clone().into_any_element(),
            _ => self.render_welcome(p, cx),
        }
    }

    /// The center area: one tab, or two in a split with a draggable divider.
    fn render_center(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use crate::split::SplitDir;
        self.fix_split();
        let Some(sp) = &self.split else {
            return self.tab_view(self.active, p, cx);
        };
        let (dir, ratio, second_focused, bounds) =
            (sp.dir, sp.ratio, sp.second_focused, sp.bounds.clone());
        let (first_ix, second_ix) = if second_focused {
            (sp.other, self.active)
        } else {
            (self.active, sp.other)
        };
        let pane = |this: &Self, ix: usize, second: bool, cx: &mut Context<Self>| {
            let focused = second == second_focused;
            div()
                .id(("split-pane", second as usize))
                .relative()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .overflow_hidden()
                .map(|d| match dir {
                    SplitDir::Right => d.h_full(),
                    SplitDir::Down => d.w_full(),
                })
                .map(|d| {
                    let share = if second { 1. - ratio } else { ratio };
                    match dir {
                        SplitDir::Right => d.w(gpui_kit::relative(share)),
                        SplitDir::Down => d.h(gpui_kit::relative(share)),
                    }
                })
                // Focus follows the mouse button, before the tab's own handlers run.
                .capture_any_mouse_down(cx.listener(move |this, _, _, cx| {
                    if let Some(sp) = &mut this.split
                        && sp.second_focused != second
                    {
                        let ix = sp.other;
                        this.activate(ix, cx);
                    }
                }))
                .child(this.tab_view(ix, p, cx))
                .child(
                    // Marks the focused pane without shifting its content.
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(rpx(2.))
                        .when(focused, |d| d.bg(p.acc)),
                )
        };
        let first = pane(self, first_ix, false, cx);
        let second = pane(self, second_ix, true, cx);
        let divider = div()
            .id("split-divider")
            .flex_none()
            .bg(p.bd)
            .hover(|s| s.bg(p.acc))
            .map(|d| match dir {
                SplitDir::Right => d.w(rpx(4.)).h_full().cursor_col_resize(),
                SplitDir::Down => d.h(rpx(4.)).w_full().cursor_row_resize(),
            })
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(sp) = &mut this.split {
                        sp.dragging = true;
                    }
                    cx.stop_propagation();
                }),
            );
        div()
            .id("split")
            .relative()
            .size_full()
            .flex()
            .map(|d| match dir {
                SplitDir::Right => d.flex_row(),
                SplitDir::Down => d.flex_col(),
            })
            .on_mouse_move(cx.listener(|this, ev: &gpui_kit::MouseMoveEvent, _, cx| {
                if let Some(sp) = &mut this.split
                    && sp.dragging
                {
                    if ev.pressed_button == Some(gpui_kit::MouseButton::Left) {
                        sp.drag_to(ev.position);
                        cx.notify();
                    } else {
                        sp.dragging = false;
                    }
                }
            }))
            .on_mouse_up(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    if let Some(sp) = &mut this.split {
                        sp.dragging = false;
                    }
                }),
            )
            .child(
                gpui_kit::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(first)
            .child(divider)
            .child(second)
            .into_any_element()
    }

    fn save_layout(&self, cx: &App) {
        let active_buffer = match self.tabs.get(self.active) {
            Some(Tab::Sql(t)) => Some(t.read(cx).buffer_id().to_owned()),
            _ => None,
        };
        self.core.send(Command::SaveWorkspace(SavedWorkspace {
            name: self.workspace_name.clone(),
            buffers: Vec::new(),
            pinned: Vec::new(),
            sidebar_collapsed: !self.sidebar_open,
            active_buffer,
        }));
    }

    /// Open (or focus) a SQL tab for a connection.
    pub(crate) fn open_connection(
        &mut self,
        id: &ProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.profiles.db(id).cloned() else {
            return;
        };
        if !conn.engine.is_sql() {
            return self.open_redis(conn, window, cx);
        }
        // An active tab without a connection adopts it.
        if let Some(t) = self.active_sql()
            && t.read(cx).connection.is_none()
        {
            t.update(cx, |t, cx| t.set_connection(Some(conn), cx));
            self.sync_schema(cx);
            return;
        }
        if let Some(ix) = self.tabs.iter().position(|t| match t {
            Tab::Sql(s) => s.read(cx).connection.as_ref().is_some_and(|c| &c.id == id),
            _ => false,
        }) {
            return self.activate(ix, cx);
        }
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, Tab::Sql(_)))
            .count();
        let buffer = BufferState {
            id: format!("b{}", next_id()),
            title: format!("{}.sql", conn.name),
            connection_id: Some(id.clone()),
            text: String::new(),
            cursor: 0,
        };
        self.new_sql_tab(buffer, Some(conn), n as i64, window, cx);
        self.active = self.tabs.len() - 1;
        self.save_layout(cx);
        self.sync_schema(cx);
    }

    /// A new SQL tab on `conn`, titled `title`, made active.
    pub(crate) fn open_query_tab(
        &mut self,
        conn: &DbConnection,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SqlTab> {
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, Tab::Sql(_)))
            .count();
        let buffer = BufferState {
            id: format!("b{}", next_id()),
            title: format!("{title}.sql"),
            connection_id: Some(conn.id.clone()),
            text: String::new(),
            cursor: 0,
        };
        let tab = self.new_sql_tab(buffer, Some(conn.clone()), n as i64, window, cx);
        self.active = self.tabs.len() - 1;
        self.save_layout(cx);
        self.sync_schema(cx);
        tab
    }

    fn new_query_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self
            .active_sql()
            .and_then(|t| t.read(cx).connection.clone())
            .or_else(|| self.profiles.dbs().find(|d| d.engine.is_sql()).cloned());
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, Tab::Sql(_)))
            .count();
        let buffer = BufferState {
            id: format!("b{}", next_id()),
            title: format!("query-{}.sql", n + 1),
            connection_id: conn.as_ref().map(|c| c.id.clone()),
            text: String::new(),
            cursor: 0,
        };
        let tab = self.new_sql_tab(buffer, conn, n as i64, window, cx);
        tab.update(cx, |t, cx| t.save_now(cx));
        self.active = self.tabs.len() - 1;
        self.save_layout(cx);
        self.sync_schema(cx);
        let ed = tab.read(cx).editor().clone();
        ed.update(cx, |e, cx| e.focus(window, cx));
    }

    /// Close a group of tabs from the tab menu: `close`, `close_others`, `close_right`,
    /// `close_left` or `close_all`, relative to tab `ix`. Each goes through
    /// [`Self::close_tab`], so open transactions and unsaved files still hold their tab.
    pub(crate) fn close_tabs(
        &mut self,
        action: &str,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = self.tabs.len();
        if ix >= n {
            return;
        }
        let doomed: Vec<usize> = match action {
            "close" => vec![ix],
            "close_others" => (0..n).filter(|&i| i != ix).collect(),
            "close_right" => (ix + 1..n).collect(),
            "close_left" => (0..ix).collect(),
            "close_all" => (0..n).collect(),
            _ => return,
        };
        // Highest first, so the remaining indices stay valid. Files with unsaved changes
        // wait for one Save / Discard / Cancel question.
        let mut dirty = Vec::new();
        for i in doomed.into_iter().rev() {
            match self.tabs.get(i) {
                Some(Tab::Editor(e)) if e.read(cx).dirty => dirty.push(e.clone()),
                _ => self.close_tab_now(i, cx),
            }
        }
        if !dirty.is_empty() {
            dirty.reverse();
            self.confirm_unsaved(dirty, crate::unsaved::AfterUnsaved::CloseTabs, window, cx);
        }
        cx.notify();
    }

    /// After a connection is saved (a fixed password, host, …), sessions on it that had
    /// failed reconnect with the new settings.
    fn reconnect_failed(&mut self, id: &ProfileId, cx: &mut Context<Self>) {
        let Some(conn) = self.profiles.db(id).cloned() else {
            return;
        };
        let core = self.core.clone();
        if let Some(s) = self.explorer.state_mut(id)
            && matches!(s.state.0, Some(SessionState::Failed(_)))
        {
            s.connection = Some(conn.clone());
            s.reconnect(&core);
        }
        for t in &self.tabs {
            if let Tab::Sql(tab) = t {
                let failed = {
                    let t = tab.read(cx);
                    t.connection.as_ref().is_some_and(|c| &c.id == id)
                        && matches!(t.session_state, SessionState::Failed(_))
                };
                if failed {
                    let conn = conn.clone();
                    tab.update(cx, |t, cx| t.set_connection(Some(conn), cx));
                }
            }
        }
    }

    /// One color per open database: tabs on the same connection share it, and each other
    /// connection takes the next color, in tab order.
    fn connection_colors(&self, cx: &App) -> Vec<(ProfileId, Hsla)> {
        let mut out: Vec<(ProfileId, Hsla)> = Vec::new();
        for t in &self.tabs {
            if let Tab::Sql(s) = t
                && let Some(c) = &s.read(cx).connection
                && !out.iter().any(|(id, _)| id == &c.id)
            {
                let hue = DB_HUES[out.len() % DB_HUES.len()];
                out.push((c.id.clone(), gpui_kit::hsla(hue / 360.0, 0.62, 0.56, 1.0)));
            }
        }
        out
    }

    /// Close tab `ix`; a file with unsaved changes asks Save / Discard / Cancel first.
    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Tab::Editor(e)) = self.tabs.get(ix)
            && e.read(cx).dirty
        {
            let e = e.clone();
            self.confirm_unsaved(vec![e], crate::unsaved::AfterUnsaved::CloseTabs, window, cx);
            return;
        }
        self.close_tab_now(ix, cx);
    }

    /// Close tab `ix` without asking about unsaved files (an open transaction still
    /// keeps the tab).
    pub(crate) fn close_tab_now(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if let Tab::Sql(t) = &self.tabs[ix] {
            let t = t.read(cx);
            if t.txn_open {
                self.toast(
                    "This tab has an open transaction · commit or roll back first",
                    cx,
                );
                return;
            }
            self.core.send(Command::DeleteBuffer {
                id: t.buffer_id().to_owned(),
            });
            if let Some(s) = t.session {
                self.core.send(Command::CloseSession { session: s });
            }
        }
        if let Tab::Terminal(t) = &self.tabs[ix] {
            t.update(cx, |t, _| t.shutdown());
        }
        if let Tab::Object(o) = &self.tabs[ix] {
            o.update(cx, |o, _| o.shutdown());
        }
        if let Tab::Activity(a) = &self.tabs[ix] {
            a.update(cx, |a, _| a.shutdown());
        }
        if let Tab::Er(e) = &self.tabs[ix] {
            e.update(cx, |e, _| e.shutdown());
        }
        if let Tab::Redis(r) = &self.tabs[ix] {
            r.update(cx, |r, _| r.shutdown());
        }
        if let Tab::Editor(e) = &self.tabs[ix] {
            e.update(cx, |e, cx| e.shutdown(cx));
        }
        self.tabs.remove(ix);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::Welcome);
        }
        if let Some(sp) = &mut self.split {
            let len = self.tabs.len();
            if ix == sp.other {
                // The other pane's tab closed: it shows another tab (or the split ends).
                sp.other = usize::MAX;
                if ix < self.active {
                    self.active -= 1;
                }
            } else {
                sp.tab_removed(ix);
                if ix < self.active {
                    self.active -= 1;
                } else if ix == self.active {
                    // The focused pane shows a neighbour that the other pane isn't showing.
                    let near = ix.min(len - 1);
                    self.active = (near..len)
                        .chain((0..near).rev())
                        .find(|&i| i != sp.other)
                        .unwrap_or(sp.other);
                    if self.active == sp.other {
                        self.split = None;
                    }
                }
            }
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.fix_split();
        self.save_layout(cx);
        self.sync_schema(cx);
        cx.notify();
    }

    pub(crate) fn open_terminal(&mut self, host: Option<ProfileId>, cx: &mut Context<Self>) {
        let name = host
            .as_ref()
            .and_then(|h| self.profiles.host(h))
            .map(|h| h.name.clone())
            .unwrap_or_else(|| "Local shell".into());
        let env = host
            .as_ref()
            .and_then(|h| self.profiles.host(h))
            .map(|h| h.environment)
            .unwrap_or_default();
        let colors = host
            .as_ref()
            .and_then(|h| self.profiles.host(h))
            .and_then(|h| h.terminal_colors.clone());
        let core = self.core.clone();
        let t =
            cx.new(|cx| TerminalTab::new(core, name, env, host, cx).with_colors(colors.as_ref()));
        self.tabs.push(Tab::Terminal(t));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    /// The Host of the active SSH terminal or remote file: the sidebar then shows its files.
    pub(crate) fn active_ssh_host(&self, cx: &App) -> Option<ProfileId> {
        match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) => t.read(cx).host().cloned(),
            Some(Tab::Editor(e)) => match &e.read(cx).fs {
                FsRef::Host(h) => Some(h.clone()),
                FsRef::Local | FsRef::Ftp(_) | FsRef::S3(_) => None,
            },
            Some(Tab::Files(f)) => f.read(cx).right_host().cloned(),
            _ => None,
        }
    }

    /// The Files panel for `host`, created on first use.
    pub(crate) fn remote_files_for(
        &mut self,
        host: &ProfileId,
        cx: &mut Context<Self>,
    ) -> Entity<RemoteFiles> {
        if let Some(e) = self.remote_files.get(&host.0) {
            return e.clone();
        }
        let core = self.core.clone();
        let h = host.clone();
        let transfers = self.transfers.clone();
        let panel = cx.new(|cx| RemoteFiles::new(core, h, transfers, cx));
        let sub = cx.subscribe(&panel, |this, _, ev: &RemoteFilesEvent, cx| match ev {
            RemoteFilesEvent::Open { host, path } => {
                this.pending_editor = Some((FsRef::Host(host.clone()), path.clone()));
                cx.notify();
            }
            RemoteFilesEvent::OpenTab(host) => {
                this.pending_files = Some(Some(host.clone()));
                cx.notify();
            }
            RemoteFilesEvent::Toast(t) => this.toast(t.clone(), cx),
        });
        self._subs.push(sub);
        self.remote_files.insert(host.0.clone(), panel.clone());
        panel
    }

    /// Open a file from a Host in an editor tab (or focus the one already open).
    pub(crate) fn open_remote_file(
        &mut self,
        fs: FsRef,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Editor(e) if e.read(cx).shows(&fs, &path)))
        {
            return self.activate(ix, cx);
        }
        let (name, env) = match &fs {
            FsRef::Host(h) => self
                .profiles
                .host(h)
                .map(|h| (h.name.clone(), h.environment))
                .unwrap_or_default(),
            FsRef::Ftp(id) | FsRef::S3(id) => self
                .profiles
                .all
                .iter()
                .find(|p| p.id() == id)
                .map(|p| (p.name().to_owned(), p.environment()))
                .unwrap_or_default(),
            FsRef::Local => ("this computer".into(), Default::default()),
        };
        let core = self.core.clone();
        let tab = cx.new(|cx| EditorTab::new(core, fs, path, &name, env, window, cx));
        self.tabs.push(Tab::Editor(tab));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    pub(crate) fn open_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let host = self.active_ssh_host(cx);
        self.open_files_for(host, cx);
    }

    /// What the Files tab's right pane can show: Hosts (SFTP) and FTP connections.
    fn file_sources(&self) -> Vec<(FsRef, String)> {
        let hosts = self
            .profiles
            .hosts()
            .map(|h| (FsRef::Host(h.id.clone()), h.name.clone()));
        let remote_files = self.profiles.all.iter().filter_map(|p| match p {
            Profile::File(f) => match &f.protocol {
                switchyard_core::store::FileProtocol::Ftp { .. } => {
                    Some((FsRef::Ftp(f.id.clone()), f.name.clone()))
                }
                switchyard_core::store::FileProtocol::S3 { .. } => {
                    Some((FsRef::S3(f.id.clone()), f.name.clone()))
                }
                _ => None,
            },
            _ => None,
        });
        hosts.chain(remote_files).collect()
    }

    /// The Files tab, with `host` on the right (or what it already shows).
    pub(crate) fn open_files_for(&mut self, host: Option<ProfileId>, cx: &mut Context<Self>) {
        self.open_files_on(host.map(FsRef::Host), cx);
    }

    /// The Files tab, with `right` (a Host or an FTP connection) on the right, or what it
    /// already shows.
    pub(crate) fn open_files_on(&mut self, right: Option<FsRef>, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, Tab::Files(_))) {
            if let (Some(fs), Tab::Files(f)) = (right, &self.tabs[ix]) {
                f.update(cx, |f, cx| {
                    if *f.right_source() != fs {
                        f.show(fs, cx)
                    }
                });
            }
            return self.activate(ix, cx);
        }
        let core = self.core.clone();
        let transfers = self.transfers.clone();
        let sources = self.file_sources();
        let right = right.unwrap_or(FsRef::Local);
        let f = cx.new(|cx| FilesTab::new(core, transfers, sources, right, cx));
        let sub = cx.subscribe(&f, |this, _, ev: &FilesTabEvent, cx| match ev {
            FilesTabEvent::Open { fs, path } => {
                this.pending_editor = Some((fs.clone(), path.clone()));
                cx.notify();
            }
        });
        self._subs.push(sub);
        self.tabs.push(Tab::Files(f));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    fn show_welcome(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, Tab::Welcome)) {
            return self.activate(ix, cx);
        }
        self.tabs.push(Tab::Welcome);
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    // --------------------------------------------------------------- toasts

    pub(crate) fn toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some(text.into());
        self.toast_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(2600))
                .await;
            let _ = this.update(cx, |w, cx| {
                w.toast = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------- overlays

    pub(crate) fn open_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profiles = self.profiles.clone();
        let view = cx.new(|cx| PaletteView::new(mode, profiles, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &PaletteEvent, window, cx| {
            this.overlay = None;
            match ev {
                PaletteEvent::Run(id) => this.run_command(*id, window, cx),
                PaletteEvent::Open(id) => this.open_profile(id, window, cx),
                PaletteEvent::Dismiss => {}
            }
            cx.notify();
        });
        self._subs.push(sub);
        self.overlay = Some(Overlay::Palette(view));
        cx.notify();
    }

    pub(crate) fn open_profile(
        &mut self,
        id: &ProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // From the quick switcher: a SQL tab without a connection picks it up.
        match self.profiles.all.iter().find(|p| p.id() == id).cloned() {
            Some(Profile::Db(d)) => {
                if !d.engine.is_sql() {
                    self.open_redis(d, window, cx);
                } else if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.set_connection(Some(d.clone()), cx));
                    self.sync_schema(cx);
                } else {
                    self.open_connection(id, window, cx);
                }
            }
            Some(Profile::Host(h)) => self.open_terminal(Some(h.id), cx),
            Some(Profile::File(f)) => match &f.protocol {
                switchyard_core::store::FileProtocol::Sftp { host_id } => {
                    self.open_files_for(Some(host_id.clone()), cx)
                }
                switchyard_core::store::FileProtocol::Ftp { .. } => {
                    self.open_files_on(Some(FsRef::Ftp(f.id.clone())), cx)
                }
                switchyard_core::store::FileProtocol::S3 { .. } => {
                    self.open_files_on(Some(FsRef::S3(f.id.clone())), cx)
                }
            },
            Some(Profile::Terminal(t)) => self.open_terminal(t.host_id, cx),
            None => {}
        }
    }

    pub(crate) fn open_conn_editor(
        &mut self,
        kind: crate::conn_editor::ConnKind,
        existing: Option<Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let profiles = self.profiles.clone();
        let comps = self.components.clone();
        let ed = cx.new(|cx| ConnEditor::new(core, profiles, kind, existing, comps, window, cx));
        let sub = cx.subscribe_in(
            &ed,
            window,
            |this, _, ev: &ConnEditorEvent, _window, cx| match ev {
                ConnEditorEvent::Close => {
                    this.overlay = None;
                    cx.notify();
                }
                ConnEditorEvent::Toast(t) => this.toast(t.clone(), cx),
            },
        );
        self._subs.push(sub);
        self.overlay = Some(Overlay::ConnEditor(ed));
        cx.notify();
    }

    /// Point the assistant panel at the active SQL tab's connection, and at the Workbench's
    /// collection in the API workspace.
    pub(crate) fn sync_assistant(&mut self, cx: &mut Context<Self>) {
        let conn = self.assistant_target(cx);
        let api_context = match (self.mode, &self.api) {
            (AppMode::Api, Some(api)) => Some(api.read(cx).ai_context()),
            _ => None,
        };
        self.assistant_panel.update(cx, |p, cx| {
            p.set_connection(conn, cx);
            if let Some(context) = api_context {
                p.set_api_context(context);
            }
        });
    }

    /// What the assistant is asked about: the active tab's database or SSH Host.
    fn assistant_target(&self, cx: &App) -> Option<crate::assistant_panel::AssistantTarget> {
        use crate::assistant_panel::AssistantTarget as T;
        match self.tabs.get(self.active)? {
            Tab::Sql(t) => t.read(cx).connection.clone().map(T::Db),
            Tab::Redis(t) => Some(T::Db(t.read(cx).connection.clone())),
            Tab::Object(t) => Some(T::Db(t.read(cx).connection.clone())),
            Tab::Activity(t) => Some(T::Db(t.read(cx).connection.clone())),
            Tab::Er(t) => Some(T::Db(t.read(cx).connection.clone())),
            Tab::Terminal(t) => {
                let id = t.read(cx).host_id()?;
                self.profiles.host(id).cloned().map(T::Host)
            }
            _ => None,
        }
    }

    /// The Workbench's AI buttons: questions go to the assistant panel beside it.
    fn subscribe_workbench(
        &mut self,
        api: &Entity<crate::api::workbench::WorkbenchPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::api::workbench::{AskAgentRequested, DescribeRequestRequested};
        let ask = cx.subscribe_in(api, window, |this, _, ev: &AskAgentRequested, _, cx| {
            this.assistant_open = true;
            this.sync_assistant(cx);
            let (label, prompt) = (ev.label.clone(), ev.prompt.clone());
            this.assistant_panel
                .update(cx, |p, cx| p.ask_api(label, prompt, cx));
            cx.notify();
        });
        let describe = cx.subscribe_in(
            api,
            window,
            |this, _, _: &DescribeRequestRequested, window, cx| {
                this.assistant_open = true;
                this.sync_assistant(cx);
                this.assistant_panel
                    .update(cx, |p, cx| p.describe_request(window, cx));
                cx.notify();
            },
        );
        self._subs.push(ask);
        self._subs.push(describe);
    }

    /// The assistant panel asks for something.
    fn on_assistant_event(
        &mut self,
        ev: &crate::assistant_panel::AssistantPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::assistant_panel::AssistantPanelEvent as E;
        use crate::plan_view::AfterPlan;
        let compare =
            |this: &mut Self, base: Option<String>, then: AfterPlan, cx: &mut Context<Self>| {
                match this.active_sql() {
                    Some(tab) => tab.update(cx, |t, cx| t.assistant_compare(base, then, cx)),
                    None => this.toast("Open the statement's SQL tab to compare plans", cx),
                }
            };
        match ev {
            E::OpenInEditor(sql) => {
                let conn = self
                    .active_sql()
                    .and_then(|t| t.read(cx).connection.clone());
                match conn {
                    Some(conn) => {
                        let tab = self.open_query_tab(&conn, "suggestion", window, cx);
                        let sql = sql.clone();
                        tab.update(cx, |t, cx| t.insert_text(&sql, window, cx));
                    }
                    None => self.toast("Pick a connection first", cx),
                }
            }
            E::ComparePlan { base, sql } => {
                compare(self, base.clone(), AfterPlan::Compare(sql.clone()), cx)
            }
            E::WhatIf { base, indexes } => {
                compare(self, base.clone(), AfterPlan::WhatIf(indexes.clone()), cx)
            }
            E::Replan { base } => compare(self, base.clone(), AfterPlan::Replan, cx),
            E::OpenTerminal(agent) => {
                let target = self.assistant_target(cx);
                let db = target.as_ref().and_then(|t| t.db());
                let kind = agent.unwrap_or_else(|| self.assistant.agent_for(db));
                let core = self.core.clone();
                let (agent, conn_id) = (*agent, target.map(|t| t.id().clone()));
                let t = cx.new(|cx| {
                    TerminalTab::new_agent(core, kind.display_name().to_owned(), agent, conn_id, cx)
                });
                self.tabs.push(Tab::Terminal(t));
                self.active = self.tabs.len() - 1;
            }
            E::OpenRequest(request) => match &self.api {
                Some(api) => {
                    api.update(cx, |w, cx| w.open_generated_request(request, window, cx));
                    // The request lands in the Workbench's collection: refresh what the next
                    // description sees.
                    self.sync_assistant(cx);
                }
                None => self.toast("Open the API workspace first", cx),
            },
            E::Close => self.assistant_open = false,
        }
        cx.notify();
    }

    pub(crate) fn open_settings(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core.send(Command::DetectComponents);
        self.overlay = Some(Overlay::Settings(page));
        window.focus(&self.overlay_focus, cx);
        cx.notify();
    }

    /// Query and index statistics for the active SQL tab's connection.
    pub(crate) fn open_workload(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_sql() else {
            return self.toast("Open a query tab on a database first", cx);
        };
        let (session, name) = {
            let t = tab.read(cx);
            (t.session, t.connection.as_ref().map(|c| c.name.clone()))
        };
        let (Some(session), Some(name)) = (session, name) else {
            return self.toast("Connect the query tab to a database first", cx);
        };
        let core = self.core.clone();
        let w = cx.new(|_| crate::workload_tab::WorkloadTab::new(core, session, name));
        self.tabs.push(Tab::Workload(w));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    pub(crate) fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search history"));
        let core = self.core.clone();
        let sub = cx.subscribe(
            &input,
            move |_, input, ev: &gpui_kit::component::input::InputEvent, cx| {
                if matches!(ev, gpui_kit::component::input::InputEvent::Change) {
                    let q = input.read(cx).value().to_string();
                    core.send(Command::SearchHistory {
                        request: next_id(),
                        query: q,
                        connection: None,
                    });
                }
            },
        );
        self._subs.push(sub);
        input.update(cx, |i, cx| i.focus(window, cx));
        self.core.send(Command::SearchHistory {
            request: next_id(),
            query: String::new(),
            connection: None,
        });
        self.overlay = Some(Overlay::History(input));
        cx.notify();
    }

    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cancel_prompt(window, cx) {
            return;
        }
        self.overlay = None;
        self.tunnels_open = false;
        self.ctx = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Finish resizing the right panel and remember its width.
    fn end_inspector_drag(&mut self) {
        if self.inspector_drag.take().is_some() {
            self.save_inspector_width();
        }
    }

    /// Finish resizing the left sidebar and remember its width.
    fn end_sidebar_drag(&mut self) {
        if self.sidebar_drag.take().is_some() {
            self.save_sidebar_width();
        }
    }

    pub(crate) fn save_sidebar_width(&self) {
        self.core.send(Command::SetSetting {
            key: "sidebar.width".into(),
            value: (self.sidebar_width.round() as i64).into(),
        });
    }

    pub(crate) fn save_inspector_width(&self) {
        self.core.send(Command::SetSetting {
            key: "inspector.width".into(),
            value: (self.inspector_width.round() as i64).into(),
        });
    }

    pub(crate) fn set_theme(&mut self, id: ThemeId, window: &mut Window, cx: &mut Context<Self>) {
        theme::apply(id.palette(), Some(window), cx);
        self.core.send(Command::SetSetting {
            key: "theme".into(),
            value: id.key().into(),
        });
        cx.notify();
    }

    /// Apply and save new text-size settings (zoom, editor font).
    pub(crate) fn set_appearance(
        &mut self,
        s: crate::appearance::AppearanceSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::appearance::set(s, cx);
        theme::apply_fonts(Some(window), cx);
        let saved = crate::appearance::current(cx);
        match serde_json::to_value(&saved) {
            Ok(value) => self.core.send(Command::SetSetting {
                key: crate::appearance::APPEARANCE_KEY.into(),
                value,
            }),
            Err(e) => tracing::warn!(error = %e, "could not save the appearance settings"),
        }
        cx.notify();
    }

    /// Change the zoom level with `f` (zoom in, out, or reset) and say where it landed.
    pub(crate) fn zoom_by(
        &mut self,
        f: impl FnOnce(f32) -> f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut s = crate::appearance::current(cx);
        s.zoom = crate::appearance::clamp_zoom(f(s.zoom));
        let pct = crate::appearance::percent(s.zoom);
        self.set_appearance(s, window, cx);
        self.toast(format!("Zoom {pct} %"), cx);
    }

    /// Execute a palette command.
    pub(crate) fn run_command(
        &mut self,
        id: CommandId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::conn_editor::ConnKind;
        match id {
            CommandId::NewConnection => self.open_conn_editor(
                ConnKind::Db(switchyard_core::db::Engine::Postgres),
                None,
                window,
                cx,
            ),
            CommandId::NewHost => self.open_conn_editor(ConnKind::Ssh, None, window, cx),
            CommandId::NewTerminal => self.open_terminal(None, cx),
            CommandId::NewQueryTab => self.new_query_tab(window, cx),
            CommandId::RunStatement => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.run_statement(window, cx));
                }
            }
            CommandId::RunScript => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.run_script(window, cx));
                }
            }
            CommandId::StopQuery => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.stop(cx));
                }
            }
            CommandId::Explain | CommandId::ExplainAnalyze => {
                let analyze = id == CommandId::ExplainAnalyze;
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.explain(analyze, window, cx));
                }
            }
            CommandId::FormatSql => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.format(window, cx));
                }
            }
            CommandId::CommitTransaction => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.end_transaction(true, cx));
                }
            }
            CommandId::RollbackTransaction => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.end_transaction(false, cx));
                }
            }
            CommandId::OpenFiles => self.open_files(window, cx),
            CommandId::Settings => self.open_settings(SettingsPage::General, window, cx),
            CommandId::SettingsDrivers => self.open_settings(SettingsPage::Drivers, window, cx),
            CommandId::ManageSnippets => crate::snippets::open_manager(self.core.clone(), cx),
            CommandId::ToggleTheme => {
                let next = if palette(cx).dark {
                    ThemeId::SwitchyardLight
                } else {
                    ThemeId::SwitchyardDark
                };
                self.set_theme(next, window, cx);
            }
            CommandId::ToggleInspector => self.inspector_open = !self.inspector_open,
            CommandId::ToggleAssistant => {
                self.assistant_open = !self.assistant_open;
                if self.assistant_open {
                    self.sync_assistant(cx);
                }
            }
            CommandId::OptimizeQuery => {
                if let Some(tab) = self.active_sql() {
                    tab.update(cx, |t, cx| t.optimize(cx));
                }
            }
            CommandId::PlanQuery => {
                self.assistant_open = true;
                self.sync_assistant(cx);
                self.assistant_panel
                    .update(cx, |p, cx| p.plan_query(window, cx));
            }
            CommandId::SettingsAssistant => self.open_settings(SettingsPage::Assistant, window, cx),
            CommandId::ToggleSidebar => {
                self.sidebar_open = !self.sidebar_open;
                self.save_layout(cx);
            }
            CommandId::ShowWelcome => self.show_welcome(cx),
            CommandId::OpenComponents => {
                self.overlay = Some(Overlay::Components);
                window.focus(&self.overlay_focus, cx);
            }
            CommandId::RefreshSchema => {
                self.side_tab = SideTab::Schema;
                self.refresh_explorer(cx);
            }
            CommandId::ImportSshConfig => self.import_ssh_config(cx),
            CommandId::ExportProfiles => {
                let dir = std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default();
                let rx = cx.prompt_for_new_path(&dir, Some("switchyard-profiles.json"));
                let core = self.core.clone();
                cx.spawn(async move |_, _| {
                    if let Ok(Ok(Some(path))) = rx.await {
                        core.send(Command::ExportProfiles { path });
                    }
                })
                .detach();
            }
            CommandId::ShowHistory => self.open_history(window, cx),
            CommandId::ShowWorkload => self.open_workload(cx),
            CommandId::ShowActivity => self.open_activity_palette(window, cx),
            CommandId::ErDiagram => self.open_er_for_current_schema(window, cx),
            CommandId::SplitRight => self.split_view(crate::split::SplitDir::Right, window, cx),
            CommandId::SplitDown => self.split_view(crate::split::SplitDir::Down, window, cx),
            CommandId::Unsplit => self.split = None,
            CommandId::SwitchToDefault => self.set_mode(AppMode::Default, window, cx),
            CommandId::SwitchToApi => self.set_mode(AppMode::Api, window, cx),
            CommandId::ZoomIn => self.zoom_by(crate::appearance::zoom_in, window, cx),
            CommandId::ZoomOut => self.zoom_by(crate::appearance::zoom_out, window, cx),
            CommandId::ResetZoom => self.zoom_by(|_| 1.0, window, cx),
            CommandId::CheckForUpdates => self.check_for_updates(cx),
            CommandId::OpenLogFolder => self.open_log_folder(cx),
        }
        cx.notify();
    }

    /// Read `~/.ssh/config`; the preview opens when the runtime answers.
    fn import_ssh_config(&mut self, cx: &mut Context<Self>) {
        self.core.send(Command::PreviewSshConfig);
        self.toast("Reading ~/.ssh/config…", cx);
    }

    // --------------------------------------------------------------- render

    /// Show the Default or API workspace.
    pub(crate) fn set_mode(&mut self, mode: AppMode, window: &mut Window, cx: &mut Context<Self>) {
        self.mode_menu = false;
        if mode == AppMode::Api && self.api.is_none() {
            let api = cx.new(|cx| crate::api::workbench::WorkbenchPanel::new(window, cx));
            self.subscribe_workbench(&api, window, cx);
            self.api = Some(api);
        }
        self.mode = mode;
        self.assistant_panel
            .update(cx, |p, cx| p.set_api(mode == AppMode::Api, window, cx));
        self.sync_assistant(cx);
        match (mode, &self.api) {
            (AppMode::Api, Some(api)) => {
                let focus = api.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        cx.notify();
    }

    /// The title bar's workspace menu: Default or API.
    fn render_mode_menu(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str, mode: AppMode, title: &'static str, sub: &'static str| {
            let active = self.mode == mode;
            div()
                .id(id)
                .flex()
                .flex_col()
                .px(rpx(10.))
                .py(rpx(6.))
                .rounded(px(5.))
                .cursor_pointer()
                .when(active, |d| d.bg(p.sel))
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(move |this, _, w, cx| this.set_mode(mode, w, cx)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .text_size(ts::UI)
                        .font_weight(FontWeight::MEDIUM)
                        .child(title)
                        .when(active, |d| d.child(div().text_color(p.acc).child("✓"))),
                )
                .child(div().text_size(ts::SMALL).text_color(p.fg3).child(sub))
        };
        gpui_kit::deferred(
            div()
                .id("mode-menu")
                .absolute()
                .top(rpx(34.))
                .left(rpx(8.))
                .w(rpx(280.))
                .p(rpx(4.))
                .bg(p.elev)
                .border_1()
                .border_color(p.bd)
                .rounded(px(7.))
                .shadow(ui::shadow(p))
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.mode_menu = false;
                    cx.notify();
                }))
                .child(item(
                    "mode-default",
                    AppMode::Default,
                    "Default",
                    "Databases, terminals, files and tunnels",
                ))
                .child(item(
                    "mode-api",
                    AppMode::Api,
                    "API",
                    "Collections, requests, environments and runs",
                ))
                .child(div().my(rpx(4.)).h(rpx(1.)).bg(p.bd))
                .child(
                    div()
                        .id("menu-assistant")
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .px(rpx(10.))
                        .py(rpx(6.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .hover(|s| s.bg(p.hover))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.mode_menu = false;
                            this.run_command(CommandId::ToggleAssistant, w, cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .text_size(ts::UI)
                                .font_weight(FontWeight::MEDIUM)
                                .child("Assistant"),
                        )
                        .when(self.assistant_open, |d| {
                            d.child(div().text_color(p.acc).child("✓"))
                        })
                        .child(ui::kbd(ui::keys("⌘J", "Ctrl+J"), p)),
                ),
        )
        .with_priority(2)
        .into_any_element()
    }

    fn render_title_bar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use gpui_kit::component::IconName;
        use gpui_kit::component::tooltip::Tooltip;
        let dark = p.dark;
        // The title bar's row is the window's drag area (`HTCAPTION` on Windows). GPUI
        // reports it under every hitbox inside it, so each control must occlude it or
        // Windows takes the click as a window drag and the control never sees it.
        //
        // The left and right clusters share the free space equally (`flex_1` from a zero
        // basis), so the search field stays centred whatever the mode shows on the left.
        let left = div()
            .flex()
            .flex_1()
            .flex_basis(rpx(0.))
            .items_center()
            .gap(rpx(6.))
            .child(
                div()
                    .id("tb-workspace")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(rpx(7.))
                    .h(rpx(26.))
                    .px(rpx(8.))
                    .rounded(px(6.))
                    .occlude()
                    .cursor_pointer()
                    .when(self.mode_menu, |d| d.bg(p.hover))
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mode_menu = !this.mode_menu;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(rpx(14.))
                            .rounded(px(3.))
                            .bg(p.fg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(rpx(6.)).rounded(px(1.)).bg(p.panel)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::UI)
                            .text_color(p.fg)
                            .whitespace_nowrap()
                            .child(match self.mode {
                                AppMode::Default => self.workspace_name.clone(),
                                AppMode::Api => "API".into(),
                            }),
                    )
                    .child(div().text_color(p.fg3).text_size(ts::CAPTION).child("▾")),
            )
            // The API workspace has no sidebar; the button is Default-only.
            .when(self.mode == AppMode::Default, |d| {
                d.child(
                    ui::icon_button("tb-sidebar", IconName::PanelLeft, self.sidebar_open, p)
                        .occlude()
                        .tooltip(|w, cx| {
                            Tooltip::new("Toggle Sidebar")
                                .action(&actions::ToggleSidebar, None)
                                .build(w, cx)
                        })
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.run_command(CommandId::ToggleSidebar, w, cx)
                        })),
                )
            });
        let search = div()
            .id("tb-search")
            .flex_shrink(1.)
            .min_w_0()
            .w(rpx(460.))
            .h(rpx(26.))
            .flex()
            .items_center()
            .gap(rpx(8.))
            .pl(rpx(10.))
            .pr(rpx(6.))
            .border_1()
            .border_color(p.bd)
            .rounded(px(6.))
            .bg(p.bg)
            .text_color(p.fg3)
            .text_size(ts::UI)
            .cursor_text()
            .occlude()
            .hover(|s| s.border_color(p.bd2))
            .on_click(cx.listener(|this, _, w, cx| this.open_palette(PaletteMode::Commands, w, cx)))
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .child("Search connections, tables, commands…"),
            )
            .child(ui::kbd(ui::keys("⇧⌘P", "Ctrl+Shift+P"), p));
        let theme_tip = if dark {
            "Switch to Light Theme"
        } else {
            "Switch to Dark Theme"
        };
        let right = div()
            .flex()
            .flex_1()
            .flex_basis(rpx(0.))
            .justify_end()
            .items_center()
            .gap(rpx(2.))
            .child(
                ui::icon_button("tb-assistant", IconName::Bot, self.assistant_open, p)
                    .occlude()
                    .tooltip(|w, cx| {
                        Tooltip::new("Assistant")
                            .action(&actions::ToggleAssistant, None)
                            .build(w, cx)
                    })
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.run_command(CommandId::ToggleAssistant, w, cx)
                    })),
            )
            .child(
                ui::icon_button(
                    "tb-theme",
                    if dark { IconName::Sun } else { IconName::Moon },
                    false,
                    p,
                )
                .occlude()
                .tooltip(move |w, cx| Tooltip::new(theme_tip).build(w, cx))
                .on_click(
                    cx.listener(|this, _, w, cx| this.run_command(CommandId::ToggleTheme, w, cx)),
                ),
            )
            // A developer gallery; release builds reach it from the command palette.
            .when(cfg!(debug_assertions), |d| {
                d.child(
                    ui::icon_button("tb-components", IconName::LayoutDashboard, false, p)
                        .occlude()
                        .tooltip(|w, cx| {
                            Tooltip::new("Component Sheet (debug builds)").build(w, cx)
                        })
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.run_command(CommandId::OpenComponents, w, cx)
                        })),
                )
            })
            .child(
                ui::icon_button("tb-settings", IconName::Settings, false, p)
                    .occlude()
                    .tooltip(|w, cx| {
                        Tooltip::new("Settings")
                            .action(&actions::OpenSettings, None)
                            .build(w, cx)
                    })
                    .on_click(
                        cx.listener(|this, _, w, cx| this.run_command(CommandId::Settings, w, cx)),
                    ),
            );
        let close_weak = cx.entity().downgrade();
        TitleBar::new()
            // Linux draws its own close button: ask about unsaved files there too.
            .on_close_window(move |_, window, cx| {
                let close = close_weak
                    .update(cx, |w, cx| w.request_close_window(window, cx))
                    .unwrap_or(true);
                if close {
                    window.remove_window();
                }
            })
            .h(rpx(38.))
            .bg(p.panel)
            .border_color(p.bd)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .gap(rpx(12.))
                    .pr(rpx(10.))
                    .font_family(SANS)
                    .child(left)
                    .child(search)
                    .child(right),
            )
            .into_any_element()
    }

    fn tab_info(
        &self,
        tab: &Tab,
        cx: &App,
    ) -> (SharedString, SharedString, Option<EnvironmentLabel>, bool) {
        match tab {
            Tab::Welcome => ("SY".into(), "Welcome".into(), None, false),
            Tab::Sql(t) => {
                let t = t.read(cx);
                let badge = t.connection.as_ref().map_or("SQL", |c| c.engine.badge());
                (
                    badge.into(),
                    t.title.clone(),
                    t.connection.as_ref().map(|c| c.environment),
                    t.dirty,
                )
            }
            Tab::Terminal(t) => {
                let t = t.read(cx);
                (
                    if t.is_remote() { "SSH" } else { "SH" }.into(),
                    t.title.clone(),
                    Some(t.env),
                    false,
                )
            }
            Tab::Files(f) => {
                let right = match f.read(cx).right_source() {
                    FsRef::Local => "local".to_owned(),
                    FsRef::Host(id) | FsRef::Ftp(id) | FsRef::S3(id) => self
                        .profiles
                        .all
                        .iter()
                        .find(|p| p.id() == id)
                        .map_or("local".to_owned(), |p| p.name().to_owned()),
                };
                ("FS".into(), format!("Files · {right}").into(), None, false)
            }
            Tab::Editor(e) => {
                let e = e.read(cx);
                ("ED".into(), e.title.clone().into(), None, e.dirty)
            }
            Tab::Workload(w) => (
                "WL".into(),
                format!("Workload · {}", w.read(cx).name).into(),
                None,
                false,
            ),
            Tab::Ddl(d) => {
                let d = d.read(cx);
                (d.badge.into(), d.title.clone(), None, false)
            }
            Tab::Activity(a) => {
                let a = a.read(cx);
                let title = format!("Activity · {}", a.connection.name);
                (
                    "AM".into(),
                    title.into(),
                    Some(a.connection.environment),
                    false,
                )
            }
            Tab::Object(o) => {
                let o = o.read(cx);
                (
                    o.badge().into(),
                    o.title.clone(),
                    Some(o.connection.environment),
                    false,
                )
            }
            Tab::Er(e) => {
                let e = e.read(cx);
                (
                    e.badge().into(),
                    e.title.clone(),
                    Some(e.connection.environment),
                    false,
                )
            }
            Tab::Redis(r) => {
                let c = &r.read(cx).connection;
                (
                    "RD".into(),
                    c.name.clone().into(),
                    Some(c.environment),
                    false,
                )
            }
        }
    }

    fn render_tab_strip(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use crate::split::SplitDir;
        let shown_other = self.split.as_ref().map(|s| s.other);
        // A small two-pane glyph, drawn so it doesn't depend on the font.
        let glyph = |dir: SplitDir, on: bool| {
            let c = if on { p.acc } else { p.fg3 };
            div()
                .w(rpx(14.))
                .h(rpx(11.))
                .flex()
                .border_1()
                .border_color(c)
                .rounded(px(2.))
                .map(|d| match dir {
                    SplitDir::Right => d.flex_row(),
                    SplitDir::Down => d.flex_col(),
                })
                .child(div().flex_1())
                .child(div().flex_none().bg(c).map(|d| match dir {
                    SplitDir::Right => d.w(rpx(1.)).h_full(),
                    SplitDir::Down => d.h(rpx(1.)).w_full(),
                }))
                .child(div().flex_1())
        };
        let current = self.split.as_ref().map(|s| s.dir);
        let split_btn = |id: &'static str,
                         dir: SplitDir,
                         tip: &'static str,
                         cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .px(rpx(6.))
                .rounded(px(4.))
                .hover(|s| s.bg(p.hover))
                .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(w, cx))
                .on_click(cx.listener(move |this, _, w, cx| this.split_view(dir, w, cx)))
                .child(glyph(dir, current == Some(dir)))
        };
        let controls = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(2.))
            .px(rpx(6.))
            .child(split_btn("split-right", SplitDir::Right, "Split right", cx))
            .child(split_btn("split-down", SplitDir::Down, "Split down", cx))
            .when(self.split.is_some(), |d| {
                d.child(
                    div()
                        .id("unsplit")
                        .px(rpx(6.))
                        .rounded(px(4.))
                        .text_size(ts::LABEL)
                        .text_color(p.fg3)
                        .hover(|s| s.bg(p.hover).text_color(p.fg))
                        .tooltip(|w, cx| {
                            gpui_kit::component::tooltip::Tooltip::new("Close split").build(w, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.split = None;
                            cx.notify();
                        }))
                        .child("Unsplit"),
                )
            });
        let colors = self.connection_colors(cx);
        let color_of = |tab: &Tab, cx: &App| -> Option<Hsla> {
            let Tab::Sql(s) = tab else { return None };
            let id = s.read(cx).connection.as_ref()?.id.clone();
            colors.iter().find(|(c, _)| *c == id).map(|(_, h)| *h)
        };
        // The SQL tabs use the same color for their editor bar and connection pill.
        for t in &self.tabs {
            if let Tab::Sql(s) = t {
                let c = color_of(t, cx);
                if s.read(cx).accent != c {
                    s.update(cx, |s, _| s.accent = c);
                }
            }
        }
        let tabs = div()
            .id("tab-strip")
            .flex_1()
            .min_w_0()
            .flex()
            .items_stretch()
            .overflow_x_scroll()
            .children(self.tabs.iter().enumerate().map(|(i, tab)| {
                let (badge, label, env, dirty) = self.tab_info(tab, cx);
                let active = i == self.active;
                // Visible in the other pane of a split.
                let shown = shown_other == Some(i);
                let db_color = color_of(tab, cx);
                let production = env == Some(EnvironmentLabel::Production);
                let edge: Hsla = match (db_color, env) {
                    (Some(c), _) => {
                        if active {
                            c
                        } else {
                            c.opacity(0.55)
                        }
                    }
                    (None, Some(e)) if e != EnvironmentLabel::Local || active => {
                        let c = p.env(e);
                        if active { c } else { c.opacity(0.45) }
                    }
                    _ => gpui_kit::transparent_black(),
                };
                div()
                    .id(("tab", i))
                    .relative()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(rpx(7.))
                    .pl(rpx(12.))
                    .pr(rpx(8.))
                    .min_w_0()
                    .max_w(rpx(230.))
                    .border_r_1()
                    .border_color(p.bd)
                    .text_size(ts::UI)
                    .bg(if active {
                        p.surface
                    } else if shown {
                        p.surface.opacity(0.6)
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .text_color(if active || shown { p.fg } else { p.fg2 })
                    .when(active, |d| d.mb(rpx(-1.)))
                    // A faint wash of the database color on the active tab.
                    .when_some(db_color.filter(|_| active), |d, c| d.bg(c.opacity(0.08)))
                    .on_click(cx.listener(move |this, _, _, cx| this.activate(i, cx)))
                    .on_mouse_down(
                        gpui_kit::MouseButton::Right,
                        cx.listener(move |this, ev: &gpui_kit::MouseDownEvent, _, cx| {
                            this.ctx = Some(crate::sidebar::CtxMenu::new(
                                ev.position,
                                crate::sidebar::CtxTarget::Tab(i),
                            ));
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .h(rpx(2.))
                            .bg(edge),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px(rpx(4.))
                            .rounded(px(3.))
                            .border_1()
                            .border_color(p.bd)
                            .text_color(p.fg2)
                            .font_family(MONO)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::MICRO)
                            .line_height(rpx(14.))
                            .child(badge),
                    )
                    // Production keeps its red marker whatever the database color.
                    .when(production, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px(rpx(3.))
                                .rounded(px(3.))
                                .bg(p.prod)
                                .text_color(gpui_kit::white())
                                .font_family(MONO)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(ts::MICRO)
                                .line_height(rpx(13.))
                                .child("P"),
                        )
                    })
                    .child(div().truncate().child(label))
                    // Unsaved marker (editor files, SQL text not yet autosaved).
                    .child(ui::dot(
                        if dirty {
                            p.stg
                        } else {
                            gpui_kit::transparent_black()
                        },
                        6.,
                    ))
                    .child(
                        div()
                            .id(("tab-close", i))
                            .flex_none()
                            .px(rpx(4.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .text_size(ts::SMALL)
                            .hover(|s| s.bg(p.hover).text_color(p.fg))
                            .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                cx.stop_propagation();
                                this.close_tab(i, w, cx);
                            }))
                            .child("×"),
                    )
            }))
            .child(
                div()
                    .id("tab-new")
                    .flex()
                    .items_center()
                    .px(rpx(10.))
                    .text_color(p.fg3)
                    .font_family(MONO)
                    .text_size(ts::BASE)
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|this, _, w, cx| this.new_query_tab(w, cx)))
                    .child("+"),
            )
            .child(div().flex_1());
        div()
            .h(rpx(34.))
            .flex_none()
            .flex()
            .items_stretch()
            .bg(p.panel)
            .border_b_1()
            .border_color(p.bd)
            .child(tabs)
            .child(controls)
            .into_any_element()
    }

    fn render_welcome(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use crate::conn_editor::ConnKind;
        let first = self.profiles_loaded && self.profiles.is_empty();
        let card = |id: &'static str, badge: &'static str, title: &'static str, body: String| {
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(rpx(6.))
                .p(rpx(14.))
                .border_1()
                .border_color(p.bd)
                .rounded(px(8.))
                .bg(p.panel)
                .hover(|s| s.border_color(p.bd2))
                .child(
                    div().flex().child(
                        div()
                            .px(rpx(4.))
                            .py(rpx(1.))
                            .border_1()
                            .border_color(p.bd2)
                            .rounded(px(3.))
                            .font_family(MONO)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::TINY)
                            .text_color(p.fg2)
                            .child(badge),
                    ),
                )
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(div().text_color(p.fg2).text_size(ts::BODY).child(body))
        };
        let ssh_hosts = std::env::var_os("HOME")
            .map(|h| std::path::Path::new(&h).join(".ssh/config"))
            .filter(|p| p.exists())
            .is_some();
        let recents: Vec<&Profile> = self
            .profiles
            .all
            .iter()
            .filter(|p| !matches!(p, Profile::Terminal(_)))
            .take(8)
            .collect();
        div()
            .id("welcome")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .justify_center()
            .px(rpx(32.))
            .py(rpx(64.))
            .child(
                div()
                    .w_full()
                    .max_w(rpx(680.))
                    .flex()
                    .flex_col()
                    .gap(rpx(28.))
                    .child(
                        div()
                            .child(
                                div()
                                    .text_size(ts::DISPLAY_L)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(if first { "Welcome to Switchyard" } else { greeting() }),
                            )
                            .child(div().mt(rpx(6.)).text_color(p.fg2).text_size(ts::BASE_PLUS).child(if first {
                                "Databases, terminals and file transfer behind one connection model. Nothing to install — PostgreSQL, SQL Server, SSH, SFTP and FTP drivers are built in."
                            } else {
                                "Pick up where you left off, or open something new."
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(rpx(10.))
                            .child(
                                card("w-host", "SSH", "New Host", "A server you reach over SSH. Terminals, files and tunnels reuse it.".into())
                                    .on_click(cx.listener(|this, _, w, cx| this.open_conn_editor(ConnKind::Ssh, None, w, cx))),
                            )
                            .child(
                                card("w-conn", "DB", "New Connection", "PostgreSQL, SQL Server, SFTP or FTP — direct or via a Host.".into())
                                    .on_click(cx.listener(|this, _, w, cx| this.open_conn_editor(ConnKind::Db(switchyard_core::db::Engine::Postgres), None, w, cx))),
                            )
                            .child(
                                card(
                                    "w-import",
                                    "~/.ssh",
                                    "Import ssh config",
                                    if ssh_hosts {
                                        "Turn the Host entries in ~/.ssh/config into Hosts, ProxyJump included.".into()
                                    } else {
                                        "No ~/.ssh/config found on this machine.".into()
                                    },
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.import_ssh_config(cx))),
                            ),
                    )
                    .when(!first && !recents.is_empty(), |d| {
                        d.child(
                            div()
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .px(rpx(2.))
                                        .pb(rpx(6.))
                                        .border_b_1()
                                        .border_color(p.bd)
                                        .child(ui::caption("RECENT", p))
                                        .child(ui::caption(format!("{} to switch", ui::keys("⌘P", "Ctrl+P")), p).font_family(MONO)),
                                )
                                .children(recents.into_iter().enumerate().map(|(i, prof)| {
                                    let id = prof.id().clone();
                                    div()
                                        .id(("recent", i))
                                        .h(rpx(32.))
                                        .flex()
                                        .items_center()
                                        .gap(rpx(8.))
                                        .px(rpx(4.))
                                        .border_b_1()
                                        .border_color(p.line)
                                        .text_size(ts::UI)
                                        .hover(|s| s.bg(p.hover))
                                        .on_click(cx.listener(move |this, _, w, cx| this.open_profile(&id, w, cx)))
                                        .child(div().w(rpx(14.)).child(ui::dot(p.env(prof.environment()), 7.)))
                                        .child(ui::monogram(badge_of(prof), 34., p))
                                        .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(prof.name().to_owned()))
                                        .child(
                                            div()
                                                .font_family(MONO)
                                                .text_size(ts::LABEL)
                                                .text_color(p.fg3)
                                                .child(describe(prof, &self.profiles)),
                                        )
                                }))
                                .into_any_element(),
                        )
                    })
                    .when(first, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(rpx(4.))
                                .p(rpx(22.))
                                .border_1()
                                .border_dashed()
                                .border_color(p.bd2)
                                .rounded(px(8.))
                                .text_color(p.fg2)
                                .text_size(ts::UI)
                                .child(div().text_color(p.fg).font_weight(FontWeight::MEDIUM).child("Nothing saved yet"))
                                .child("Start with a Host. Once it's saved, open a terminal, browse its files or tunnel a database through it with one login."),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap(rpx(18.))
                            .text_color(p.fg3)
                            .text_size(ts::BODY)
                            .child(shortcut_hint("Command palette", ui::keys("⇧⌘P", "Ctrl+Shift+P"), p))
                            .child(shortcut_hint("Quick switch", ui::keys("⌘P", "Ctrl+P"), p))
                            .child(shortcut_hint("Settings", ui::keys("⌘,", "Ctrl+,"), p)),
                    ),
            )
            .into_any_element()
    }

    fn render_status_bar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.tabs.get(self.active);
        let (env, conn, engine, rows, txn) = match tab {
            Some(Tab::Sql(t)) => {
                let t = t.read(cx);
                let env = t.connection.as_ref().map(|c| c.environment);
                let conn = t
                    .connection
                    .as_ref()
                    .map(|c| {
                        let via = c
                            .via_host
                            .as_ref()
                            .and_then(|h| self.profiles.host(h))
                            .map(|h| format!(" @ {}", h.name))
                            .unwrap_or_default();
                        format!("{}{via}", c.name)
                    })
                    .unwrap_or_else(|| "No connection".into());
                let engine = match &t.session_state {
                    SessionState::Open { version } => version.clone(),
                    SessionState::Connecting => "connecting…".into(),
                    SessionState::Failed(_) => "connection failed".into(),
                    SessionState::None => String::new(),
                };
                let (_, _, meta, _) = t.status(p);
                let txn = if t.txn_open {
                    (
                        format!(
                            "Transaction open · {} statement{}",
                            t.txn_statements,
                            if t.txn_statements == 1 { "" } else { "s" }
                        ),
                        p.stg,
                    )
                } else if t.manual_txn {
                    ("Manual · no open transaction".to_owned(), p.fg3)
                } else {
                    ("Auto-commit".to_owned(), p.fg3)
                };
                (env, conn, engine, meta, Some(txn))
            }
            Some(Tab::Terminal(t)) => {
                let t = t.read(cx);
                (
                    Some(t.env),
                    t.title.to_string(),
                    "terminal".into(),
                    t.status(),
                    None,
                )
            }
            Some(Tab::Files(_)) => (None, "Files".into(), String::new(), String::new(), None),
            _ => (
                None,
                self.workspace_name.clone(),
                String::new(),
                String::new(),
                None,
            ),
        };
        let pos = self.active_sql().map(|t| {
            let ed = t.read(cx).editor().read(cx);
            let pos = ed.cursor_position();
            format!("Ln {}, Col {}", pos.line + 1, pos.character + 1)
        });
        let (env_label, env_bg, env_fg) = match env {
            Some(e) => (e.name().to_uppercase(), p.env(e), p.env_on(e)),
            None => ("NO CONNECTION".into(), p.hover, p.fg2),
        };
        div()
            .h(rpx(24.))
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(14.))
            .border_t_1()
            .border_color(p.bd)
            .bg(p.panel)
            .text_size(ts::LABEL)
            .text_color(p.fg2)
            .whitespace_nowrap()
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .px(rpx(10.))
                    .bg(env_bg)
                    .text_color(env_fg)
                    .font_family(MONO)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::CAPTION)
                    .child(env_label),
            )
            .child(div().text_color(p.fg).child(conn))
            .when_some(txn, |d, (label, c)| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(5.))
                        .text_color(c)
                        .child(ui::dot(c, 6.))
                        .child(label),
                )
            })
            .child(
                div()
                    .id("sb-tunnels")
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tunnels_open = !this.tunnels_open;
                        cx.notify();
                    }))
                    .child(match self.tunnels.len() {
                        1 => "1 tunnel".to_owned(),
                        n => format!("{n} tunnels"),
                    }),
            )
            .child(div().flex_1())
            .children(self.render_update_notice(p, cx))
            .child(
                div()
                    .id("sb-transfers")
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|this, _, w, cx| this.open_files(w, cx)))
                    .child(match self.transfers.read(cx).summary() {
                        Some((n, speed)) if speed > 1.0 => format!(
                            "↑↓ {n} transfer{} · {}/s",
                            if n == 1 { "" } else { "s" },
                            crate::remote_files::human(speed as u64)
                        ),
                        Some((n, _)) => format!("↑↓ {n} transfer{}", if n == 1 { "" } else { "s" }),
                        None => "↑↓ no transfers".into(),
                    }),
            )
            .child(div().font_family(MONO).child(rows))
            .when_some(pos, |d, pos| {
                d.child(div().font_family(MONO).text_color(p.fg3).child(pos))
            })
            .child(div().pr(rpx(12.)).text_color(p.fg3).child(engine))
            .into_any_element()
    }
}

fn greeting() -> &'static str {
    use chrono::Timelike as _;
    greeting_for(chrono::Local::now().hour())
}

/// The greeting for an hour of the local day.
fn greeting_for(hour: u32) -> &'static str {
    match hour {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

fn shortcut_hint(label: &'static str, key: SharedString, p: &Palette) -> AnyElement {
    div()
        .flex()
        .gap(rpx(6.))
        .child(label)
        .child(div().font_family(MONO).text_color(p.fg2).child(key))
        .into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        if let Some((fs, path)) = self.pending_editor.take() {
            self.open_remote_file(fs, path, window, cx);
        }
        if let Some(host) = self.pending_files.take() {
            self.open_files_for(host, cx);
        }
        // Bind restored tabs whose connection arrived after the workspace.
        if !self.rebind.is_empty() && self.profiles_loaded {
            for (tab, id) in std::mem::take(&mut self.rebind) {
                let conn = id.as_ref().and_then(|id| self.profiles.db(id).cloned());
                tab.update(cx, |t, cx| t.set_connection(conn, cx));
            }
            self.sync_schema(cx);
        }
        // Remember the explorer's connection nodes (restored "not connected" on launch).
        if let Some(ids) = self.explorer.take_save() {
            let ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
            self.core.send(Command::SetSetting {
                key: crate::explorer::SAVED_NODES_KEY.into(),
                value: ids.into(),
            });
        }
        let mut assistant = self.assistant_open.then(|| {
            div()
                .w(rpx(400.))
                .flex_none()
                .border_l_1()
                .border_color(p.bd)
                .child(self.assistant_panel.clone())
        });
        // The API workspace shows the assistant beside the Workbench.
        let api_body = match (self.mode, &self.api) {
            (AppMode::Api, Some(api)) => Some(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(api.clone()),
                    )
                    .children(assistant.take())
                    .into_any_element(),
            ),
            _ => None,
        };
        let mode_menu = self.mode_menu.then(|| self.render_mode_menu(&p, cx));
        let center = self.render_center(&p, cx);
        let show_inspector =
            self.inspector_open && matches!(self.tabs.get(self.active), Some(Tab::Sql(_)));
        let sidebar = self
            .sidebar_open
            .then(|| self.render_sidebar(&p, window, cx));
        let inspector = show_inspector.then(|| self.render_inspector(&p, cx));
        let title = self.render_title_bar(&p, cx);
        let strip = self.render_tab_strip(&p, cx);
        let status = self.render_status_bar(&p, cx);
        let overlays = self.render_overlays(&p, window, cx);

        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(p.bg)
            .text_color(p.fg)
            .font_family(SANS)
            .text_size(crate::appearance::ui_font_size(cx))
            .on_action(cx.listener(|this, _: &actions::OpenPalette, w, cx| {
                this.open_palette(PaletteMode::Commands, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::QuickSwitch, w, cx| {
                this.open_palette(PaletteMode::Connections, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::RunStatement, w, cx| {
                this.run_command(CommandId::RunStatement, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::RunScript, w, cx| {
                this.run_command(CommandId::RunScript, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::StopQuery, w, cx| {
                this.run_command(CommandId::StopQuery, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::Explain, w, cx| {
                this.run_command(CommandId::Explain, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ExplainAnalyze, w, cx| {
                this.run_command(CommandId::ExplainAnalyze, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::NewTerminal, w, cx| {
                this.run_command(CommandId::NewTerminal, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::NewConnection, w, cx| {
                this.run_command(CommandId::NewConnection, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::NewQueryTab, w, cx| {
                this.run_command(CommandId::NewQueryTab, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::CloseTab, w, cx| {
                    this.close_tab(this.active, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::OpenSettings, w, cx| {
                this.open_settings(SettingsPage::General, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleAssistant, w, cx| {
                this.run_command(CommandId::ToggleAssistant, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleSidebar, w, cx| {
                this.run_command(CommandId::ToggleSidebar, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::FormatSql, w, cx| {
                this.run_command(CommandId::FormatSql, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ShowHistory, w, cx| {
                this.run_command(CommandId::ShowHistory, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::Dismiss, w, cx| this.dismiss(w, cx)))
            .on_mouse_move(cx.listener(|this, ev: &gpui_kit::MouseMoveEvent, w, cx| {
                if let Some((x0, w0)) = this.inspector_drag {
                    if ev.pressed_button == Some(gpui_kit::MouseButton::Left) {
                        // Widths are in 100 % design units (drawn with `rpx`).
                        let z = crate::appearance::zoom(cx);
                        let x: f32 = ev.position.x.into();
                        let max = (f32::from(w.bounds().size.width) / z - 420.)
                            .max(crate::sidebar::INSPECTOR_MIN);
                        this.inspector_width =
                            (w0 - (x - x0) / z).clamp(crate::sidebar::INSPECTOR_MIN, max);
                        cx.notify();
                    } else {
                        this.end_inspector_drag();
                    }
                }
                if let Some((x0, w0)) = this.sidebar_drag {
                    if ev.pressed_button == Some(gpui_kit::MouseButton::Left) {
                        // Widths are in 100 % design units (drawn with `rpx`).
                        let z = crate::appearance::zoom(cx);
                        let x: f32 = ev.position.x.into();
                        let max = (f32::from(w.bounds().size.width) / z - 420.)
                            .max(crate::sidebar::SIDEBAR_MIN);
                        this.sidebar_width =
                            (w0 + (x - x0) / z).clamp(crate::sidebar::SIDEBAR_MIN, max);
                        cx.notify();
                    } else {
                        this.end_sidebar_drag();
                    }
                }
            }))
            .on_mouse_up(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.end_inspector_drag();
                    this.end_sidebar_drag();
                }),
            )
            .on_action(cx.listener(|this, _: &actions::SplitRight, w, cx| {
                this.run_command(CommandId::SplitRight, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::SplitDown, w, cx| {
                this.run_command(CommandId::SplitDown, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::Unsplit, w, cx| {
                this.run_command(CommandId::Unsplit, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ZoomIn, w, cx| {
                this.run_command(CommandId::ZoomIn, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ZoomOut, w, cx| {
                this.run_command(CommandId::ZoomOut, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ZoomReset, w, cx| {
                this.run_command(CommandId::ResetZoom, w, cx)
            }))
            .child(title)
            .when_some(mode_menu, |d, menu| d.child(menu))
            .when_some(api_body, |d, body| d.child(body))
            .when(self.mode == AppMode::Default, |d| {
                d.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .children(sidebar)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .bg(p.surface)
                                .child(strip)
                                .child(div().flex_1().min_h_0().flex().flex_col().child(center)),
                        )
                        .children(inspector)
                        .children(assistant),
                )
            })
            .child(status)
            .children(overlays)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_follows_the_hour() {
        assert_eq!(greeting_for(4), "Good evening");
        assert_eq!(greeting_for(5), "Good morning");
        assert_eq!(greeting_for(11), "Good morning");
        assert_eq!(greeting_for(12), "Good afternoon");
        assert_eq!(greeting_for(17), "Good afternoon");
        assert_eq!(greeting_for(18), "Good evening");
        assert_eq!(greeting_for(23), "Good evening");
    }
}
