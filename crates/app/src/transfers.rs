//! The one transfer queue, shared by the Files tab's drawer, the sidebar's Files panel and
//! the status bar: start, pause, resume, retry, cancel, conflict answers, speed and ETA.

use std::path::PathBuf;
use std::time::Instant;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px, relative,
};
use switchyard_core::{Command, Event, FsOp, FsRef, OnConflict, RuntimeHandle, TransferError};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::remote_files::human;
use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};

/// Where a transfer is.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Waiting for one of the four slots.
    Queued,
    /// Copying.
    Running,
    /// Paused by the user; the partial file waits on the target.
    Paused,
    /// Finished, at this path.
    Done(PathBuf),
    /// Failed; Retry continues from the last byte.
    Failed(String),
    /// The target exists: Replace, Keep both or Skip.
    Exists,
    /// An interrupted copy (bytes) is on the target: Resume or Start over.
    Partial(u64),
    /// Cancelled (partial file deleted).
    Cancelled,
}

/// One transfer.
#[derive(Clone, Debug)]
pub struct Item {
    /// Transfer id.
    pub id: u64,
    /// File or folder name.
    pub name: String,
    /// Source.
    pub from: FsRef,
    /// Source path.
    pub path: PathBuf,
    /// Target.
    pub to: FsRef,
    /// Target folder (`None` = Downloads).
    pub dir: Option<PathBuf>,
    /// State.
    pub state: State,
    /// Bytes copied.
    pub done: u64,
    /// Total bytes, once known.
    pub total: Option<u64>,
    /// Smoothed bytes per second.
    pub speed: f64,
    last: Option<(Instant, u64)>,
}

impl Item {
    /// Whether it still needs attention (not done, cancelled).
    pub fn active(&self) -> bool {
        matches!(
            self.state,
            State::Queued
                | State::Running
                | State::Paused
                | State::Failed(_)
                | State::Exists
                | State::Partial(_)
        )
    }

    /// Seconds left at the current speed.
    pub fn eta(&self) -> Option<u64> {
        let total = self.total?;
        (self.state == State::Running && self.speed > 1.0)
            .then(|| (total.saturating_sub(self.done) as f64 / self.speed) as u64)
    }

    /// Upload (towards a Host or FTP server) or download.
    pub fn upload(&self) -> bool {
        self.to.is_remote()
    }
}

/// The partial file a paused file transfer left. (For a folder there is no such file and
/// the delete simply fails; its finished files stay.)
fn part_path(it: &Item) -> Option<PathBuf> {
    let dir = match &it.dir {
        Some(d) => d.clone(),
        None => {
            PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?)
                .join("Downloads")
        }
    };
    let name = format!("{}{}", it.name, ".swypart");
    Some(match it.to {
        FsRef::Host(_) | FsRef::Ftp(_) | FsRef::S3(_) => {
            let d = dir.to_string_lossy().replace('\\', "/");
            PathBuf::from(format!("{}/{name}", d.trim_end_matches('/')))
        }
        FsRef::Local => dir.join(name),
    })
}

/// `1m 05s`, `45s`, `<1s`.
pub fn duration(secs: u64) -> String {
    if secs == 0 {
        "<1s".into()
    } else if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The queue.
pub struct Transfers {
    core: RuntimeHandle,
    /// Newest last.
    pub items: Vec<Item>,
}

impl Transfers {
    /// An empty queue.
    pub fn new(core: RuntimeHandle) -> Self {
        Self {
            core,
            items: Vec::new(),
        }
    }

    fn send(&self, it: &Item, on_conflict: OnConflict, resume: bool) {
        self.core.send(Command::Transfer {
            id: it.id,
            from: it.from.clone(),
            path: it.path.clone(),
            to: it.to.clone(),
            dir: it.dir.clone(),
            on_conflict,
            resume,
        });
    }

    /// Copy `path` from `from` into `dir` on `to`.
    pub fn start(
        &mut self,
        from: FsRef,
        path: PathBuf,
        to: FsRef,
        dir: Option<PathBuf>,
        on_conflict: OnConflict,
        cx: &mut Context<Self>,
    ) {
        let it = Item {
            id: next_id(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            from,
            path,
            to,
            dir,
            state: State::Running,
            done: 0,
            total: None,
            speed: 0.0,
            last: None,
        };
        self.send(&it, on_conflict, false);
        self.items.push(it);
        // Keep the history short: drop the oldest finished ones beyond 30.
        while self.items.len() > 30 {
            match self.items.iter().position(|i| !i.active()) {
                Some(i) => {
                    self.items.remove(i);
                }
                None => break,
            }
        }
        cx.notify();
    }

    fn item(&mut self, id: u64) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// Pause a running or queued transfer.
    pub fn pause(&mut self, id: u64) {
        self.core.send(Command::PauseTransfer { id });
    }

    /// Resume a paused transfer, or retry a failed one, from its last byte.
    pub fn resume(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(it) = self.item(id) else { return };
        it.state = State::Running;
        it.last = None;
        it.speed = 0.0;
        let it = it.clone();
        self.send(&it, OnConflict::Replace, true);
        cx.notify();
    }

    /// Cancel: the runtime deletes a running transfer's partial file; for a paused or
    /// failed file transfer the leftover `.swypart` is deleted here.
    pub fn cancel(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(it) = self.item(id) else { return };
        match it.state {
            State::Running | State::Queued => self.core.send(Command::CancelTransfer { id }),
            _ => {
                it.state = State::Cancelled;
                let it = it.clone();
                if let Some(part) = part_path(&it) {
                    self.core.send(Command::FsOp {
                        request: next_id(),
                        fs: it.to.clone(),
                        op: FsOp::Delete(part),
                    });
                }
                cx.notify();
            }
        }
    }

    /// Answer "target exists".
    pub fn resolve(&mut self, id: u64, policy: Option<OnConflict>, cx: &mut Context<Self>) {
        let Some(it) = self.item(id) else { return };
        match policy {
            Some(p) => {
                it.state = State::Running;
                let it = it.clone();
                self.send(&it, p, false);
            }
            None => it.state = State::Cancelled,
        }
        cx.notify();
    }

    /// Forget finished transfers.
    pub fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.items.retain(|i| i.active());
        cx.notify();
    }

    /// Transfer events. Returns whether one finished successfully (panes refresh then).
    pub fn on_event(&mut self, ev: &Event, cx: &mut Context<Self>) -> bool {
        let mut finished = false;
        match ev {
            Event::TransferQueued { id } => {
                if let Some(it) = self.item(*id) {
                    it.state = State::Queued;
                }
            }
            Event::TransferProgress {
                id, done, total, ..
            } => {
                if let Some(it) = self.item(*id) {
                    let now = Instant::now();
                    // The first sample after a (re)start may jump to the resume point;
                    // that is not speed.
                    if let Some((t, d)) = it.last.filter(|(_, d)| *d > 0) {
                        let dt = now.duration_since(t).as_secs_f64();
                        if dt > 0.05 && *done >= d {
                            let rate = (*done - d) as f64 / dt;
                            it.speed = if it.speed == 0.0 {
                                rate
                            } else {
                                it.speed * 0.7 + rate * 0.3
                            };
                        }
                    }
                    if it
                        .last
                        .is_none_or(|(t, _)| now.duration_since(t).as_millis() > 50)
                    {
                        it.last = Some((now, *done));
                    }
                    it.state = State::Running;
                    it.done = *done;
                    it.total = *total;
                }
            }
            Event::TransferDone { id, result } => {
                if let Some(it) = self.item(*id) {
                    it.state = match result {
                        Ok(p) => {
                            finished = true;
                            if let Some(t) = it.total {
                                it.done = t;
                            }
                            State::Done(p.clone())
                        }
                        Err(TransferError::Exists(_)) => State::Exists,
                        Err(TransferError::Partial(n)) => {
                            it.done = *n;
                            State::Partial(*n)
                        }
                        Err(TransferError::Paused) => State::Paused,
                        Err(TransferError::Cancelled) => State::Cancelled,
                        Err(TransferError::Failed(e)) => State::Failed(e.clone()),
                    };
                    it.speed = 0.0;
                }
            }
            _ => return false,
        }
        cx.notify();
        finished
    }

    /// Running count and combined speed, for the status bar.
    pub fn summary(&self) -> Option<(usize, f64)> {
        let running: Vec<_> = self
            .items
            .iter()
            .filter(|i| matches!(i.state, State::Running | State::Queued))
            .collect();
        (!running.is_empty()).then(|| (running.len(), running.iter().map(|i| i.speed).sum()))
    }

    /// One transfer row with its controls; `compact` for the narrow sidebar.
    pub fn render_item(
        &self,
        it: &Item,
        compact: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = it.id;
        let arrow = if it.upload() { "↑" } else { "↓" };
        let pct = it
            .total
            .filter(|t| *t > 0)
            .map(|t| (it.done as f32 / t as f32).min(1.0));
        let (status, color) = match &it.state {
            State::Queued => ("queued · 4 run at once".to_owned(), p.fg3),
            State::Running => {
                let mut s = match it.total {
                    Some(t) => format!("{} of {}", human(it.done), human(t)),
                    None => "starting…".into(),
                };
                if it.speed > 1.0 {
                    s.push_str(&format!(" · {}/s", human(it.speed as u64)));
                }
                if let Some(e) = it.eta() {
                    s.push_str(&format!(" · {} left", duration(e)));
                }
                (s, p.fg2)
            }
            State::Paused => (
                format!(
                    "paused at {}{}",
                    human(it.done),
                    it.total
                        .map(|t| format!(" of {}", human(t)))
                        .unwrap_or_default()
                ),
                p.stg,
            ),
            State::Done(path) => (
                if it.upload() {
                    "done".to_owned()
                } else {
                    format!("saved to {}", path.display())
                },
                p.dev,
            ),
            State::Failed(e) => (e.clone(), p.prod),
            State::Exists => ("already exists".into(), p.stg),
            State::Partial(n) => (
                format!("an interrupted copy ({}) is already there", human(*n)),
                p.stg,
            ),
            State::Cancelled => ("cancelled".into(), p.fg3),
        };
        let link = |sid: String, label: &'static str, color| {
            div()
                .id(SharedString::from(sid))
                .px(rpx(5.))
                .rounded(px(3.))
                .text_size(ts::SMALL)
                .text_color(color)
                .hover(|s| s.bg(p.hover))
                .child(label)
        };
        let mut actions: Vec<AnyElement> = Vec::new();
        match &it.state {
            State::Running | State::Queued => {
                actions.push(
                    link(format!("tx-pause-{id}"), "Pause", p.fg2)
                        .on_click(cx.listener(move |this, _, _, _| this.pause(id)))
                        .into_any_element(),
                );
                actions.push(
                    link(format!("tx-cancel-{id}"), "Cancel", p.fg2)
                        .on_click(cx.listener(move |this, _, _, cx| this.cancel(id, cx)))
                        .into_any_element(),
                );
            }
            State::Paused | State::Failed(_) => {
                let label = if it.state == State::Paused {
                    "Resume"
                } else {
                    "Retry"
                };
                actions.push(
                    link(format!("tx-resume-{id}"), label, p.acc)
                        .on_click(cx.listener(move |this, _, _, cx| this.resume(id, cx)))
                        .into_any_element(),
                );
                actions.push(
                    link(format!("tx-cancel-{id}"), "Remove", p.fg3)
                        .on_click(cx.listener(move |this, _, _, cx| this.cancel(id, cx)))
                        .into_any_element(),
                );
            }
            State::Partial(_) => {
                actions.push(
                    link(format!("tx-resume-{id}"), "Resume", p.acc)
                        .on_click(cx.listener(move |this, _, _, cx| this.resume(id, cx)))
                        .into_any_element(),
                );
                actions.push(
                    link(format!("tx-over-{id}"), "Start over", p.fg2)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.resolve(id, Some(OnConflict::Replace), cx)
                        }))
                        .into_any_element(),
                );
                actions.push(
                    link(format!("tx-skip-{id}"), "Skip", p.fg3)
                        .on_click(cx.listener(move |this, _, _, cx| this.resolve(id, None, cx)))
                        .into_any_element(),
                );
            }
            State::Exists => {
                for (label, policy) in [
                    ("Replace", Some(OnConflict::Replace)),
                    ("Keep both", Some(OnConflict::KeepBoth)),
                    ("Skip", None),
                ] {
                    actions.push(
                        link(format!("tx-{label}-{id}"), label, p.acc)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.resolve(id, policy, cx)),
                            )
                            .into_any_element(),
                    );
                }
            }
            _ => {}
        }
        let bar =
            pct.filter(|_| matches!(it.state, State::Running | State::Paused | State::Queued));
        let progress_bar = |f: f32| {
            div().h(rpx(3.)).rounded(px(2.)).bg(p.bd).child(
                div()
                    .h_full()
                    .rounded(px(2.))
                    .bg(if it.state == State::Paused {
                        p.stg
                    } else {
                        p.acc
                    })
                    .w(relative(f)),
            )
        };
        if compact {
            return div()
                .px(rpx(8.))
                .py(rpx(3.))
                .flex()
                .flex_col()
                .gap(rpx(3.))
                .text_size(ts::LABEL)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .child(div().text_color(p.fg3).child(arrow))
                        .child(div().flex_1().min_w_0().truncate().child(it.name.clone())),
                )
                .when_some(bar, |d, f| d.child(progress_bar(f)))
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(ts::CAPTION_PLUS)
                        .text_color(color)
                        .truncate()
                        .child(status),
                )
                .when(!actions.is_empty(), |d| {
                    d.child(div().flex().flex_wrap().gap(rpx(2.)).children(actions))
                })
                .into_any_element();
        }
        div()
            .h(rpx(34.))
            .px(rpx(12.))
            .flex()
            .items_center()
            .gap(rpx(10.))
            .border_b_1()
            .border_color(p.line)
            .text_size(ts::BODY)
            .child(div().w(rpx(12.)).text_color(p.fg3).child(arrow))
            .child(
                div()
                    .w(rpx(220.))
                    .flex_none()
                    .min_w_0()
                    .truncate()
                    .child(it.name.clone()),
            )
            .child(
                div()
                    .w(rpx(160.))
                    .flex_none()
                    .child(progress_bar(bar.unwrap_or(match it.state {
                        State::Done(_) => 1.0,
                        _ => 0.0,
                    }))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(color)
                    .truncate()
                    .child(status),
            )
            .child(div().flex().gap(rpx(2.)).children(actions))
            .into_any_element()
    }

    /// The drawer at the bottom of the Files tab.
    pub fn render_drawer(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let items: Vec<Item> = self.items.iter().rev().cloned().collect();
        let (running, speed) = self.summary().unwrap_or((0, 0.0));
        let rows: Vec<AnyElement> = items
            .iter()
            .map(|it| self.render_item(it, false, p, cx))
            .collect();
        div()
            .flex_none()
            .h(rpx(170.))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(
                div()
                    .h(rpx(28.))
                    .flex_none()
                    .px(rpx(12.))
                    .flex()
                    .items_center()
                    .gap(rpx(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(ts::LABEL)
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("Transfers"),
                    )
                    .child(div().text_color(p.fg3).child(if running > 0 {
                        format!("{running} active · {}/s", human(speed as u64))
                    } else if items.is_empty() {
                        "Drag files between the panes, or drop them from your computer".into()
                    } else {
                        "idle".into()
                    }))
                    .child(div().flex_1())
                    .when(items.iter().any(|i| !i.active()), |d| {
                        d.child(
                            ui::button("tx-clear", "Clear finished", Kind::Ghost, p)
                                .h(rpx(20.))
                                .text_size(ts::SMALL)
                                .on_click(cx.listener(|this, _, _, cx| this.clear_finished(cx))),
                        )
                    }),
            )
            .child(
                div()
                    .id("tx-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(0), "<1s");
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(65), "1m 05s");
        assert_eq!(duration(3725), "1h 02m");
    }
}
