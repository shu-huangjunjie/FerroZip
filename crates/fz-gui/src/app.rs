use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use eframe::egui::{self, Align, Color32, Id, Key, Layout, Modifiers, RichText, Sense, Ui, Vec2};
use egui_extras::{Column, TableBuilder};
use fz_core::codepage::Codepage;
use fz_core::util::{archive_stem, human_bytes, unique_path};
use fz_core::*;

use crate::look::{self, glyph, icon};
use crate::settings::Settings;
use fz_shell as shell;
use crate::task::{Job, Outcome, Task};
use crate::tree::Tree;

/// How the process was launched (Explorer context-menu verbs close the
/// window once their work is done).
pub enum StartMode {
    Browse(Option<PathBuf>),
    SmartExtract(Vec<PathBuf>),
    ExtractHere(Vec<PathBuf>),
    ExtractTo(Vec<PathBuf>),
    Compress(Vec<PathBuf>),
    QuickCompress(Vec<PathBuf>, CreateFormat),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Row {
    Dir(usize),
    File(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Packed,
    Size,
    Kind,
    Modified,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterOpen {
    Browse,
    Smart,
    Here,
    Dialog,
}

enum Action {
    Navigate(usize),
    Up,
    Back,
    Activate(Row),
    ExtractSelected { dialog: bool },
    ExtractAll(AfterOpen),
    SelectAll,
    /// Move the keyboard cursor by n rows; `true` extends the selection.
    Move(isize, bool),
    Sort(SortKey),
    OpenDialog,
    OpenPath(PathBuf),
    NewArchive(Vec<PathBuf>),
    Test,
    Settings,
    About,
    CloseArchive,
}

struct CreateDlg {
    inputs: Vec<PathBuf>,
    out: String,
    format: CreateFormat,
    level: u32,
    method: ZipMethod,
    password: String,
    password2: String,
    show_pw: bool,
    encrypt_names: bool,
    threads: usize,
}

struct ExtractDlg {
    dest: String,
    selection: Option<Vec<usize>>,
    strip: String,
    overwrite: Overwrite,
    open_after: bool,
}

struct PasswordDlg {
    job: Option<Job>,
    title: String,
    text: String,
    show: bool,
    wrong: bool,
}

struct SettingsDlg {
    s: Settings,
    shell_installed: bool,
    message: String,
}

enum Dialog {
    Create(CreateDlg),
    Extract(ExtractDlg),
    Password(PasswordDlg),
    Settings(SettingsDlg),
    About,
}

pub struct App {
    settings: Settings,
    archive: Option<Arc<Archive>>,
    tree: Tree,
    cwd: usize,
    history: Vec<usize>,
    rows: Vec<Row>,
    rows_dirty: bool,
    selected: BTreeSet<Row>,
    anchor: Option<usize>,
    cursor: Option<usize>,
    scroll_to: Option<usize>,
    sort: (SortKey, bool),
    search: String,
    task: Option<Task>,
    dialog: Option<Dialog>,
    error: Option<(String, String)>,
    notice: Option<(String, Option<PathBuf>)>,
    /// Launched from a context-menu verb: close when the queue is done.
    quick: bool,
    queue: VecDeque<PathBuf>,
    after_open: AfterOpen,
    temp_dirs: Vec<PathBuf>,
    title: String,
    first_frame: bool,
}

const ROW_H: f32 = 24.0;

fn fmt_time(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return String::new() };
    let d: time::OffsetDateTime = t.into();
    let d = d.to_offset(time::UtcOffset::local_offset_at(d).unwrap_or(time::UtcOffset::UTC));
    format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year(), d.month() as u8, d.day(), d.hour(), d.minute())
}

fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

fn kind_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 8 => format!("{} 文件", ext.to_uppercase()),
        _ => "文件".into(),
    }
}

fn looks_like_archive(p: &Path) -> bool {
    let n = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    p.is_file() && (shell::ARCHIVE_EXTS.iter().any(|e| n.ends_with(e)) || fz_core::read::detect(p).is_ok())
}

/// Default archive path for compressing `inputs`: next to them, named after
/// the single input or after the containing folder.
fn default_output(inputs: &[PathBuf], format: CreateFormat) -> PathBuf {
    let first = inputs.first().cloned().unwrap_or_default();
    let parent = first.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = if inputs.len() == 1 {
        if first.is_dir() {
            first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            first.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        }
    } else {
        parent.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "新建压缩文件".into())
    };
    unique_path(&parent.join(format!("{stem}.{}", format.extension())))
}

fn level_name(level: u32) -> &'static str {
    match level {
        0 => "仅存储",
        1 => "最快",
        2 | 3 => "快速",
        4..=6 => "标准",
        7 | 8 => "高",
        _ => "最大",
    }
}

fn change_extension(path: &str, format: CreateFormat) -> String {
    let lower = path.to_ascii_lowercase();
    for f in CreateFormat::ALL {
        let ext = format!(".{}", f.extension());
        if lower.ends_with(&ext) {
            return format!("{}.{}", &path[..path.len() - ext.len()], format.extension());
        }
    }
    format!("{path}.{}", format.extension())
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, mode: StartMode, settings: Settings) -> Self {
        let mut app = App {
            settings,
            archive: None,
            tree: Tree::default(),
            cwd: 0,
            history: Vec::new(),
            rows: Vec::new(),
            rows_dirty: true,
            selected: BTreeSet::new(),
            anchor: None,
            cursor: None,
            scroll_to: None,
            sort: (SortKey::Name, true),
            search: String::new(),
            task: None,
            dialog: None,
            error: None,
            notice: None,
            quick: false,
            queue: VecDeque::new(),
            after_open: AfterOpen::Browse,
            temp_dirs: Vec::new(),
            title: String::new(),
            first_frame: true,
        };
        let ctx = &cc.egui_ctx;
        match mode {
            StartMode::Browse(Some(p)) => app.open(ctx, p),
            StartMode::Browse(None) => {}
            StartMode::SmartExtract(list)
            | StartMode::ExtractHere(list)
            | StartMode::ExtractTo(list)
            | StartMode::Compress(list)
            | StartMode::QuickCompress(list, _)
                if list.is_empty() => {}
            StartMode::SmartExtract(list) => app.start_queue(ctx, list, AfterOpen::Smart),
            StartMode::ExtractHere(list) => app.start_queue(ctx, list, AfterOpen::Here),
            StartMode::ExtractTo(list) => app.start_queue(ctx, list, AfterOpen::Dialog),
            StartMode::Compress(inputs) => {
                app.quick = true;
                app.dialog = Some(app.create_dialog(inputs));
            }
            StartMode::QuickCompress(inputs, format) => {
                app.quick = true;
                let out = default_output(&inputs, format);
                let opts = app.create_options(format, None);
                app.start(ctx, format!("正在压缩到 {}", out.file_name().unwrap_or_default().to_string_lossy()), Job::Create {
                    out,
                    inputs,
                    opts,
                });
            }
        }
        app
    }

    fn create_options(&self, format: CreateFormat, password: Option<String>) -> CreateOptions {
        CreateOptions {
            format,
            level: self.settings.level,
            threads: self.settings.threads,
            password,
            zip_method: self.settings.zip_method,
            encrypt_names: true,
            store_precompressed: self.settings.store_precompressed,
        }
    }

    fn create_dialog(&self, inputs: Vec<PathBuf>) -> Dialog {
        let format = self.settings.format;
        Dialog::Create(CreateDlg {
            out: default_output(&inputs, format).display().to_string(),
            inputs,
            format,
            level: self.settings.level,
            method: self.settings.zip_method,
            password: String::new(),
            password2: String::new(),
            show_pw: false,
            encrypt_names: true,
            threads: self.settings.threads,
        })
    }

    fn start_queue(&mut self, ctx: &egui::Context, list: Vec<PathBuf>, how: AfterOpen) {
        self.quick = true;
        self.queue = list.into();
        self.after_open = how;
        self.next_in_queue(ctx);
    }

    fn next_in_queue(&mut self, ctx: &egui::Context) {
        match self.queue.pop_front() {
            Some(p) => {
                let how = self.after_open;
                self.open(ctx, p);
                self.after_open = how;
            }
            None if self.quick && self.error.is_none() && self.dialog.is_none() => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            None => {}
        }
    }

    fn start(&mut self, ctx: &egui::Context, title: impl Into<String>, job: Job) {
        self.task = Some(Task::spawn(ctx, title, job));
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.after_open = AfterOpen::Browse;
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let opts = OpenOptions { password: None, codepage: self.settings.codepage };
        self.start(ctx, format!("正在打开 {name}"), Job::Open { path, opts });
    }

    fn set_archive(&mut self, ctx: &egui::Context, archive: Archive) {
        self.settings.push_recent(&archive.path);
        self.tree = Tree::build(&archive.entries);
        self.title = format!("{} - FerroZip", archive.path.file_name().unwrap_or_default().to_string_lossy());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.title.clone()));
        self.archive = Some(Arc::new(archive));
        self.cwd = 0;
        self.history.clear();
        self.selected.clear();
        self.anchor = None;
        self.search.clear();
        self.rows_dirty = true;
        // A single top-level folder: show its content right away, like Bandizip.
        if self.tree.nodes[0].files.is_empty() && self.tree.nodes[0].dirs.len() == 1 {
            self.cwd = self.tree.nodes[0].dirs[0];
        }
    }

    fn close_archive(&mut self, ctx: &egui::Context) {
        self.archive = None;
        self.tree = Tree::default();
        self.rows.clear();
        self.selected.clear();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("FerroZip".into()));
    }

    fn archive_parent(&self) -> PathBuf {
        self.archive
            .as_ref()
            .and_then(|a| std::path::absolute(&a.path).ok())
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_default()
    }

    fn extract_job(&mut self, ctx: &egui::Context, dest: PathBuf, selection: Option<Vec<usize>>, strip: String, overwrite: Overwrite, open_after: bool) {
        let Some(archive) = self.archive.clone() else { return };
        let opts = ExtractOptions {
            dest: dest.clone(),
            selection,
            strip_prefix: strip,
            overwrite,
            password: archive.password.clone(),
            threads: self.settings.threads,
        };
        let then_open = open_after.then(|| dest.clone());
        let name = archive.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.start(ctx, format!("正在解压 {name}"), Job::Extract { archive, opts, then_open });
    }

    fn selection_indices(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for r in &self.selected {
            match *r {
                Row::Dir(n) => self.tree.collect(n, &mut out),
                Row::File(i) => out.push(i),
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    fn view_entry(&mut self, ctx: &egui::Context, index: usize) {
        let Some(archive) = self.archive.clone() else { return };
        let dir = std::env::temp_dir().join("FerroZip").join(format!("view-{}-{}", std::process::id(), self.temp_dirs.len()));
        self.temp_dirs.push(dir.clone());
        let name = archive.entries[index].name.clone();
        let strip = match name.rfind('/') {
            Some(p) => name[..=p].to_string(),
            None => String::new(),
        };
        let file = dir.join(fz_core::util::sanitize_entry_path(&name[strip.len()..]).unwrap_or_else(|| "file".into()));
        let opts = ExtractOptions {
            dest: dir,
            selection: Some(vec![index]),
            strip_prefix: strip,
            overwrite: Overwrite::Always,
            password: archive.password.clone(),
            threads: 0,
        };
        self.start(ctx, format!("正在打开 {}", name.rsplit('/').next().unwrap_or("")), Job::Extract {
            archive,
            opts,
            then_open: Some(file),
        });
    }

    fn rebuild_rows(&mut self) {
        let Some(archive) = &self.archive else {
            self.rows.clear();
            return;
        };
        let entries = &archive.entries;
        let mut rows: Vec<Row> = if self.search.trim().is_empty() {
            let n = &self.tree.nodes[self.cwd];
            n.dirs.iter().map(|&d| Row::Dir(d)).chain(n.files.iter().map(|&f| Row::File(f))).collect()
        } else {
            let q = self.search.trim().to_lowercase();
            let mut v: Vec<Row> = (1..self.tree.nodes.len())
                .filter(|&d| self.tree.nodes[d].name.to_lowercase().contains(&q))
                .map(Row::Dir)
                .collect();
            v.extend(
                entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| !e.is_dir && e.name.rsplit('/').next().unwrap_or("").to_lowercase().contains(&q))
                    .map(|(i, _)| Row::File(i)),
            );
            v
        };
        let tree = &self.tree;
        let name_of = |r: &Row| match *r {
            Row::Dir(d) => tree.nodes[d].name.to_lowercase(),
            Row::File(f) => entries[f].name.rsplit('/').next().unwrap_or("").to_lowercase(),
        };
        let (key, asc) = self.sort;
        rows.sort_by(|a, b| {
            let dir_first = matches!(b, Row::Dir(_)).cmp(&matches!(a, Row::Dir(_)));
            let ord = match key {
                SortKey::Name => name_of(a).cmp(&name_of(b)),
                SortKey::Size | SortKey::Packed => {
                    let v = |r: &Row| match *r {
                        Row::Dir(d) => if key == SortKey::Size { tree.nodes[d].size } else { tree.nodes[d].packed },
                        Row::File(f) => if key == SortKey::Size { entries[f].size } else { entries[f].packed.unwrap_or(0) },
                    };
                    v(a).cmp(&v(b))
                }
                SortKey::Kind => {
                    let k = |r: &Row| match *r {
                        Row::Dir(_) => String::new(),
                        Row::File(f) => kind_of(&entries[f].name),
                    };
                    k(a).cmp(&k(b)).then_with(|| name_of(a).cmp(&name_of(b)))
                }
                SortKey::Modified => {
                    let t = |r: &Row| match *r {
                        Row::Dir(d) => tree.nodes[d].mtime,
                        Row::File(f) => entries[f].mtime,
                    };
                    t(a).cmp(&t(b))
                }
            };
            dir_first.then(if asc { ord } else { ord.reverse() })
        });
        self.rows = rows;
        self.selected.retain(|r| self.rows.contains(r));
        self.rows_dirty = false;
    }

    // ---------------------------------------------------------------- tasks

    fn poll_task(&mut self, ctx: &egui::Context) {
        let Some(task) = &mut self.task else { return };
        let Some(result) = task.poll() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        };
        let task = self.task.take().expect("task present");
        let elapsed = task.started.elapsed().as_secs_f64();
        match result {
            Ok(Outcome::Opened(archive)) => {
                let how = self.after_open;
                self.set_archive(ctx, archive);
                match how {
                    AfterOpen::Browse => {}
                    AfterOpen::Smart => {
                        let a = self.archive.clone().expect("just opened");
                        let dest = smart_destination(&a, &self.archive_parent());
                        let ow = self.settings.overwrite;
                        self.extract_job(ctx, dest, None, String::new(), ow, self.settings.open_folder_after_extract);
                    }
                    AfterOpen::Here => {
                        let dest = self.archive_parent();
                        let ow = self.settings.overwrite;
                        self.extract_job(ctx, dest, None, String::new(), ow, self.settings.open_folder_after_extract);
                    }
                    AfterOpen::Dialog => self.open_extract_dialog(None, String::new()),
                }
            }
            Ok(Outcome::Extracted { dest, summary, then_open }) => {
                let skipped = if summary.skipped > 0 { format!("，跳过 {} 个", summary.skipped) } else { String::new() };
                let folder = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dest.display().to_string());
                self.notice = Some((format!("解压完成：{} 个文件{skipped}，用时 {elapsed:.1} 秒 → {folder}", summary.files), Some(dest)));
                if let Some(p) = then_open {
                    shell::open_path(&p);
                }
                if self.quick {
                    self.next_in_queue(ctx);
                } else if self.settings.close_after_extract {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Ok(Outcome::Created { out }) => {
                let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                let input = task.progress.snapshot().total_bytes;
                let ratio = if input > 0 { size as f64 * 100.0 / input as f64 } else { 0.0 };
                self.notice = Some((
                    format!("压缩完成：{} → {}（{ratio:.1}%），用时 {elapsed:.1} 秒，{}/s", human_bytes(input), human_bytes(size), human_bytes((input as f64 / elapsed.max(0.001)) as u64)),
                    Some(out.clone()),
                ));
                if self.quick {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    self.open(ctx, out);
                }
            }
            Ok(Outcome::Tested(s)) => {
                self.error = Some(("测试完成".into(), format!("没有发现错误。\n\n已检查 {} 个文件，用时 {elapsed:.1} 秒。", s.files.max(self.archive.as_ref().map_or(0, |a| a.entries.iter().filter(|e| !e.is_dir).count() as u64)))));
            }
            Err(e) if e.is_password_problem() => {
                let wrong = matches!(e, Error::WrongPassword);
                self.dialog = Some(Dialog::Password(PasswordDlg {
                    job: task.retry,
                    title: task.title.clone(),
                    text: String::new(),
                    show: false,
                    wrong,
                }));
            }
            Err(Error::Cancelled) => {
                self.notice = Some(("操作已取消".into(), None));
                if self.quick {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Err(e) => {
                self.error = Some((task.title.clone(), e.to_string()));
                self.queue.clear();
            }
        }
    }

    fn open_extract_dialog(&mut self, selection: Option<Vec<usize>>, strip: String) {
        let Some(a) = &self.archive else { return };
        let dest = smart_destination(a, &self.archive_parent());
        self.dialog = Some(Dialog::Extract(ExtractDlg {
            dest: dest.display().to_string(),
            selection,
            strip,
            overwrite: self.settings.overwrite,
            open_after: self.settings.open_folder_after_extract,
        }));
    }

    // -------------------------------------------------------------- actions

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Navigate(n) => {
                if n != self.cwd {
                    self.history.push(self.cwd);
                    self.cwd = n;
                }
                self.search.clear();
                self.selected.clear();
                self.anchor = None;
                self.rows_dirty = true;
            }
            Action::Up => {
                if let Some(p) = self.tree.nodes.get(self.cwd).and_then(|n| n.parent) {
                    self.apply(ctx, Action::Navigate(p));
                }
            }
            Action::Back => {
                if let Some(n) = self.history.pop() {
                    self.cwd = n;
                    self.selected.clear();
                    self.rows_dirty = true;
                }
            }
            Action::Activate(Row::Dir(n)) => self.apply(ctx, Action::Navigate(n)),
            Action::Activate(Row::File(i)) => self.view_entry(ctx, i),
            Action::ExtractSelected { dialog } => {
                let strip = if self.search.is_empty() { self.tree.prefix(self.cwd) } else { String::new() };
                let sel = if self.selected.is_empty() { None } else { Some(self.selection_indices()) };
                if dialog {
                    self.open_extract_dialog(sel, strip);
                } else {
                    let dest = self.archive_parent();
                    let ow = self.settings.overwrite;
                    let open = self.settings.open_folder_after_extract;
                    self.extract_job(ctx, dest, sel, strip, ow, open);
                }
            }
            Action::ExtractAll(how) => {
                let Some(a) = self.archive.clone() else { return };
                let ow = self.settings.overwrite;
                let open = self.settings.open_folder_after_extract;
                match how {
                    AfterOpen::Smart => {
                        let dest = smart_destination(&a, &self.archive_parent());
                        self.extract_job(ctx, dest, None, String::new(), ow, open);
                    }
                    AfterOpen::Here => {
                        let dest = self.archive_parent();
                        self.extract_job(ctx, dest, None, String::new(), ow, open);
                    }
                    AfterOpen::Browse => {
                        let dest = unique_path(&self.archive_parent().join(archive_stem(&a.path)));
                        self.extract_job(ctx, dest, None, String::new(), ow, open);
                    }
                    AfterOpen::Dialog => self.open_extract_dialog(None, String::new()),
                }
            }
            Action::SelectAll => {
                self.selected = self.rows.iter().copied().collect();
            }
            Action::Move(delta, extend) => {
                if self.rows.is_empty() {
                    return;
                }
                let last = self.rows.len() - 1;
                let next = match self.cursor {
                    Some(c) => (c as isize + delta).clamp(0, last as isize) as usize,
                    None => 0,
                };
                if extend {
                    let a = self.anchor.unwrap_or(next);
                    let (lo, hi) = if a <= next { (a, next) } else { (next, a) };
                    self.selected = self.rows[lo..=hi].iter().copied().collect();
                } else {
                    self.selected.clear();
                    self.selected.insert(self.rows[next]);
                    self.anchor = Some(next);
                }
                self.cursor = Some(next);
                self.scroll_to = Some(next);
            }
            Action::Sort(k) => {
                self.sort = if self.sort.0 == k { (k, !self.sort.1) } else { (k, true) };
                self.rows_dirty = true;
            }
            Action::OpenDialog => {
                let mut dlg = rfd::FileDialog::new().set_title("打开压缩文件").add_filter(
                    "压缩文件",
                    &["zip", "7z", "rar", "tar", "gz", "tgz", "zst", "tzst", "xz", "txz", "bz2", "tbz2", "zipx", "001", "exe"],
                );
                dlg = dlg.add_filter("所有文件", &["*"]);
                if let Some(p) = dlg.pick_file() {
                    self.open(ctx, p);
                }
            }
            Action::OpenPath(p) => self.open(ctx, p),
            Action::NewArchive(inputs) => {
                let inputs = if inputs.is_empty() {
                    match rfd::FileDialog::new().set_title("选择要压缩的文件").pick_files() {
                        Some(f) => f,
                        None => return,
                    }
                } else {
                    inputs
                };
                self.dialog = Some(self.create_dialog(inputs));
            }
            Action::Test => {
                if let Some(a) = self.archive.clone() {
                    let password = a.password.clone();
                    self.start(ctx, "正在测试压缩文件", Job::Test { archive: a, password });
                }
            }
            Action::Settings => {
                self.dialog = Some(Dialog::Settings(SettingsDlg {
                    s: self.settings.clone(),
                    shell_installed: shell::is_installed(),
                    message: String::new(),
                }));
            }
            Action::About => self.dialog = Some(Dialog::About),
            Action::CloseArchive => self.close_archive(ctx),
        }
    }

    // ------------------------------------------------------------------- UI

    fn tool_button(ui: &mut Ui, code: &str, label: &str, enabled: bool) -> egui::Response {
        let size = Vec2::new(64.0, 58.0);
        let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
        let painter = ui.painter_at(rect);
        if enabled && (resp.hovered() || resp.has_focus()) {
            let fill = if resp.is_pointer_button_down_on() { Color32::from_rgb(0xD5, 0xE5, 0xFC) } else { Color32::from_rgb(0xE8, 0xF0, 0xFD) };
            painter.rect_filled(rect.shrink(2.0), 6.0, fill);
        }
        let color = if enabled { Color32::from_rgb(0x25, 0x63, 0xEB) } else { Color32::from_gray(0xB0) };
        let text_color = if enabled { Color32::from_gray(0x30) } else { Color32::from_gray(0xB0) };
        painter.text(rect.center_top() + Vec2::new(0.0, 19.0), egui::Align2::CENTER_CENTER, code, egui::FontId::new(24.0, look::icons_family()), color);
        painter.text(rect.center_bottom() - Vec2::new(0.0, 12.0), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(13.0), text_color);
        resp
    }

    fn toolbar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let has = self.archive.is_some();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if Self::tool_button(ui, icon::OPEN, "打开", true).on_hover_text("打开压缩文件 (Ctrl+O)").clicked() {
                actions.push(Action::OpenDialog);
            }
            let ex = Self::tool_button(ui, icon::EXTRACT, "解压", has).on_hover_text("解压到… (Ctrl+E)");
            if ex.clicked() {
                actions.push(Action::ExtractSelected { dialog: true });
            }
            ex.context_menu(|ui| {
                if ui.button("解压到…").clicked() {
                    actions.push(Action::ExtractAll(AfterOpen::Dialog));
                }
                if ui.button("智能解压").clicked() {
                    actions.push(Action::ExtractAll(AfterOpen::Smart));
                }
                if ui.button("解压到当前文件夹").clicked() {
                    actions.push(Action::ExtractAll(AfterOpen::Here));
                }
                if let Some(a) = &self.archive {
                    if ui.button(format!("解压到 \"{}\\\"", archive_stem(&a.path))).clicked() {
                        actions.push(Action::ExtractAll(AfterOpen::Browse));
                    }
                }
            });
            if Self::tool_button(ui, icon::NEW, "新建", true).on_hover_text("新建压缩文件 (Ctrl+N)").clicked() {
                actions.push(Action::NewArchive(Vec::new()));
            }
            if Self::tool_button(ui, icon::TEST, "测试", has).on_hover_text("测试压缩文件完整性").clicked() {
                actions.push(Action::Test);
            }
            let can_view = self.selected.iter().any(|r| matches!(r, Row::File(_)));
            if Self::tool_button(ui, icon::VIEW, "查看", has && can_view).on_hover_text("用默认程序打开选中的文件").clicked() {
                if let Some(r) = self.selected.iter().find(|r| matches!(r, Row::File(_))) {
                    actions.push(Action::Activate(*r));
                }
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            if Self::tool_button(ui, icon::SETTINGS, "设置", true).clicked() {
                actions.push(Action::Settings);
            }
            if Self::tool_button(ui, icon::INFO, "关于", true).clicked() {
                actions.push(Action::About);
            }
            if has {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(glyph(icon::CLOSE, 12.0)).on_hover_text("关闭压缩文件").clicked() {
                        actions.push(Action::CloseArchive);
                    }
                });
            }
        });
    }

    fn address_bar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.history.is_empty(), egui::Button::new(glyph(icon::BACK, 14.0))).on_hover_text("后退").clicked() {
                actions.push(Action::Back);
            }
            if ui.add_enabled(self.cwd != 0, egui::Button::new(glyph(icon::UP, 14.0))).on_hover_text("上一级 (Backspace)").clicked() {
                actions.push(Action::Up);
            }
            let search_w = 220.0;
            let avail = ui.available_width() - search_w - 16.0;
            egui::Frame::new()
                .fill(Color32::WHITE)
                .stroke(egui::Stroke::new(1.0, Color32::from_rgb(0xD9, 0xDD, 0xE3)))
                .corner_radius(5.0)
                .inner_margin(egui::Margin::symmetric(8, 3))
                .show(ui, |ui| {
                    ui.set_width(avail.max(100.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let mut chain = vec![self.cwd];
                        while let Some(p) = self.tree.nodes[*chain.last().unwrap()].parent {
                            chain.push(p);
                        }
                        chain.reverse();
                        let root_name = self
                            .archive
                            .as_ref()
                            .map(|a| a.path.file_name().unwrap_or_default().to_string_lossy().into_owned())
                            .unwrap_or_default();
                        for (k, &n) in chain.iter().enumerate() {
                            if k > 0 {
                                ui.label(RichText::new("›").color(look::MUTED));
                            }
                            let label = if n == 0 { root_name.clone() } else { self.tree.nodes[n].name.clone() };
                            if ui.add(egui::Button::new(label).frame(false)).clicked() {
                                actions.push(Action::Navigate(n));
                            }
                        }
                    });
                });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("搜索文件名").desired_width(search_w - 30.0));
                if r.changed() {
                    self.rows_dirty = true;
                }
                if ui.input(|i| i.modifiers.command && i.key_pressed(Key::F)) {
                    r.request_focus();
                }
                ui.label(glyph(icon::SEARCH, 14.0).color(look::MUTED));
            });
        });
    }

    fn tree_ui(&self, ui: &mut Ui, node: usize, actions: &mut Vec<Action>) {
        let n = &self.tree.nodes[node];
        let name = if node == 0 {
            self.archive.as_ref().map(|a| a.path.file_name().unwrap_or_default().to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            n.name.clone()
        };
        let selected = node == self.cwd;
        let mut label = |ui: &mut Ui| {
            let mut job = egui::text::LayoutJob::default();
            let fmt = |font_id, color| egui::TextFormat { font_id, color, valign: Align::Center, ..Default::default() };
            job.append(
                if node == 0 { icon::ZIP } else { icon::FOLDER },
                0.0,
                fmt(egui::FontId::new(14.0, look::icons_family()), if node == 0 { look::ACCENT } else { look::FOLDER }),
            );
            job.append(&name, 6.0, fmt(egui::FontId::proportional(14.0), ui.visuals().text_color()));
            let r = ui.add(egui::Button::selectable(selected, job).frame_when_inactive(false));
            if r.clicked() {
                actions.push(Action::Navigate(node));
            }
        };
        if n.dirs.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                label(ui);
            });
        } else {
            let id = Id::unique(("tree", node));
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, node == 0);
            // Keep the path to the current folder expanded.
            let mut p = Some(self.cwd);
            while let Some(x) = p {
                if self.tree.nodes[x].parent == Some(node) || (x == node && node != self.cwd) {
                    state.set_open(true);
                    break;
                }
                p = self.tree.nodes[x].parent;
            }
            state
                .show_header(ui, |ui| label(ui))
                .body(|ui| {
                    for &d in &n.dirs {
                        self.tree_ui(ui, d, actions);
                    }
                });
        }
    }

    fn file_table(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some(archive) = self.archive.clone() else { return };
        let entries = &archive.entries;
        let modifiers: Modifiers = ui.input(|i| i.modifiers);
        let mut click: Option<(usize, Row)> = None;
        let mut double: Option<Row> = None;
        let mut context: Option<(usize, Row)> = None;
        let rows = self.rows.clone();
        let search_mode = !self.search.trim().is_empty();
        let sort = self.sort;
        let header_cell = |ui: &mut Ui, label: &str, key: SortKey, actions: &mut Vec<Action>| {
            let arrow = if sort.0 == key { if sort.1 { " ▲" } else { " ▼" } } else { "" };
            let r = ui.add(egui::Label::new(RichText::new(format!("{label}{arrow}")).strong()).sense(Sense::click()));
            if r.clicked() {
                actions.push(Action::Sort(key));
            }
        };
        let text_h = ROW_H;
        let mut builder = TableBuilder::new(ui);
        if let Some(r) = self.scroll_to.take() {
            builder = builder.scroll_to_row(r, None);
        }
        builder
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(220.0).clip(true))
            .column(Column::initial(100.0).at_least(60.0))
            .column(Column::initial(100.0).at_least(60.0))
            .column(Column::initial(90.0).at_least(50.0))
            .column(Column::initial(132.0).at_least(80.0))
            .column(Column::initial(78.0).at_least(50.0))
            .header(26.0, |mut h| {
                h.col(|ui| header_cell(ui, "名称", SortKey::Name, actions));
                h.col(|ui| header_cell(ui, "压缩后大小", SortKey::Packed, actions));
                h.col(|ui| header_cell(ui, "原始大小", SortKey::Size, actions));
                h.col(|ui| header_cell(ui, "类型", SortKey::Kind, actions));
                h.col(|ui| header_cell(ui, "修改时间", SortKey::Modified, actions));
                h.col(|ui| {
                    ui.label(RichText::new("CRC32").strong());
                });
            })
            .body(|body| {
                body.rows(text_h, rows.len(), |mut row| {
                    let i = row.index();
                    let r = rows[i];
                    row.set_selected(self.selected.contains(&r));
                    match r {
                        Row::Dir(d) => {
                            let n = &self.tree.nodes[d];
                            row.col(|ui| {
                                ui.label(glyph(icon::FOLDER, 15.0).color(look::FOLDER));
                                ui.label(if search_mode { n.path.clone() } else { n.name.clone() });
                            });
                            row.col(|ui| {
                                if n.packed > 0 {
                                    ui.label(human_bytes(n.packed));
                                }
                            });
                            row.col(|ui| {
                                ui.label(human_bytes(n.size));
                            });
                            row.col(|ui| {
                                ui.label("文件夹");
                            });
                            row.col(|ui| {
                                ui.label(fmt_time(n.mtime));
                            });
                            row.col(|_| {});
                        }
                        Row::File(f) => {
                            let e = &entries[f];
                            let name = if search_mode { e.name.as_str() } else { e.name.rsplit('/').next().unwrap_or(&e.name) };
                            row.col(|ui| {
                                ui.label(glyph(icon::FILE, 15.0).color(Color32::from_rgb(0x7A, 0x86, 0x99)));
                                ui.label(name);
                                if e.encrypted {
                                    ui.label(glyph(icon::LOCK, 11.0).color(look::FOLDER)).on_hover_text("已加密");
                                }
                            });
                            row.col(|ui| {
                                if let Some(p) = e.packed {
                                    ui.label(human_bytes(p));
                                }
                            });
                            row.col(|ui| {
                                ui.label(human_bytes(e.size));
                            });
                            row.col(|ui| {
                                ui.label(kind_of(&e.name));
                            });
                            row.col(|ui| {
                                ui.label(fmt_time(e.mtime));
                            });
                            row.col(|ui| {
                                if let Some(c) = e.crc {
                                    ui.label(RichText::new(format!("{c:08X}")).monospace().color(look::MUTED));
                                }
                            });
                        }
                    }
                    let resp = row.response();
                    if resp.double_clicked() {
                        double = Some(r);
                    } else if resp.clicked() {
                        click = Some((i, r));
                    }
                    if resp.secondary_clicked() {
                        context = Some((i, r));
                    }
                    resp.context_menu(|ui| {
                        let file = matches!(r, Row::File(_));
                        if ui.button(if file { "打开" } else { "进入" }).clicked() {
                            actions.push(Action::Activate(r));
                        }
                        ui.separator();
                        if ui.button("解压到…").clicked() {
                            actions.push(Action::ExtractSelected { dialog: true });
                        }
                        if ui.button("解压到当前文件夹").clicked() {
                            actions.push(Action::ExtractSelected { dialog: false });
                        }
                        ui.separator();
                        if ui.button("全选").clicked() {
                            actions.push(Action::SelectAll);
                        }
                        if ui.button("复制文件名").clicked() {
                            let name = match r {
                                Row::Dir(d) => self.tree.nodes[d].name.clone(),
                                Row::File(f) => entries[f].name.rsplit('/').next().unwrap_or("").to_string(),
                            };
                            ui.ctx().copy_text(name);
                        }
                    });
                });
            });

        if let Some(r) = double {
            actions.push(Action::Activate(r));
        }
        if let Some((i, r)) = context {
            if !self.selected.contains(&r) {
                self.selected.clear();
                self.selected.insert(r);
                self.anchor = Some(i);
            }
        }
        if let Some((i, _)) = click.or(context) {
            self.cursor = Some(i);
        }
        if let Some((i, r)) = click {
            if modifiers.command {
                if !self.selected.remove(&r) {
                    self.selected.insert(r);
                }
                self.anchor = Some(i);
            } else if modifiers.shift {
                let a = self.anchor.unwrap_or(i);
                let (lo, hi) = if a <= i { (a, i) } else { (i, a) };
                self.selected = self.rows[lo..=hi].iter().copied().collect();
            } else {
                self.selected.clear();
                self.selected.insert(r);
                self.anchor = Some(i);
            }
        }
    }

    fn status_bar(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            if let Some((text, path)) = &self.notice {
                let r = ui.label(RichText::new(text).color(Color32::from_rgb(0x16, 0x7C, 0x3A)));
                if let Some(p) = path {
                    r.on_hover_text(p.display().to_string());
                    if ui.link("打开位置").clicked() {
                        shell::reveal(p);
                    }
                }
                ui.separator();
            }
            if let Some(a) = &self.archive {
                let dirs = self.rows.iter().filter(|r| matches!(r, Row::Dir(_))).count();
                let files = self.rows.len() - dirs;
                ui.label(format!("{dirs} 个文件夹，{files} 个文件"));
                if !self.selected.is_empty() {
                    let size: u64 = self
                        .selected
                        .iter()
                        .map(|r| match *r {
                            Row::Dir(d) => self.tree.nodes[d].size,
                            Row::File(f) => a.entries[f].size,
                        })
                        .sum();
                    ui.label(format!("已选择 {} 项（{}）", self.selected.len(), human_bytes(size)));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let total = a.total_size();
                    let ratio = if total > 0 { a.archive_size as f64 * 100.0 / total as f64 } else { 100.0 };
                    let mut info = format!(
                        "{}{} · 共 {} 个文件 · {} → {}（{ratio:.1}%）",
                        a.kind.label(),
                        if a.solid { " 固实" } else { "" },
                        self.tree.nodes[0].file_count,
                        human_bytes(total),
                        human_bytes(a.archive_size)
                    );
                    if a.has_encrypted() {
                        info.push_str(" · 已加密");
                    }
                    ui.label(RichText::new(info).color(look::MUTED));
                });
            } else if self.notice.is_none() {
                ui.label(RichText::new(format!("就绪 · {} 线程", fz_core::default_threads())).color(look::MUTED));
            }
        });
    }

    fn welcome(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.16).max(20.0));
            ui.label(glyph(icon::ZIP, 64.0).color(look::ACCENT));
            ui.add_space(6.0);
            ui.label(RichText::new("FerroZip").size(30.0).strong());
            ui.label(RichText::new("开源 · 无广告 · 多核极速压缩").size(15.0).color(look::MUTED));
            ui.add_space(26.0);
            ui.horizontal(|ui| {
                let w = 2.0 * 190.0 + 16.0;
                ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                let big = |ui: &mut Ui, code: &str, title: &str, sub: &str| {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(190.0, 96.0), Sense::click());
                    let p = ui.painter_at(rect);
                    let hovered = resp.hovered();
                    p.rect(
                        rect,
                        10.0,
                        if hovered { Color32::from_rgb(0xEE, 0xF4, 0xFF) } else { Color32::WHITE },
                        egui::Stroke::new(1.0, if hovered { look::ACCENT } else { Color32::from_rgb(0xDD, 0xE2, 0xE8) }),
                        egui::StrokeKind::Inside,
                    );
                    p.text(rect.left_center() + Vec2::new(36.0, 0.0), egui::Align2::CENTER_CENTER, code, egui::FontId::new(30.0, look::icons_family()), look::ACCENT);
                    p.text(rect.left_center() + Vec2::new(66.0, -11.0), egui::Align2::LEFT_CENTER, title, egui::FontId::proportional(16.0), Color32::from_gray(0x20));
                    p.text(rect.left_center() + Vec2::new(66.0, 13.0), egui::Align2::LEFT_CENTER, sub, egui::FontId::proportional(12.0), look::MUTED);
                    resp
                };
                if big(ui, icon::OPEN, "打开压缩文件", "ZIP 7Z RAR TAR GZ XZ…").clicked() {
                    actions.push(Action::OpenDialog);
                }
                ui.add_space(16.0);
                if big(ui, icon::NEW, "新建压缩文件", "ZIP / 7Z / TAR.ZST…").clicked() {
                    actions.push(Action::NewArchive(Vec::new()));
                }
            });
            ui.add_space(18.0);
            ui.label(RichText::new("也可以把文件或压缩文件直接拖到这个窗口").color(look::MUTED));
            if !self.settings.recent.is_empty() {
                ui.add_space(26.0);
                ui.label(RichText::new("最近打开").strong());
                ui.add_space(4.0);
                for p in self.settings.recent.clone().iter().take(6) {
                    if p.exists() && ui.link(p.display().to_string()).clicked() {
                        actions.push(Action::OpenPath(p.clone()));
                    }
                }
            }
        });
    }

    fn task_window(&self, ui: &mut Ui) {
        let Some(task) = &self.task else { return };
        let p = task.progress.snapshot();
        let elapsed = task.started.elapsed().as_secs_f64();
        ui.set_width(440.0);
        ui.label(RichText::new(&task.title).size(16.0).strong());
        ui.add_space(4.0);
        let current = if p.current.chars().count() > 60 {
            format!("…{}", p.current.chars().rev().take(58).collect::<Vec<_>>().into_iter().rev().collect::<String>())
        } else {
            p.current.clone()
        };
        ui.label(RichText::new(current).color(look::MUTED).size(12.0));
        ui.add_space(6.0);
        let frac = if p.total_bytes > 0 { (p.done_bytes as f64 / p.total_bytes as f64).min(1.0) as f32 } else { 0.0 };
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 10.0, Color32::from_rgb(0xE6, 0xEA, 0xF0));
        if frac > 0.0 {
            let mut fill = rect;
            fill.set_width((rect.width() * frac).max(20.0));
            painter.rect_filled(fill, 10.0, look::ACCENT);
        }
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, format!("{:.1}%", frac * 100.0), egui::FontId::proportional(12.0), if frac > 0.5 { Color32::WHITE } else { Color32::from_gray(0x30) });
        ui.add_space(4.0);
        let speed = p.done_bytes as f64 / elapsed.max(0.001);
        let remaining = if frac > 0.01 { elapsed * (1.0 - frac as f64) / frac as f64 } else { 0.0 };
        egui::Grid::new("task_stats").num_columns(4).spacing([18.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("已处理").color(look::MUTED));
            ui.label(format!("{} / {}", human_bytes(p.done_bytes), human_bytes(p.total_bytes)));
            ui.label(RichText::new("速度").color(look::MUTED));
            ui.label(format!("{}/s", human_bytes(speed as u64)));
            ui.end_row();
            ui.label(RichText::new("文件").color(look::MUTED));
            ui.label(format!("{} / {}", p.done_files, p.total_files));
            ui.label(RichText::new("时间").color(look::MUTED));
            ui.label(format!("{} / 剩余 {}", fmt_secs(elapsed), if frac > 0.01 { fmt_secs(remaining) } else { "--:--".into() }));
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let cancelling = task.progress.is_cancelled();
            if ui.add_enabled(!cancelling, egui::Button::new(if cancelling { "正在取消…" } else { "取消" })).clicked() {
                task.progress.cancel();
            }
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        match &mut dialog {
            Dialog::Create(d) => keep = self.create_ui(ctx, d),
            Dialog::Extract(d) => keep = self.extract_ui(ctx, d),
            Dialog::Password(d) => keep = self.password_ui(ctx, d),
            Dialog::Settings(d) => keep = self.settings_ui(ctx, d),
            Dialog::About => {
                let r = egui::Modal::new(Id::unique("about")).show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.horizontal(|ui| {
                        ui.label(glyph(icon::ZIP, 40.0).color(look::ACCENT));
                        ui.vertical(|ui| {
                            ui.label(RichText::new("FerroZip").size(22.0).strong());
                            ui.label(format!("版本 {} · MIT 许可证", env!("CARGO_PKG_VERSION")));
                        });
                    });
                    ui.add_space(8.0);
                    ui.label("开源、无广告的压缩软件，界面参考 Bandizip。");
                    ui.label("ZIP 用 libdeflate 分块并行压缩（类似 pigz），单个大文件也能用满所有核心；7Z 用自研写入器：并行固实块按完成顺序写出，大文件分片并行 LZMA2。");
                    ui.add_space(6.0);
                    ui.label(RichText::new("组件：libdeflate · zlib-rs · zstd · liblzma · lzma-rust2 · sevenz-rust2 · zip · unRAR · egui").color(look::MUTED).size(12.0));
                    ui.add_space(10.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.button("确定").clicked()).inner
                });
                if r.inner || r.should_close() {
                    keep = false;
                }
            }
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        } else if !keep && self.quick && self.task.is_none() && self.dialog.is_none() && !matches!(dialog, Dialog::Password(_)) {
            // Context-menu "compress to…" / "extract to…" cancelled.
            if self.queue.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn create_ui(&mut self, ctx: &egui::Context, d: &mut CreateDlg) -> bool {
        let mut keep = true;
        let mut go = false;
        let r = egui::Modal::new(Id::unique("create")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(RichText::new("新建压缩文件").size(18.0).strong());
            ui.add_space(6.0);
            ui.add_space(4.0);
            egui::Grid::new("create_grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                ui.label("源文件");
                let what = match d.inputs.len() {
                    1 => d.inputs[0].display().to_string(),
                    n => format!("{} 等 {n} 项", d.inputs[0].display()),
                };
                ui.add(egui::Label::new(RichText::new(&what).color(look::MUTED)).truncate()).on_hover_text(what);
                ui.end_row();

                ui.label("保存为");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.out).desired_width(380.0));
                    if ui.button("浏览…").clicked() {
                        let p = PathBuf::from(&d.out);
                        let mut dlg = rfd::FileDialog::new().set_title("保存压缩文件");
                        if let Some(dir) = p.parent() {
                            dlg = dlg.set_directory(dir);
                        }
                        if let Some(name) = p.file_name() {
                            dlg = dlg.set_file_name(name.to_string_lossy());
                        }
                        if let Some(p) = dlg.save_file() {
                            d.out = p.display().to_string();
                            if let Some(f) = CreateFormat::from_path(&p) {
                                d.format = f;
                            }
                        }
                    }
                });
                ui.end_row();

                ui.label("格式");
                ui.horizontal(|ui| {
                    for f in CreateFormat::ALL {
                        if ui.selectable_label(d.format == f, f.label()).clicked() && d.format != f {
                            d.format = f;
                            d.out = change_extension(&d.out, f);
                        }
                    }
                });
                ui.end_row();

                ui.label("压缩级别");
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut d.level, 0..=9).show_value(false));
                    ui.label(format!("{} ({})", level_name(d.level), d.level));
                });
                ui.end_row();

                if d.format == CreateFormat::Zip {
                    ui.label("压缩方法");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut d.method, ZipMethod::Deflate, "Deflate（兼容性最好）");
                        ui.selectable_value(&mut d.method, ZipMethod::Zstd, "Zstandard（更快更小）");
                    });
                    ui.end_row();
                }

                ui.label("线程");
                ui.horizontal(|ui| {
                    let label = if d.threads == 0 { format!("自动（{}）", fz_core::default_threads()) } else { d.threads.to_string() };
                    egui::ComboBox::from_id_salt("threads").selected_text(label).show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.threads, 0, "自动");
                        for t in [1, 2, 4, 8, 12, 16, 24, 32, 64] {
                            ui.selectable_value(&mut d.threads, t, t.to_string());
                        }
                    });
                });
                ui.end_row();

                if d.format.supports_password() {
                    ui.label("密码");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut d.password).password(!d.show_pw).hint_text("不加密请留空").desired_width(160.0));
                        ui.add(egui::TextEdit::singleline(&mut d.password2).password(!d.show_pw).hint_text("再次输入").desired_width(160.0));
                        ui.checkbox(&mut d.show_pw, "显示");
                    });
                    ui.end_row();
                    if d.format == CreateFormat::SevenZ && !d.password.is_empty() {
                        ui.label("");
                        ui.checkbox(&mut d.encrypt_names, "同时加密文件名");
                        ui.end_row();
                    }
                }
            });
            if d.format == CreateFormat::Zip && !d.password.is_empty() {
                ui.label(RichText::new("ZIP 使用 AES-256 加密（WinZip AE-2），Bandizip / 7-Zip / WinRAR 均可解压。").color(look::MUTED).size(12.0));
            }
            let mismatch = !d.password.is_empty() && d.password != d.password2 && !d.show_pw;
            if mismatch {
                ui.label(RichText::new("两次输入的密码不一致").color(look::DANGER));
            }
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!mismatch && !d.out.trim().is_empty(), egui::Button::new(RichText::new("  压缩  ").strong())).clicked() {
                    go = true;
                }
                if ui.button("取消").clicked() {
                    keep = false;
                }
            });
        });
        if r.should_close() {
            keep = false;
        }
        if go {
            let out = PathBuf::from(d.out.trim());
            let format = CreateFormat::from_path(&out).unwrap_or(d.format);
            let opts = CreateOptions {
                format,
                level: d.level,
                threads: d.threads,
                password: (!d.password.is_empty() && format.supports_password()).then(|| d.password.clone()),
                zip_method: d.method,
                encrypt_names: d.encrypt_names,
                store_precompressed: self.settings.store_precompressed,
            };
            self.settings.format = format;
            self.settings.level = d.level;
            self.settings.zip_method = d.method;
            self.settings.save();
            let title = format!("正在压缩到 {}", out.file_name().unwrap_or_default().to_string_lossy());
            self.start(ctx, title, Job::Create { out, inputs: std::mem::take(&mut d.inputs), opts });
            keep = false;
            return keep;
        }
        keep
    }

    fn extract_ui(&mut self, ctx: &egui::Context, d: &mut ExtractDlg) -> bool {
        let mut keep = true;
        let mut go = false;
        let r = egui::Modal::new(Id::unique("extract")).show(ctx, |ui| {
            ui.set_width(500.0);
            ui.label(RichText::new("解压").size(18.0).strong());
            ui.add_space(4.0);
            if let Some(sel) = &d.selection {
                ui.label(RichText::new(format!("解压选中的 {} 项", sel.len())).color(look::MUTED));
            }
            ui.add_space(6.0);
            ui.label("目标文件夹");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut d.dest).desired_width(400.0));
                if ui.button("浏览…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("选择解压位置").set_directory(&d.dest).pick_folder() {
                        d.dest = p.display().to_string();
                    }
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("已存在的文件：");
                ui.selectable_value(&mut d.overwrite, Overwrite::Always, "覆盖");
                ui.selectable_value(&mut d.overwrite, Overwrite::Skip, "跳过");
                ui.selectable_value(&mut d.overwrite, Overwrite::Rename, "自动重命名");
            });
            ui.checkbox(&mut d.open_after, "完成后打开文件夹");
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!d.dest.trim().is_empty(), egui::Button::new(RichText::new("  解压  ").strong())).clicked() {
                    go = true;
                }
                if ui.button("取消").clicked() {
                    keep = false;
                }
            });
        });
        if r.should_close() {
            keep = false;
        }
        if go || ctx.input(|i| i.key_pressed(Key::Enter)) && keep {
            self.settings.overwrite = d.overwrite;
            self.settings.open_folder_after_extract = d.open_after;
            self.settings.save();
            let dest = PathBuf::from(d.dest.trim());
            self.extract_job(ctx, dest, d.selection.take(), std::mem::take(&mut d.strip), d.overwrite, d.open_after);
            return false;
        }
        keep
    }

    fn password_ui(&mut self, ctx: &egui::Context, d: &mut PasswordDlg) -> bool {
        let mut keep = true;
        let mut go = false;
        let r = egui::Modal::new(Id::unique("password")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.horizontal(|ui| {
                ui.label(glyph(icon::LOCK, 22.0).color(look::FOLDER));
                ui.label(RichText::new("请输入密码").size(17.0).strong());
            });
            ui.label(RichText::new(&d.title).color(look::MUTED));
            if d.wrong {
                ui.label(RichText::new("密码错误，请重试").color(look::DANGER));
            }
            ui.add_space(6.0);
            let edit = ui.add(egui::TextEdit::singleline(&mut d.text).password(!d.show).desired_width(f32::INFINITY));
            if !edit.has_focus() && d.text.is_empty() {
                edit.request_focus();
            }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                go = true;
            }
            ui.checkbox(&mut d.show, "显示密码");
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("  确定  ").clicked() {
                    go = true;
                }
                if ui.button("取消").clicked() {
                    keep = false;
                }
            });
        });
        if r.should_close() {
            keep = false;
        }
        if !keep {
            self.queue.clear();
            if self.quick {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return false;
        }
        if go && !d.text.is_empty() {
            if let Some(job) = d.job.take() {
                let pw = d.text.clone();
                let after = self.after_open;
                let job = job.with_password(pw);
                let title = d.title.clone();
                self.start(ctx, title, job);
                self.after_open = after;
            }
            return false;
        }
        true
    }

    fn settings_ui(&mut self, ctx: &egui::Context, d: &mut SettingsDlg) -> bool {
        let mut keep = true;
        let r = egui::Modal::new(Id::unique("settings")).show(ctx, |ui| {
            ui.set_width(500.0);
            ui.label(RichText::new("设置").size(18.0).strong());
            ui.add_space(8.0);
            ui.label(RichText::new("系统集成").strong());
            ui.horizontal(|ui| {
                ui.label(if d.shell_installed { "✔ 已添加到资源管理器右键菜单" } else { "未添加到资源管理器右键菜单" });
                let exe = std::env::current_exe().unwrap_or_default();
                if !d.shell_installed {
                    if ui.button("添加右键菜单和“打开方式”").clicked() {
                        d.message = match shell::install(&exe) {
                            Ok(()) => "已添加（Windows 11 中位于“显示更多选项”）".into(),
                            Err(e) => format!("失败：{e}"),
                        };
                        d.shell_installed = shell::is_installed();
                    }
                } else if ui.button("移除").clicked() {
                    d.message = match shell::uninstall() {
                        Ok(()) => "已移除".into(),
                        Err(e) => format!("失败：{e}"),
                    };
                    d.shell_installed = shell::is_installed();
                }
            });
            if !d.message.is_empty() {
                ui.label(RichText::new(&d.message).color(look::MUTED));
            }
            ui.add_space(10.0);
            ui.label(RichText::new("解压").strong());
            ui.checkbox(&mut d.s.open_folder_after_extract, "解压完成后打开文件夹");
            ui.checkbox(&mut d.s.close_after_extract, "解压完成后关闭 FerroZip");
            ui.horizontal(|ui| {
                ui.label("文件名编码（无 UTF-8 标记的 ZIP）");
                egui::ComboBox::from_id_salt("codepage").selected_text(d.s.codepage.label()).show_ui(ui, |ui| {
                    for c in Codepage::ALL {
                        ui.selectable_value(&mut d.s.codepage, c, c.label());
                    }
                });
            });
            ui.add_space(10.0);
            ui.label(RichText::new("压缩").strong());
            ui.checkbox(&mut d.s.store_precompressed, "对 JPG/MP4/ZIP 等已压缩文件直接存储（更快）");
            ui.horizontal(|ui| {
                ui.label("右键“压缩为”的默认级别");
                ui.add(egui::Slider::new(&mut d.s.level, 0..=9));
            });
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("  保存  ").clicked() {
                    let codepage_changed = d.s.codepage != self.settings.codepage;
                    self.settings = d.s.clone();
                    self.settings.save();
                    keep = false;
                    if codepage_changed {
                        if let Some(a) = &self.archive {
                            let p = a.path.clone();
                            self.open(ctx, p);
                        }
                    }
                }
                if ui.button("取消").clicked() {
                    keep = false;
                }
            });
        });
        if r.should_close() {
            keep = false;
        }
        keep
    }

    fn error_ui(&mut self, ctx: &egui::Context) {
        let Some((title, msg)) = &self.error else { return };
        let mut close = false;
        let r = egui::Modal::new(Id::unique("error")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(RichText::new(title).size(17.0).strong());
            ui.add_space(6.0);
            ui.label(msg);
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("  确定  ").clicked() {
                    close = true;
                }
            });
        });
        if close || r.should_close() {
            self.error = None;
            if self.quick {
                if self.queue.is_empty() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    self.next_in_queue(ctx);
                }
            }
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if dropped.is_empty() || self.task.is_some() {
            return;
        }
        if dropped.len() == 1 && looks_like_archive(&dropped[0]) {
            actions.push(Action::OpenPath(dropped[0].clone()));
        } else {
            actions.push(Action::NewArchive(dropped));
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if self.dialog.is_some() || self.task.is_some() || self.error.is_some() {
            return;
        }
        let typing = ctx.memory(|m| m.focused().is_some());
        ctx.input(|i| {
            if i.modifiers.command && i.key_pressed(Key::O) {
                actions.push(Action::OpenDialog);
            }
            if i.modifiers.command && i.key_pressed(Key::N) {
                actions.push(Action::NewArchive(Vec::new()));
            }
            if self.archive.is_some() {
                if i.modifiers.command && i.key_pressed(Key::E) {
                    actions.push(Action::ExtractSelected { dialog: true });
                }
                if !typing {
                    if i.modifiers.command && i.key_pressed(Key::A) {
                        actions.push(Action::SelectAll);
                    }
                    let page = 20;
                    for (key, delta) in [
                        (Key::ArrowDown, 1),
                        (Key::ArrowUp, -1),
                        (Key::PageDown, page),
                        (Key::PageUp, -page),
                        (Key::End, isize::MAX / 2),
                        (Key::Home, -(isize::MAX / 2)),
                    ] {
                        if i.key_pressed(key) {
                            actions.push(Action::Move(delta, i.modifiers.shift));
                        }
                    }
                    if i.key_pressed(Key::Backspace) {
                        actions.push(Action::Up);
                    }
                    if i.key_pressed(Key::Enter) {
                        if let Some(r) = self.selected.iter().next() {
                            actions.push(Action::Activate(*r));
                        }
                    }
                }
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.first_frame {
            self.first_frame = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(if self.title.is_empty() { "FerroZip".into() } else { self.title.clone() }));
        }
        self.poll_task(&ctx);
        if self.rows_dirty {
            self.rebuild_rows();
        }
        let mut actions = Vec::new();
        self.handle_drops(&ctx, &mut actions);
        self.shortcuts(&ctx, &mut actions);

        if self.quick {
            // Context-menu mode: a compact progress window, no browser.
            egui::CentralPanel::default().show(ui, |ui| {
                ui.add_space(6.0);
                if self.task.is_some() {
                    self.task_window(ui);
                } else if let Some((text, _)) = &self.notice {
                    ui.label(text);
                }
            });
        } else {
            egui::Panel::top("toolbar")
                .frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(egui::Margin::symmetric(8, 4)))
                .show(ui, |ui| self.toolbar(ui, &mut actions));
            if self.archive.is_some() {
                egui::Panel::top("address")
                    .frame(egui::Frame::new().fill(Color32::from_rgb(0xF7, 0xF8, 0xFA)).inner_margin(egui::Margin::symmetric(8, 6)))
                    .show(ui, |ui| self.address_bar(ui, &mut actions));
            }
            egui::Panel::bottom("status")
                .frame(egui::Frame::new().fill(Color32::from_rgb(0xF2, 0xF4, 0xF7)).inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| self.status_bar(ui));
            if self.archive.is_some() {
                egui::Panel::left("tree")
                    .default_size(230.0)
                    .frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(egui::Margin::symmetric(6, 6)))
                    .show(ui, |ui| {
                        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                            self.tree_ui(ui, 0, &mut actions);
                        });
                    });
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(egui::Margin::symmetric(4, 2)))
                .show(ui, |ui| {
                    if self.archive.is_some() {
                        self.file_table(ui, &mut actions);
                    } else {
                        self.welcome(ui, &mut actions);
                    }
                });
            if self.task.is_some() {
                egui::Modal::new(Id::unique("task")).show(&ctx, |ui| self.task_window(ui));
            }
        }

        // Drag-over hint.
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering {
            let rect = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::unique("drop")));
            painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(31, 111, 235, 40));
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "松开鼠标：压缩文件将被打开，其他文件将被压缩", egui::FontId::proportional(20.0), look::ACCENT);
        }

        self.dialogs(&ctx);
        self.error_ui(&ctx);
        for a in actions {
            self.apply(&ctx, a);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for d in &self.temp_dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

