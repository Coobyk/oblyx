use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    empty::{Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle},
    form::{Field, Form},
    group_box::GroupBox,
    h_flex,
    input::{Input, InputState, NumberInput},
    notification::{Notification, NotificationType},
    progress::Progress,
    radio::RadioGroup,
    spinner::Spinner,
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rayon::prelude::*;

use crate::convert::{
    ConvertOptions, Fmt, JobOutcome, JobProgress, collect_inputs, convert_job_with, plan_jobs,
};
use crate::mem::{JobLimiter, MemBudget};

/// A user-picked input root: a single notebook file or a folder to scan.
#[derive(Clone, Debug)]
struct Source {
    path: PathBuf,
    is_dir: bool,
}

#[derive(Clone, Debug)]
enum Status {
    Ready,
    Running {
        preparing: bool,
        done: usize,
        total: usize,
    },
    Done {
        pages: usize,
        written: usize,
        secs: f32,
    },
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug)]
struct Entry {
    file: PathBuf,
    status: Status,
}

#[derive(Clone, Debug)]
struct Summary {
    ok: usize,
    failed: usize,
    cancelled: usize,
    pages: usize,
    written: usize,
    secs: f32,
    root: Option<PathBuf>,
}

enum RunEvent {
    Start(usize),
    Progress(usize, JobProgress),
    Done(usize, Result<JobOutcome>),
}

pub struct ConverterView {
    sources: Vec<Source>,
    files: Vec<PathBuf>,
    scan_error: Option<String>,

    fmt_svg: bool,
    fmt_png: bool,
    fmt_pdf: bool,
    output_mode: usize,
    output_dir: Entity<InputState>,
    dpi: Entity<InputState>,
    page: Entity<InputState>,
    include_deleted: bool,

    entries: Vec<Entry>,
    running: bool,
    cancelling: bool,
    started: Option<Instant>,
    cancel: Arc<AtomicBool>,
    summary: Option<Summary>,
    task: Option<Task<()>>,
    picker: Option<Task<()>>,
}

impl ConverterView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let output_dir =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/output-folder"));
        let dpi = cx.new(|cx| InputState::new(window, cx).default_value("144"));
        let page = cx.new(|cx| InputState::new(window, cx).placeholder("All pages"));
        Self {
            sources: Vec::new(),
            files: Vec::new(),
            scan_error: None,
            fmt_svg: true,
            fmt_png: false,
            fmt_pdf: false,
            output_mode: 0,
            output_dir,
            dpi,
            page,
            include_deleted: false,
            entries: Vec::new(),
            running: false,
            cancelling: false,
            started: None,
            cancel: Arc::new(AtomicBool::new(false)),
            summary: None,
            task: None,
            picker: None,
        }
    }

    // ----- sources ----------------------------------------------------------

    fn add_sources(&mut self, paths: Vec<PathBuf>, is_dir: bool) {
        for path in paths {
            if !self.sources.iter().any(|s| s.path == path) {
                self.sources.push(Source { path, is_dir });
            }
        }
        self.rescan();
    }

    fn remove_source(&mut self, index: usize) {
        self.sources.remove(index);
        self.rescan();
    }

    fn clear_sources(&mut self) {
        self.sources.clear();
        self.rescan();
    }

    fn rescan(&mut self) {
        if self.sources.is_empty() {
            self.files.clear();
            self.scan_error = None;
        } else {
            match collect_inputs(
                &self
                    .sources
                    .iter()
                    .map(|s| s.path.clone())
                    .collect::<Vec<_>>(),
            ) {
                Ok(files) => {
                    self.files = files;
                    self.scan_error = None;
                }
                Err(err) => {
                    self.files.clear();
                    self.scan_error = Some(format!("{err:#}"));
                }
            }
        }
        if !self.running {
            self.entries = self
                .files
                .iter()
                .map(|file| Entry {
                    file: file.clone(),
                    status: Status::Ready,
                })
                .collect();
            self.summary = None;
        }
    }

    // ----- pickers ----------------------------------------------------------

    fn pick_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.background_spawn(async move {
            rfd::FileDialog::new()
                .set_title("Add GoodNotes notebooks")
                .add_filter("GoodNotes notebooks", &["goodnotes"])
                .pick_files()
        });
        self.picker = Some(cx.spawn(async move |this, cx| {
            let Some(paths) = picked.await else {
                return;
            };
            let _ = this.update(cx, |view, _| view.add_sources(paths, false));
        }));
    }

    fn pick_folder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.background_spawn(async move {
            rfd::FileDialog::new()
                .set_title("Add folder to scan for notebooks")
                .pick_folder()
        });
        self.picker = Some(cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else {
                return;
            };
            let _ = this.update(cx, |view, _| view.add_sources(vec![path], true));
        }));
    }

    fn pick_output(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let entity_id = cx.entity().entity_id();
        let view = cx.entity();
        let picked = cx.background_spawn(async move {
            rfd::FileDialog::new()
                .set_title("Choose output folder")
                .pick_folder()
        });
        self.picker = Some(cx.spawn(async move |_this, cx| {
            let Some(dir) = picked.await else {
                return;
            };
            let raw = dir.to_string_lossy().into_owned();
            let _ = cx.with_window(entity_id, |window, cx| {
                view.update(cx, |view, cx| {
                    view.output_mode = 1;
                    view.output_dir
                        .update(cx, |state, cx| state.set_value(raw.clone(), window, cx));
                    cx.notify();
                });
            });
        }));
    }

    // ----- conversion run ---------------------------------------------------

    fn formats(&self) -> Vec<Fmt> {
        let mut formats = Vec::new();
        if self.fmt_svg {
            formats.push(Fmt::Svg);
        }
        if self.fmt_png {
            formats.push(Fmt::Png);
        }
        if self.fmt_pdf {
            formats.push(Fmt::Pdf);
        }
        formats
    }

    fn toast(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
        window.push_notification(
            Notification::new()
                .message(message)
                .with_type(NotificationType::Error),
            cx,
        );
    }

    fn start_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let formats = self.formats();
        if formats.is_empty() {
            Self::toast(window, cx, "Select at least one output format.");
            return;
        }
        let dpi_raw = self.dpi.read(cx).value().to_string();
        let dpi: f32 = match dpi_raw.trim().parse::<f32>() {
            Ok(dpi) if dpi.is_finite() && dpi > 0.0 => dpi,
            _ => {
                Self::toast(window, cx, "DPI must be a positive number.");
                return;
            }
        };
        let page_raw = self.page.read(cx).value().to_string();
        let page = {
            let trimmed = page_raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        let output = if self.output_mode == 1 {
            let raw = self.output_dir.read(cx).value().to_string();
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                Self::toast(window, cx, "Choose an output folder first.");
                return;
            }
            Some(PathBuf::from(trimmed))
        } else {
            None
        };
        let options = ConvertOptions {
            formats,
            output,
            dpi,
            page,
            include_deleted: self.include_deleted,
        };
        if let Err(err) = options.validate() {
            Self::toast(window, cx, format!("{err:#}"));
            return;
        }
        let files = match collect_inputs(
            &self
                .sources
                .iter()
                .map(|s| s.path.clone())
                .collect::<Vec<_>>(),
        ) {
            Ok(files) if !files.is_empty() => files,
            Ok(_) => {
                Self::toast(window, cx, "Add .goodnotes files to convert.");
                return;
            }
            Err(err) => {
                Self::toast(window, cx, format!("{err:#}"));
                return;
            }
        };
        self.files = files;
        let jobs = plan_jobs(&self.files, &options.formats, &options.output);
        if jobs.is_empty() {
            Self::toast(window, cx, "Nothing to convert.");
            return;
        }

        self.entries = jobs
            .iter()
            .map(|job| Entry {
                file: job.file.clone(),
                status: Status::Ready,
            })
            .collect();
        self.summary = None;
        self.running = true;
        self.cancelling = false;
        self.started = Some(Instant::now());
        self.cancel = Arc::new(AtomicBool::new(false));

        let cancel = Arc::clone(&self.cancel);
        let (tx, rx) = mpsc::channel::<RunEvent>();
        let tx = Mutex::new(tx);
        let bg = cx.background_spawn(async move {
            let limiter = JobLimiter::new(
                MemBudget::from_env(),
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            );
            jobs.par_iter().enumerate().for_each(|(index, job)| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                limiter.run(|| {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let _ = tx.lock().unwrap().send(RunEvent::Start(index));
                    let result = convert_job_with(job, &options, |update| {
                        let _ = tx.lock().unwrap().send(RunEvent::Progress(index, update));
                    });
                    let _ = tx.lock().unwrap().send(RunEvent::Done(index, result));
                });
            });
        });

        let task = cx.spawn(async move |this, cx| {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        if this
                            .update(cx, |view, cx| {
                                view.apply_event(event);
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(50))
                            .await;
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            let _ = bg.await;
            let _ = this.update(cx, |view, cx| {
                view.finish_run(cx);
                cx.notify();
            });
        });
        self.task = Some(task);
    }

    fn apply_event(&mut self, event: RunEvent) {
        match event {
            RunEvent::Start(index) => {
                if let Some(entry) = self.entries.get_mut(index) {
                    entry.status = Status::Running {
                        preparing: true,
                        done: 0,
                        total: 0,
                    };
                }
            }
            RunEvent::Progress(index, update) => {
                if let Some(entry) = self.entries.get_mut(index)
                    && let Status::Running {
                        preparing,
                        done,
                        total,
                    } = &mut entry.status
                {
                    match update {
                        JobProgress::Decode { done: d, total: t } => {
                            *preparing = true;
                            *done = d;
                            *total = t;
                        }
                        JobProgress::Render { done: d, total: t } => {
                            *preparing = false;
                            *done = d;
                            *total = t;
                        }
                    }
                }
            }
            RunEvent::Done(index, result) => {
                if let Some(entry) = self.entries.get_mut(index) {
                    entry.status = match result {
                        Ok(outcome) => Status::Done {
                            pages: outcome.pages,
                            written: outcome.written,
                            secs: outcome.took.as_secs_f32(),
                        },
                        Err(err) => Status::Failed(format!("{err:#}")),
                    };
                }
            }
        }
    }

    fn finish_run(&mut self, cx: &mut Context<Self>) {
        self.running = false;
        self.cancelling = false;
        for entry in &mut self.entries {
            if matches!(entry.status, Status::Ready | Status::Running { .. }) {
                entry.status = Status::Cancelled;
            }
        }
        let ok = self
            .entries
            .iter()
            .filter(|e| matches!(e.status, Status::Done { .. }))
            .count();
        let failed = self
            .entries
            .iter()
            .filter(|e| matches!(e.status, Status::Failed(_)))
            .count();
        let cancelled = self
            .entries
            .iter()
            .filter(|e| matches!(e.status, Status::Cancelled))
            .count();
        let (pages, written) = self
            .entries
            .iter()
            .fold((0usize, 0usize), |acc, entry| match entry.status {
                Status::Done { pages, written, .. } => (acc.0 + pages, acc.1 + written),
                _ => acc,
            });
        let secs = self
            .started
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.);
        let root = if self.output_mode == 1 {
            let raw = self.output_dir.read(cx).value().to_string();
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(PathBuf::from(trimmed))
            }
        } else {
            self.files
                .first()
                .and_then(|file| file.parent())
                .map(Path::to_path_buf)
        };
        self.summary = Some(Summary {
            ok,
            failed,
            cancelled,
            pages,
            written,
            secs,
            root,
        });
        self.started = None;
    }

    fn request_cancel(&mut self) {
        if self.running && !self.cancelling {
            self.cancel.store(true, Ordering::Relaxed);
            self.cancelling = true;
        }
    }

    // ----- render -----------------------------------------------------------

    fn section_title(text: &'static str) -> impl IntoElement {
        div()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .child(text)
    }

    fn render_sources(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.running;
        let sources = self.sources.clone();
        let count = self.files.len();
        let scan_error = self.scan_error.clone();

        let toolbar = h_flex()
            .gap_2()
            .child(
                Button::new("add-files")
                    .outline()
                    .icon(IconName::FileText)
                    .label("Add files…")
                    .disabled(running)
                    .on_click(cx.listener(|view, _, window, cx| view.pick_files(window, cx))),
            )
            .child(
                Button::new("add-folder")
                    .outline()
                    .icon(IconName::FolderOpen)
                    .label("Add folder…")
                    .disabled(running)
                    .on_click(cx.listener(|view, _, window, cx| view.pick_folder(window, cx))),
            )
            .child(
                Button::new("clear-sources")
                    .text()
                    .label("Clear")
                    .disabled(running || sources.is_empty())
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.clear_sources();
                        cx.notify();
                    })),
            );

        let mut list = v_flex()
            .gap_1()
            .child(toolbar)
            .when_some(scan_error, |this, err| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(err))
            });

        if sources.is_empty() {
            list = list.child(
                Empty::new().min_h(px(170.)).header(
                    EmptyHeader::new()
                        .media(
                            EmptyMedia::new()
                                .with_variant(EmptyMediaVariant::Icon)
                                .child(Icon::new(IconName::FileText)),
                        )
                        .title(EmptyTitle::new().child("No notebooks added"))
                        .description(
                            EmptyDescription::new()
                                .child("Add .goodnotes files or a folder to begin."),
                        ),
                ),
            );
        } else {
            for (index, source) in sources.iter().enumerate() {
                let name = source
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| source.path.display().to_string());
                let path = source.path.display().to_string();
                let icon = if source.is_dir {
                    IconName::Folder
                } else {
                    IconName::FileText
                };
                let remove_id = SharedString::from(format!("remove-source-{index}"));
                let row = h_flex()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .hover(|style| style.bg(cx.theme().list_hover))
                    .child(
                        Icon::new(icon)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(path),
                            ),
                    )
                    .child(
                        Button::new(remove_id)
                            .ghost()
                            .icon(IconName::Close)
                            .xsmall()
                            .tooltip("Remove")
                            .disabled(running)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.remove_source(index);
                                cx.notify();
                            })),
                    );
                list = list.child(row);
            }
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if count == 1 {
                        "1 notebook found".to_string()
                    } else {
                        format!("{count} notebooks found")
                    }),
            );
        }

        GroupBox::new()
            .title(Self::section_title("Sources"))
            .child(list)
            .into_any_element()
    }

    fn render_options(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let output_custom = self.output_mode == 1;

        let formats = h_flex()
            .gap_5()
            .child(
                Checkbox::new("fmt-svg")
                    .label("SVG")
                    .checked(self.fmt_svg)
                    .on_change(cx.listener(|view, value, _, cx| {
                        view.fmt_svg = *value;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("fmt-png")
                    .label("PNG")
                    .checked(self.fmt_png)
                    .on_change(cx.listener(|view, value, _, cx| {
                        view.fmt_png = *value;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("fmt-pdf")
                    .label("PDF")
                    .checked(self.fmt_pdf)
                    .on_change(cx.listener(|view, value, _, cx| {
                        view.fmt_pdf = *value;
                        cx.notify();
                    })),
            );

        let output = v_flex()
            .gap_2()
            .w_full()
            .child(
                RadioGroup::horizontal("output-mode")
                    .children(["Next to each input file", "Choose a folder…"])
                    .selected_index(Some(self.output_mode))
                    .on_change(cx.listener(|view, value, _, cx| {
                        view.output_mode = *value;
                        cx.notify();
                    })),
            )
            .when(output_custom, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .w_full()
                        .child(div().flex_1().min_w_0().child(Input::new(&self.output_dir)))
                        .child(
                            Button::new("browse-output")
                                .outline()
                                .icon(IconName::FolderOpen)
                                .label("Browse…")
                                .disabled(self.running)
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.pick_output(window, cx)),
                                ),
                        ),
                )
            });

        Form::new()
            .label_layout(Axis::Horizontal)
            .child(Field::new().label("Formats").child(formats))
            .child(Field::new().label("Output").child(output))
            .child(
                Field::new().label("DPI").child(
                    div()
                        .w(px(220.))
                        .child(NumberInput::new(&self.dpi).suffix("dpi")),
                ),
            )
            .child(
                Field::new()
                    .label("Page filter")
                    .description("Only pages whose UUID contains this text")
                    .child(div().w(px(320.)).child(Input::new(&self.page))),
            )
            .child(
                Field::new().label_indent(false).child(
                    Switch::new("include-deleted")
                        .label("Include deleted (tombstoned) objects")
                        .checked(self.include_deleted)
                        .on_change(cx.listener(|view, value, _, cx| {
                            view.include_deleted = *value;
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_entry_row(index: usize, entry: &Entry, cx: &Context<Self>) -> AnyElement {
        let name = entry
            .file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.file.display().to_string());

        let (status_element, meta_element): (AnyElement, AnyElement) = match &entry.status {
            Status::Ready => (
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Ready")
                    .into_any_element(),
            ),
            Status::Running {
                preparing,
                done,
                total,
            } => (
                Spinner::new().small().into_any_element(),
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if *total == 0 {
                        "Converting…".to_string()
                    } else if *preparing {
                        format!("preparing {done}/{total}")
                    } else {
                        format!("{done}/{total} page(s)")
                    })
                    .into_any_element(),
            ),
            Status::Done {
                pages,
                written,
                secs,
            } => (
                Icon::new(IconName::CircleCheck)
                    .small()
                    .text_color(cx.theme().success)
                    .into_any_element(),
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{pages} page(s) · {written} output(s) · {secs:.2}s"
                    ))
                    .into_any_element(),
            ),
            Status::Failed(message) => (
                Icon::new(IconName::CircleX)
                    .small()
                    .text_color(cx.theme().danger)
                    .into_any_element(),
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .text_right()
                    .max_w(px(460.))
                    .child(message.clone())
                    .into_any_element(),
            ),
            Status::Cancelled => (
                Icon::new(IconName::Ban)
                    .small()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Cancelled")
                    .into_any_element(),
            ),
        };

        div()
            .flex()
            .items_center()
            .gap_3()
            .py_2()
            .min_w_0()
            .when(index > 0, |this| {
                this.border_t_1().border_color(cx.theme().border)
            })
            .child(status_element)
            .child(
                div()
                    .text_sm()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(name),
            )
            .child(meta_element)
            .into_any_element()
    }

    fn render_results(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.entries.is_empty() {
            return None;
        }
        let entries = self.entries.clone();
        let mut rows = v_flex().gap_0();
        for (index, entry) in entries.iter().enumerate() {
            rows = rows.child(Self::render_entry_row(index, entry, cx));
        }
        Some(
            GroupBox::new()
                .title(Self::section_title("Results"))
                .child(rows)
                .into_any_element(),
        )
    }

    fn render_footer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.running;
        let total = self.entries.len();
        let finished = self
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.status,
                    Status::Done { .. } | Status::Failed(_) | Status::Cancelled
                )
            })
            .count();

        let status: AnyElement = if running {
            let mut page_done = 0usize;
            let mut page_total = 0usize;
            let mut partial = 0f32;
            for entry in &self.entries {
                if let Status::Running { done, total, .. } = &entry.status
                    && *total > 0
                {
                    page_done += *done;
                    page_total += *total;
                    partial += *done as f32 / *total as f32;
                }
            }
            let percent = if total == 0 {
                0.
            } else {
                (finished as f32 + partial) / total as f32 * 100.
            };
            let label = if page_total > 0 {
                format!("{finished} / {total} · page {page_done}/{page_total}")
            } else {
                format!("{finished} / {total}")
            };
            h_flex()
                .gap_3()
                .w_full()
                .child(
                    Progress::new("run-progress")
                        .loading(finished == 0 && page_total == 0)
                        .value(percent)
                        .flex_1(),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .into_any_element()
        } else if let Some(summary) = &self.summary {
            let (text, color) = if summary.failed > 0 {
                (
                    format!(
                        "{} failed · {} converted · {} page(s) · {:.2}s",
                        summary.failed, summary.ok, summary.pages, summary.secs
                    ),
                    cx.theme().danger,
                )
            } else if summary.cancelled > 0 {
                (
                    format!(
                        "Cancelled · {} converted · {} page(s) · {:.2}s",
                        summary.ok, summary.pages, summary.secs
                    ),
                    cx.theme().muted_foreground,
                )
            } else {
                (
                    format!(
                        "Converted {} file(s) · {} page(s) · {} output(s) · {:.2}s",
                        summary.ok, summary.pages, summary.written, summary.secs
                    ),
                    cx.theme().success,
                )
            };
            div()
                .text_sm()
                .text_color(color)
                .child(text)
                .into_any_element()
        } else if self.entries.is_empty() {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Add .goodnotes files to begin")
                .into_any_element()
        } else {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{} notebook(s) ready", self.entries.len()))
                .into_any_element()
        };

        let open_root = self
            .summary
            .as_ref()
            .and_then(|summary| summary.root.clone())
            .or_else(|| {
                if self.output_mode == 1 {
                    let raw = self.output_dir.read(cx).value().to_string();
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(trimmed))
                    }
                } else {
                    None
                }
            });

        let mut actions = h_flex().gap_2();
        if let Some(root) = open_root.filter(|_| !running) {
            actions = actions.child(
                Button::new("open-output")
                    .outline()
                    .icon(IconName::FolderOpen)
                    .label("Open output folder")
                    .on_click(cx.listener(move |_, _, _, _| open_path(&root))),
            );
        }
        if running {
            actions = actions.child(
                Button::new("cancel-run")
                    .outline()
                    .danger()
                    .label(if self.cancelling {
                        "Cancelling…"
                    } else {
                        "Cancel"
                    })
                    .disabled(self.cancelling)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.request_cancel();
                        cx.notify();
                    })),
            );
        }
        actions = actions.child(
            Button::new("convert")
                .primary()
                .icon(IconName::RefreshCw)
                .label(if running { "Converting…" } else { "Convert" })
                .loading(running)
                .disabled(running || self.files.is_empty())
                .on_click(cx.listener(|view, _, window, cx| view.start_run(window, cx))),
        );

        div()
            .flex()
            .items_center()
            .gap_4()
            .px_6()
            .py_4()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().min_w_0().child(status))
            .child(actions)
            .into_any_element()
    }
}

impl Render for ConverterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.render_results(cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(
                                Icon::new(IconName::BookOpen)
                                    .small()
                                    .text_color(cx.theme().primary),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("oblyx"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Convert GoodNotes notebooks to SVG, PDF and PNG"),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if self.files.is_empty() {
                                String::new()
                            } else {
                                format!("{} file(s)", self.files.len())
                            }),
                    ),
            )
            .child(
                div()
                    .id("main-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(920.))
                            .mx_auto()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .px_6()
                            .py_6()
                            .child(self.render_sources(cx))
                            .child(self.render_options(cx))
                            .when_some(results, |this, results| this.child(results)),
                    ),
            )
            .child(self.render_footer(cx))
    }
}

fn open_path(path: &Path) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

pub fn run() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(120.), px(80.)),
                size(px(1080.), px(820.)),
            ))),
            window_min_size: Some(size(px(760.), px(560.))),
            app_id: Some("oblyx".into()),
            ..WindowOptions::default()
        };

        open_window(options, cx, |window, cx| {
            window.set_window_title("oblyx");
            cx.new(|cx| ConverterView::new(window, cx))
        })
        .expect("failed to open window");
    });
}
