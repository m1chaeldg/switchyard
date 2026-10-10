//! Files tab: dual pane (this computer on the left, a Host or this computer on the right),
//! breadcrumbs, sortable columns, hidden-file toggle, new folder / rename / delete, drag
//! between panes and from the OS, and the transfer drawer at the bottom.

use std::path::{Path, PathBuf};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, ExternalPaths, FocusHandle,
    Focusable, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    deferred, div, px, uniform_list,
};
use switchyard_core::remote::{EntryKind, FileEntry};
use switchyard_core::store::ProfileId;
use switchyard_core::{Command, Event, FsOp, FsRef, OnConflict, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::remote_files::human;
use crate::theme::{MONO, Palette, palette};
use crate::transfers::Transfers;
use crate::ui::{self, Kind};

/// What the tab asks the workspace to do.
pub enum FilesTabEvent {
    /// Open a file in an editor tab.
    Open {
        /// Where.
        fs: FsRef,
        /// File.
        path: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortBy {
    Name,
    Size,
    Modified,
}

/// Files being dragged from one pane.
#[derive(Clone, Debug)]
pub struct DraggedFiles {
    pane: usize,
    fs: FsRef,
    paths: Vec<PathBuf>,
}

/// What follows the pointer while dragging.
pub struct DragPreview(pub String);

impl Render for DragPreview {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .px(rpx(8.))
            .py(rpx(4.))
            .rounded(px(5.))
            .bg(p.elev)
            .border_1()
            .border_color(p.acc)
            .text_size(ts::BODY)
            .text_color(p.fg)
            .child(self.0.clone())
    }
}

struct Pane {
    fs: FsRef,
    label: String,
    path: Option<PathBuf>,
    entries: Vec<FileEntry>,
    request: Option<RequestId>,
    error: Option<String>,
    sort: (SortBy, bool),
    show_hidden: bool,
    selected: Vec<String>,
    last_click: Option<(String, std::time::Instant)>,
}

impl Pane {
    fn new(fs: FsRef, label: String) -> Self {
        Self {
            fs,
            label,
            path: None,
            entries: Vec::new(),
            request: None,
            error: None,
            sort: (SortBy::Name, true),
            show_hidden: false,
            selected: Vec::new(),
            last_click: None,
        }
    }

    fn posix(&self) -> bool {
        self.fs.is_remote() || cfg!(unix)
    }

    fn join(&self, dir: &Path, name: &str) -> PathBuf {
        if self.posix() {
            let d = dir.to_string_lossy().replace('\\', "/");
            PathBuf::from(if d.ends_with('/') {
                format!("{d}{name}")
            } else {
                format!("{d}/{name}")
            })
        } else {
            dir.join(name)
        }
    }

    fn visible(&self) -> Vec<FileEntry> {
        let (by, asc) = self.sort;
        // Lowercase each name once, not once per comparison.
        let mut v: Vec<(String, &FileEntry)> = self
            .entries
            .iter()
            .filter(|e| self.show_hidden || !e.is_hidden())
            .map(|e| match by {
                SortBy::Name => (e.name.to_lowercase(), e),
                _ => (String::new(), e),
            })
            .collect();
        v.sort_by(|(ka, a), (kb, b)| {
            // Folders stay on top whatever the column.
            let dirs = b.is_dir().cmp(&a.is_dir());
            let ord = match by {
                SortBy::Name => ka.cmp(kb),
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Modified => a.modified_ms.cmp(&b.modified_ms),
            };
            dirs.then(if asc { ord } else { ord.reverse() })
        });
        v.into_iter().map(|(_, e)| e.clone()).collect()
    }

    /// `/`, `home`, `swy`, `app` with the path up to each.
    fn crumbs(&self) -> Vec<(String, PathBuf)> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut acc = PathBuf::new();
        for c in path.components() {
            acc.push(c);
            let label = match c {
                std::path::Component::RootDir => "/".to_owned(),
                std::path::Component::Prefix(p) => p.as_os_str().to_string_lossy().into_owned(),
                other => other.as_os_str().to_string_lossy().into_owned(),
            };
            if label == "\\" {
                continue;
            }
            out.push((label, acc.clone()));
        }
        out
    }
}

enum Edit {
    NewFolder,
    Rename(String),
}

/// The files tab.
pub struct FilesTab {
    core: RuntimeHandle,
    transfers: Entity<Transfers>,
    panes: [Pane; 2],
    active: usize,
    /// What the right pane can show: Hosts (SFTP) and FTP connections, with names.
    sources: Vec<(FsRef, String)>,
    picker_open: bool,
    edit: Option<(usize, Edit, Entity<InputState>)>,
    /// A pane's "go to folder" box while it is open.
    goto: Option<(usize, Entity<InputState>, Subscription)>,
    confirm_delete: Option<usize>,
    op: Option<RequestId>,
    error: Option<String>,
    focus: FocusHandle,
}

impl EventEmitter<FilesTabEvent> for FilesTab {}

/// The display name of `fs` among `sources`.
fn source_name(sources: &[(FsRef, String)], fs: &FsRef) -> String {
    match fs {
        FsRef::Local => "This computer".into(),
        _ => sources
            .iter()
            .find(|(s, _)| s == fs)
            .map(|(_, n)| n.clone())
            .unwrap_or_default(),
    }
}

fn fmt_time(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (y, m, d) = switchyard_core::db::value::civil_from_days(days);
    let mins = ms.rem_euclid(86_400_000) / 60_000;
    format!("{y}-{m:02}-{d:02} {:02}:{:02}", mins / 60, mins % 60)
}

impl FilesTab {
    /// Local on the left; `right` is a Host, an FTP connection or this computer.
    pub fn new(
        core: RuntimeHandle,
        transfers: Entity<Transfers>,
        sources: Vec<(FsRef, String)>,
        right: FsRef,
        cx: &mut Context<Self>,
    ) -> Self {
        let right_pane = Pane::new(right.clone(), source_name(&sources, &right));
        let mut this = Self {
            core,
            transfers,
            panes: [Pane::new(FsRef::Local, "This computer".into()), right_pane],
            active: 0,
            sources,
            picker_open: false,
            edit: None,
            goto: None,
            confirm_delete: None,
            op: None,
            error: None,
            focus: cx.focus_handle(),
        };
        this.list(0, None, cx);
        this.list(1, None, cx);
        this
    }

    /// The Host on the right, if any.
    pub fn right_host(&self) -> Option<&ProfileId> {
        match &self.panes[1].fs {
            FsRef::Host(h) => Some(h),
            FsRef::Local | FsRef::Ftp(_) | FsRef::S3(_) => None,
        }
    }

    /// What the right pane shows.
    pub fn right_source(&self) -> &FsRef {
        &self.panes[1].fs
    }

    /// Point the right pane at a Host, an FTP connection or this computer.
    pub fn show(&mut self, fs: FsRef, cx: &mut Context<Self>) {
        self.picker_open = false;
        let name = source_name(&self.sources, &fs);
        self.panes[1] = Pane::new(fs, name);
        self.list(1, None, cx);
    }

    /// Hosts and FTP connections for the right pane's picker.
    pub fn set_sources(&mut self, sources: Vec<(FsRef, String)>, cx: &mut Context<Self>) {
        if self.panes[1].fs.is_remote()
            && let Some((_, n)) = sources.iter().find(|(fs, _)| *fs == self.panes[1].fs)
        {
            self.panes[1].label = n.clone();
        }
        self.sources = sources;
        cx.notify();
    }

    fn list(&mut self, ix: usize, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let request = next_id();
        let pane = &mut self.panes[ix];
        pane.request = Some(request);
        self.core.send(Command::ListDir {
            request,
            fs: pane.fs.clone(),
            path,
        });
        cx.notify();
    }

    fn refresh(&mut self, ix: usize, cx: &mut Context<Self>) {
        let path = self.panes[ix].path.clone();
        self.list(ix, path, cx);
    }

    /// Runtime events.
    pub fn on_event(&mut self, ev: &Event, cx: &mut Context<Self>) {
        match ev {
            Event::FsListing {
                request,
                path,
                result,
                ..
            } => {
                let Some(ix) = self.panes.iter().position(|p| p.request == Some(*request)) else {
                    return;
                };
                let pane = &mut self.panes[ix];
                pane.request = None;
                match result {
                    Ok(entries) => {
                        if pane.path.as_ref() != Some(path) {
                            pane.selected.clear();
                        }
                        pane.path = Some(path.clone());
                        pane.entries = entries.clone();
                        pane.error = None;
                    }
                    // A folder that can't be opened leaves the current one on screen.
                    Err(e) if pane.path.is_some() && pane.error.is_none() => {
                        self.error = Some(format!("Can't open {}: {e}", path.to_string_lossy()));
                    }
                    Err(e) => pane.error = Some(e.clone()),
                }
            }
            Event::FsOpDone {
                request, result, ..
            } if Some(*request) == self.op => {
                self.op = None;
                self.error = result.as_ref().err().cloned();
                self.refresh(0, cx);
                self.refresh(1, cx);
            }
            // Changed elsewhere (sidebar panel, editor): refresh panes on that file system.
            Event::FsOpDone { fs, .. } => {
                for ix in 0..2 {
                    if self.panes[ix].fs == *fs {
                        self.refresh(ix, cx);
                    }
                }
            }
            Event::TransferDone { result: Ok(_), .. } => {
                self.refresh(0, cx);
                self.refresh(1, cx);
            }
            _ => return,
        }
        cx.notify();
    }

    /// Copy `paths` from `from` into the folder shown in pane `to_ix`.
    fn copy_into(
        &mut self,
        from: FsRef,
        paths: Vec<PathBuf>,
        to_ix: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.panes[to_ix].path.clone() else {
            return;
        };
        let to = self.panes[to_ix].fs.clone();
        self.transfers.update(cx, |t, cx| {
            for p in paths {
                t.start(
                    from.clone(),
                    p,
                    to.clone(),
                    Some(dir.clone()),
                    OnConflict::Ask,
                    cx,
                );
            }
        });
    }

    fn selected_paths(&self, ix: usize) -> Vec<PathBuf> {
        let pane = &self.panes[ix];
        let Some(dir) = &pane.path else {
            return Vec::new();
        };
        pane.selected.iter().map(|n| pane.join(dir, n)).collect()
    }

    fn copy_selection_across(&mut self, ix: usize, cx: &mut Context<Self>) {
        let paths = self.selected_paths(ix);
        if paths.is_empty() {
            return;
        }
        let from = self.panes[ix].fs.clone();
        self.copy_into(from, paths, 1 - ix, cx);
    }

    fn click(&mut self, ix: usize, e: &FileEntry, toggle: bool, cx: &mut Context<Self>) {
        self.active = ix;
        self.confirm_delete = None;
        let now = std::time::Instant::now();
        let pane = &mut self.panes[ix];
        let double = pane
            .last_click
            .as_ref()
            .is_some_and(|(n, t)| *n == e.name && now.duration_since(*t).as_millis() < 450);
        pane.last_click = Some((e.name.clone(), now));
        if toggle {
            if let Some(i) = pane.selected.iter().position(|n| *n == e.name) {
                pane.selected.remove(i);
            } else {
                pane.selected.push(e.name.clone());
            }
        } else {
            pane.selected = vec![e.name.clone()];
        }
        if double && let Some(dir) = pane.path.clone() {
            let full = pane.join(&dir, &e.name);
            if e.is_dir() {
                self.list(ix, Some(full), cx);
            } else {
                let fs = pane.fs.clone();
                cx.emit(FilesTabEvent::Open { fs, path: full });
            }
        }
        cx.notify();
    }

    fn up(&mut self, ix: usize, cx: &mut Context<Self>) {
        let up = self.panes[ix].path.as_ref().and_then(|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            let s = s.trim_end_matches('/').to_owned();
            if self.panes[ix].posix() {
                match s.rfind('/') {
                    Some(0) if s.len() > 1 => Some(PathBuf::from("/")),
                    Some(i) if i > 0 => Some(PathBuf::from(&s[..i])),
                    _ => None,
                }
            } else {
                p.parent().map(Path::to_path_buf)
            }
        });
        if let Some(up) = up {
            self.list(ix, Some(up), cx);
        }
    }

    fn open_goto(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.panes[ix]
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Folder path, ~ or a subfolder")
                .default_value(current)
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            move |this, input, ev: &InputEvent, _, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    let typed = input.read(cx).value().trim().to_owned();
                    this.goto = None;
                    let pane = &this.panes[ix];
                    let target = if typed.is_empty() {
                        None
                    } else if typed.starts_with('~')
                        || typed.starts_with('/')
                        || Path::new(&typed).is_absolute()
                    {
                        Some(PathBuf::from(&typed))
                    } else {
                        pane.path.as_deref().map(|d| pane.join(d, &typed))
                    };
                    if let Some(target) = target {
                        this.error = None;
                        this.list(ix, Some(target), cx);
                    }
                    cx.notify();
                }
                InputEvent::Blur => {
                    this.goto = None;
                    cx.notify();
                }
                _ => {}
            },
        );
        input.update(cx, |i, cx| {
            i.focus(window, cx);
            i.select_all(window, cx);
        });
        self.goto = Some((ix, input, sub));
        cx.notify();
    }

    fn start_edit(&mut self, ix: usize, rename: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (edit, value) = if rename {
            let Some(name) = self.panes[ix].selected.first().cloned() else {
                return;
            };
            (Edit::Rename(name.clone()), name)
        } else {
            (Edit::NewFolder, String::new())
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Folder name")
                .default_value(value)
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.edit = Some((ix, edit, input));
        cx.notify();
    }

    fn finish_edit(&mut self, cx: &mut Context<Self>) {
        let Some((ix, edit, input)) = self.edit.take() else {
            return;
        };
        let name = input.read(cx).value().trim().to_owned();
        let pane = &self.panes[ix];
        let Some(dir) = pane.path.clone() else { return };
        if name.is_empty() || name.contains('/') {
            cx.notify();
            return;
        }
        let op = match edit {
            Edit::NewFolder => FsOp::Mkdir(pane.join(&dir, &name)),
            Edit::Rename(old) if old != name => {
                FsOp::Rename(pane.join(&dir, &old), pane.join(&dir, &name))
            }
            Edit::Rename(_) => {
                cx.notify();
                return;
            }
        };
        let request = next_id();
        self.op = Some(request);
        self.core.send(Command::FsOp {
            request,
            fs: pane.fs.clone(),
            op,
        });
        cx.notify();
    }

    fn delete(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.confirm_delete != Some(ix) {
            self.confirm_delete = Some(ix);
            cx.notify();
            return;
        }
        self.confirm_delete = None;
        let fs = self.panes[ix].fs.clone();
        for path in self.selected_paths(ix) {
            let request = next_id();
            self.op = Some(request);
            self.core.send(Command::FsOp {
                request,
                fs: fs.clone(),
                op: FsOp::Delete(path),
            });
        }
        self.panes[ix].selected.clear();
        cx.notify();
    }

    fn render_pane(&mut self, ix: usize, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[ix];
        let rows = pane.visible();
        let count = rows.len();
        let active = self.active == ix;
        let n_sel = pane.selected.len();
        let tool = |id: String, label: &'static str| {
            div()
                .id(SharedString::from(id))
                .h(rpx(22.))
                .px(rpx(7.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .text_size(ts::LABEL)
                .text_color(p.fg2)
                .hover(|s| s.bg(p.hover))
                .child(label)
        };
        let arrow = if ix == 0 { "Copy →" } else { "← Copy" };
        let source: AnyElement = if ix == 0 {
            div()
                .text_size(ts::BODY)
                .font_weight(FontWeight::SEMIBOLD)
                .child(pane.label.clone())
                .into_any_element()
        } else {
            div()
                .id("files-source")
                .flex()
                .items_center()
                .gap(rpx(4.))
                .px(rpx(6.))
                .h(rpx(22.))
                .rounded(px(4.))
                .border_1()
                .border_color(p.bd2)
                .text_size(ts::BODY)
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.picker_open = !this.picker_open;
                    cx.notify();
                }))
                .child(pane.label.clone())
                .child(div().text_color(p.fg3).text_size(ts::TINY).child("▾"))
                .into_any_element()
        };
        let toolbar = div()
            .h(rpx(32.))
            .flex_none()
            .px(rpx(8.))
            .flex()
            .items_center()
            .gap(rpx(2.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(source)
            .child(div().flex_1())
            .child(
                tool(format!("f{ix}-up"), "↑")
                    .on_click(cx.listener(move |this, _, _, cx| this.up(ix, cx))),
            )
            .child(
                tool(format!("f{ix}-refresh"), "⟳")
                    .on_click(cx.listener(move |this, _, _, cx| this.refresh(ix, cx))),
            )
            .child(
                tool(
                    format!("f{ix}-hidden"),
                    if pane.show_hidden {
                        "Hidden ●"
                    } else {
                        "Hidden ○"
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.panes[ix].show_hidden = !this.panes[ix].show_hidden;
                    cx.notify();
                })),
            )
            .child(
                tool(format!("f{ix}-mkdir"), "New folder")
                    .on_click(cx.listener(move |this, _, w, cx| this.start_edit(ix, false, w, cx))),
            )
            .when(n_sel == 1, |d| {
                d.child(
                    tool(format!("f{ix}-rename"), "Rename").on_click(
                        cx.listener(move |this, _, w, cx| this.start_edit(ix, true, w, cx)),
                    ),
                )
            })
            .when(n_sel > 0, |d| {
                let confirming = self.confirm_delete == Some(ix);
                d.child(
                    tool(
                        format!("f{ix}-delete"),
                        if confirming { "Delete?" } else { "Delete" },
                    )
                    .when(confirming, |t| t.text_color(p.prod))
                    .on_click(cx.listener(move |this, _, _, cx| this.delete(ix, cx))),
                )
                .child(
                    ui::button(
                        SharedString::from(format!("f{ix}-copy")),
                        arrow,
                        Kind::Primary,
                        p,
                    )
                    .h(rpx(22.))
                    .text_size(ts::LABEL)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.copy_selection_across(ix, cx)),
                    ),
                )
            });
        let crumbs = pane.crumbs();
        let goto = self
            .goto
            .as_ref()
            .filter(|(i, ..)| *i == ix)
            .map(|(_, input, _)| input.clone());
        let crumb_bar = div()
            .id(SharedString::from(format!("f{ix}-path")))
            .h(rpx(26.))
            .flex_none()
            .px(rpx(10.))
            .flex()
            .items_center()
            .gap(rpx(2.))
            .overflow_hidden()
            .border_b_1()
            .border_color(p.bd)
            .font_family(MONO)
            .text_size(ts::SMALL)
            .when_some(goto, |d, input| {
                d.child(
                    div()
                        .flex_1()
                        .h(rpx(20.))
                        .flex()
                        .items_center()
                        .px(rpx(6.))
                        .border_1()
                        .border_color(p.acc)
                        .rounded(px(4.))
                        .child(Input::new(&input).appearance(false).text_size(ts::LABEL)),
                )
            })
            .when(self.goto.as_ref().is_none_or(|(i, ..)| *i != ix), |d| {
                d.cursor_text()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_goto(ix, window, cx)),
                    )
                    .children(crumbs.into_iter().enumerate().map(|(i, (label, path))| {
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(2.))
                            .when(i > 1, |d| d.child(div().text_color(p.fg3).child("/")))
                            .child(
                                div()
                                    .id(SharedString::from(format!("f{ix}-crumb-{i}")))
                                    .px(rpx(3.))
                                    .rounded(px(3.))
                                    .text_color(p.fg2)
                                    .hover(|s| s.bg(p.hover).text_color(p.fg))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.list(ix, Some(path.clone()), cx)
                                    }))
                                    .child(label),
                            )
                    }))
                    .child(div().flex_1().h_full())
                    .child(
                        div()
                            .id(SharedString::from(format!("f{ix}-home")))
                            .px(rpx(5.))
                            .rounded(px(3.))
                            .cursor_pointer()
                            .text_color(p.fg3)
                            .hover(|s| s.bg(p.hover).text_color(p.fg))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.list(ix, Some(PathBuf::from("~")), cx)
                            }))
                            .child("~"),
                    )
            });
        let (by, asc) = pane.sort;
        let head = |id: &'static str, label: &'static str, col: SortBy| {
            let mark = if by == col {
                if asc { " ↑" } else { " ↓" }
            } else {
                ""
            };
            div()
                .id(SharedString::from(format!("f{ix}-{id}")))
                .hover(|s| s.text_color(p.fg))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let s = &mut this.panes[ix].sort;
                    *s = if s.0 == col { (col, !s.1) } else { (col, true) };
                    cx.notify();
                }))
                .child(format!("{label}{mark}"))
        };
        let header = div()
            .h(rpx(24.))
            .flex_none()
            .flex()
            .items_center()
            .px(rpx(10.))
            .gap(rpx(8.))
            .text_size(ts::SMALL)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg2)
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex_1()
                    .pl(rpx(18.))
                    .child(head("sort-name", "Name", SortBy::Name)),
            )
            .child(div().w(rpx(72.)).flex().justify_end().child(head(
                "sort-size",
                "Size",
                SortBy::Size,
            )))
            .child(
                div()
                    .w(rpx(118.))
                    .child(head("sort-mod", "Modified", SortBy::Modified)),
            )
            .child(div().w(rpx(76.)).child("Mode"));

        let edit_row: Option<AnyElement> =
            self.edit
                .as_ref()
                .filter(|(i, _, _)| *i == ix)
                .map(|(_, e, input)| {
                    div()
                        .flex_none()
                        .px(rpx(10.))
                        .py(rpx(5.))
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .border_b_1()
                        .border_color(p.bd)
                        .bg(p.surface)
                        .child(div().text_size(ts::LABEL).text_color(p.fg2).child(match e {
                            Edit::NewFolder => "New folder:",
                            Edit::Rename(_) => "Rename to:",
                        }))
                        .child(
                            div()
                                .flex_1()
                                .h(rpx(24.))
                                .flex()
                                .items_center()
                                .px(rpx(7.))
                                .border_1()
                                .border_color(p.acc)
                                .rounded(px(4.))
                                .child(Input::new(input).appearance(false).text_size(ts::BODY)),
                        )
                        .child(
                            ui::button("f-edit-ok", "OK", Kind::Primary, p)
                                .h(rpx(22.))
                                .on_click(cx.listener(|this, _, _, cx| this.finish_edit(cx))),
                        )
                        .child(
                            ui::button("f-edit-cancel", "Cancel", Kind::Ghost, p)
                                .h(rpx(22.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.edit = None;
                                    cx.notify();
                                })),
                        )
                        .into_any_element()
                });

        let body: AnyElement = if let Some(e) = &pane.error {
            div()
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element()
        } else if pane.path.is_none() {
            div()
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(match pane.fs {
                    FsRef::Host(_) => "Opening SFTP on the Host's session…",
                    FsRef::Ftp(_) => "Connecting to the FTP server…",
                    FsRef::S3(_) => "Connecting to S3…",
                    FsRef::Local => "Loading…",
                })
                .into_any_element()
        } else if count == 0 {
            div()
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("Empty folder")
                .into_any_element()
        } else {
            let selected = pane.selected.clone();
            let fs = pane.fs.clone();
            let dir = pane.path.clone().unwrap_or_default();
            let posix = pane.posix();
            uniform_list(
                SharedString::from(format!("f{ix}-list")),
                count,
                cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                    let p = palette(cx);
                    range
                        .filter_map(|i| rows.get(i).cloned())
                        .map(|e| {
                            let is_sel = selected.contains(&e.name);
                            let (icon, color) = match &e.kind {
                                EntryKind::Dir => ("▸", p.acc),
                                EntryKind::Symlink(_) => ("↪", p.fg3),
                                EntryKind::File => ("·", p.fg3),
                            };
                            // Drag the whole selection when the row is part of it.
                            let names = if is_sel {
                                selected.clone()
                            } else {
                                vec![e.name.clone()]
                            };
                            let paths: Vec<PathBuf> = names
                                .iter()
                                .map(|n| {
                                    if posix {
                                        let d = dir.to_string_lossy().replace('\\', "/");
                                        PathBuf::from(format!("{}/{n}", d.trim_end_matches('/')))
                                    } else {
                                        dir.join(n)
                                    }
                                })
                                .collect();
                            let label = if names.len() == 1 {
                                names[0].clone()
                            } else {
                                format!("{} items", names.len())
                            };
                            let drag = DraggedFiles {
                                pane: ix,
                                fs: fs.clone(),
                                paths,
                            };
                            let entry = e.clone();
                            div()
                                .id(SharedString::from(format!("f{ix}-row-{}", e.name)))
                                .w_full()
                                .h(rpx(24.))
                                .px(rpx(10.))
                                .flex()
                                .items_center()
                                .gap(rpx(8.))
                                .text_size(ts::UI)
                                .when(is_sel, |d| d.bg(p.sel))
                                .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                                .on_click(cx.listener(
                                    move |this, ev: &gpui_kit::ClickEvent, _, cx| {
                                        let toggle = ev.modifiers().secondary();
                                        this.click(ix, &entry, toggle, cx)
                                    },
                                ))
                                .on_drag(drag, move |_, _, _, cx| {
                                    let label = label.clone();
                                    cx.new(|_| DragPreview(label))
                                })
                                .child(div().w(rpx(10.)).text_color(color).child(icon))
                                .child(div().flex_1().min_w_0().truncate().child(e.name.clone()))
                                .child(
                                    div()
                                        .w(rpx(72.))
                                        .flex()
                                        .justify_end()
                                        .font_family(MONO)
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg2)
                                        .child(if e.is_dir() {
                                            String::new()
                                        } else {
                                            human(e.size)
                                        }),
                                )
                                .child(
                                    div()
                                        .w(rpx(118.))
                                        .font_family(MONO)
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .child(e.modified_ms.map(fmt_time).unwrap_or_default()),
                                )
                                .child(
                                    div()
                                        .w(rpx(76.))
                                        .font_family(MONO)
                                        .text_size(ts::CAPTION_PLUS)
                                        .text_color(p.fg3)
                                        .child(e.mode_string()),
                                )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        };

        let picker: Option<AnyElement> = (ix == 1 && self.picker_open).then(|| {
            let mut items: Vec<(FsRef, String)> = vec![(FsRef::Local, "This computer".into())];
            items.extend(self.sources.iter().cloned());
            deferred(
                div()
                    .absolute()
                    .top(rpx(30.))
                    .left(rpx(8.))
                    .w(rpx(220.))
                    .py(rpx(4.))
                    .bg(p.elev)
                    .border_1()
                    .border_color(p.bd2)
                    .rounded(px(6.))
                    .shadow(ui::shadow(p))
                    .children(items.into_iter().enumerate().map(|(i, (fs, name))| {
                        let badge = match fs {
                            FsRef::Local => "FS",
                            FsRef::Host(_) => "SSH",
                            FsRef::Ftp(_) => "FTP",
                            FsRef::S3(_) => "S3",
                        };
                        div()
                            .id(SharedString::from(format!("files-pick-{i}")))
                            .px(rpx(10.))
                            .h(rpx(26.))
                            .flex()
                            .items_center()
                            .gap(rpx(6.))
                            .text_size(ts::BODY)
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(move |this, _, _, cx| this.show(fs.clone(), cx)))
                            .child(
                                div()
                                    .text_color(p.fg3)
                                    .font_family(MONO)
                                    .text_size(ts::TINY)
                                    .child(badge),
                            )
                            .child(name)
                    })),
            )
            .into_any_element()
        });

        let drop_bg = p.sel;
        div()
            .id(SharedString::from(format!("files-pane-{ix}")))
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .when(ix == 0, |d| d.border_r_1().border_color(p.bd))
            .when(active, |d| d.bg(p.surface))
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.active != ix {
                        this.active = ix;
                        cx.notify();
                    }
                }),
            )
            .drag_over::<DraggedFiles>(
                move |s, d, _, _| if d.pane != ix { s.bg(drop_bg) } else { s },
            )
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg))
            .on_drop(cx.listener(move |this, d: &DraggedFiles, _, cx| {
                if d.pane != ix {
                    this.copy_into(d.fs.clone(), d.paths.clone(), ix, cx);
                }
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.copy_into(FsRef::Local, paths.paths().to_vec(), ix, cx);
            }))
            .child(toolbar)
            .child(crumb_bar)
            .children(edit_row)
            .child(header)
            .child(body)
            .children(picker)
            .into_any_element()
    }
}

impl Focusable for FilesTab {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilesTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let left = self.render_pane(0, &p, cx);
        let right = self.render_pane(1, &p, cx);
        let drawer = self.transfers.update(cx, |t, cx| t.render_drawer(&p, cx));
        div()
            .key_context("FilesTab")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if this.edit.is_some() {
                    match ev.keystroke.key.as_str() {
                        "enter" => this.finish_edit(cx),
                        "escape" => {
                            this.edit = None;
                            cx.notify();
                        }
                        _ => {}
                    }
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .px(rpx(12.))
                        .py(rpx(5.))
                        .bg(p.prod_bg)
                        .text_color(p.prod)
                        .text_size(ts::BODY)
                        .child(e),
                )
            })
            .child(div().flex_1().min_h_0().flex().child(left).child(right))
            .child(drawer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(entries: Vec<(&str, bool, u64)>) -> Pane {
        let mut p = Pane::new(FsRef::Local, "x".into());
        p.path = Some(PathBuf::from("/home/swy/app"));
        p.entries = entries
            .into_iter()
            .map(|(n, dir, size)| FileEntry {
                name: n.into(),
                kind: if dir { EntryKind::Dir } else { EntryKind::File },
                size,
                modified_ms: Some(size as i64),
                mode: None,
            })
            .collect();
        p
    }

    #[test]
    fn sorting_keeps_folders_first() {
        let mut p = pane(vec![
            ("b.txt", false, 10),
            ("a.txt", false, 30),
            ("zdir", true, 0),
            (".env", false, 5),
        ]);
        let names = |p: &Pane| {
            p.visible()
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&p), ["zdir", "a.txt", "b.txt"]);
        p.sort = (SortBy::Size, false);
        assert_eq!(names(&p), ["zdir", "a.txt", "b.txt"]);
        p.sort = (SortBy::Size, true);
        p.show_hidden = true;
        assert_eq!(names(&p), ["zdir", ".env", "b.txt", "a.txt"]);
    }

    #[test]
    fn breadcrumbs() {
        let p = pane(vec![]);
        let c: Vec<_> = p.crumbs().into_iter().map(|(l, _)| l).collect();
        assert_eq!(c, ["/", "home", "swy", "app"]);
        assert_eq!(p.crumbs()[2].1, PathBuf::from("/home/swy"));
        assert_eq!(fmt_time(86_400_000 + 3_723_000), "1970-01-02 01:02");
    }
}
