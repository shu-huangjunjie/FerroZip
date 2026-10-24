//! FerroZip-Setup.exe: a one-page installer, and (as uninstall.exe or with
//! `--uninstall`) the uninstaller. `--quiet` does either without a window.

#![windows_subsystem = "windows"]

#[allow(dead_code)]
#[path = "../../fz-gui/src/icon_pixels.rs"]
mod icon_pixels;
mod install;
#[allow(dead_code)]
#[path = "../../fz-gui/src/look.rs"]
mod look;

use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Ui, Vec2};
use fz_shell::{Hive, InstallRecord};

use install::Options;
use look::{glyph, icon};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Install,
    Uninstall,
}

enum Page {
    Start,
    Working,
    Done(Result<(), String>),
}

enum Msg {
    Step(f32, String),
    Finished(Result<(), String>),
}

struct Setup {
    mode: Mode,
    page: Page,
    opts: Options,
    existing: Option<InstallRecord>,
    rx: Option<Receiver<Msg>>,
    frac: f32,
    status: String,
    launch: bool,
}

impl Setup {
    fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let (mode, opts, ctx) = (self.mode, self.opts.clone(), ctx.clone());
        std::thread::spawn(move || {
            let step = |f: f32, s: &str| {
                let _ = tx.send(Msg::Step(f, s.to_string()));
                ctx.request_repaint();
            };
            let r = match mode {
                Mode::Install => install::install(&opts, &step),
                Mode::Uninstall => install::uninstall(&step),
            };
            let _ = tx.send(Msg::Finished(r));
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.page = Page::Working;
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Step(f, s) => {
                    self.frac = f;
                    self.status = s;
                }
                Msg::Finished(r) => {
                    self.page = Page::Done(r);
                }
            }
        }
    }

    fn header(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(glyph(icon::ZIP, 46.0).color(look::ACCENT));
            ui.add_space(6.0);
            ui.vertical(|ui| {
                let title = match (self.mode, &self.existing) {
                    (Mode::Uninstall, _) => "卸载 FerroZip".to_string(),
                    (Mode::Install, Some(r)) if r.version != install::VERSION => format!("更新 FerroZip {} → {}", r.version, install::VERSION),
                    (Mode::Install, Some(_)) => format!("重新安装 FerroZip {}", install::VERSION),
                    (Mode::Install, None) => format!("安装 FerroZip {}", install::VERSION),
                };
                ui.label(RichText::new(title).size(22.0).strong());
                ui.label(RichText::new("开源 · 无广告 · 多核极速压缩").color(look::MUTED));
            });
        });
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
    }

    fn start_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if self.mode == Mode::Uninstall {
            ui.label("将删除 FerroZip 的程序文件、“打开方式”关联、右键菜单和快捷方式。");
            ui.label(RichText::new("你的设置（%APPDATA%\\FerroZip）会保留。").color(look::MUTED));
            if let Some(r) = &self.existing {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("安装位置：{}", r.dir.display())).color(look::MUTED));
            }
        } else {
            ui.label("安装位置");
            ui.horizontal(|ui| {
                let mut text = self.opts.dir.display().to_string();
                if ui.add(egui::TextEdit::singleline(&mut text).desired_width(430.0)).changed() {
                    self.opts.dir = text.into();
                }
                if ui.button("浏览…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("选择安装位置").pick_folder() {
                        self.opts.dir = p.join("FerroZip");
                    }
                }
            });
            ui.add_space(8.0);
            ui.checkbox(&mut self.opts.associate, "关联压缩文件（在右键“打开方式”中显示 FerroZip）");
            ui.checkbox(&mut self.opts.context_menu, "添加右键菜单（压缩为 ZIP/7Z、智能解压、解压到…）");
            ui.checkbox(&mut self.opts.start_menu, "创建开始菜单快捷方式");
            ui.checkbox(&mut self.opts.desktop, "创建桌面快捷方式");
            ui.checkbox(&mut self.opts.path_env, "把命令行工具 fz 加入 PATH");
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("只为当前用户安装，不需要管理员权限").size(12.0).color(look::MUTED));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = match self.mode {
                        Mode::Uninstall => "  卸载  ",
                        Mode::Install if self.existing.is_some() => "  更新  ",
                        Mode::Install => "  安装  ",
                    };
                    let ok = self.mode == Mode::Uninstall || !self.opts.dir.as_os_str().is_empty();
                    if ui.add_enabled(ok, primary(label)).clicked() {
                        self.start(ctx);
                    }
                    if ui.button("取消").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }

    fn working_page(&self, ui: &mut Ui) {
        ui.add_space(30.0);
        ui.label(&self.status);
        ui.add_space(8.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 9.0, Color32::from_rgb(0xE6, 0xEA, 0xF0));
        let mut fill = rect;
        fill.set_width((rect.width() * self.frac).max(18.0));
        p.rect_filled(fill, 9.0, look::ACCENT);
    }

    fn done_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let Page::Done(result) = &self.page else { return };
        match result {
            Ok(()) if self.mode == Mode::Install => {
                ui.label(RichText::new("✔ 安装完成").size(18.0).strong().color(Color32::from_rgb(0x16, 0x7C, 0x3A)));
                ui.add_space(8.0);
                ui.label("现在可以在压缩文件上右键 → 打开方式 → FerroZip；");
                ui.label("在“显示更多选项”里还有 FerroZip 的压缩 / 解压菜单。");
                ui.add_space(8.0);
                if self.opts.associate && ui.link("把 FerroZip 设为默认的压缩软件…").clicked() {
                    fz_shell::open_default_apps();
                }
                ui.add_space(8.0);
                ui.checkbox(&mut self.launch, "立即运行 FerroZip");
            }
            Ok(()) => {
                ui.label(RichText::new("✔ FerroZip 已卸载").size(18.0).strong());
                ui.add_space(6.0);
                ui.label("感谢使用。");
            }
            Err(e) => {
                ui.label(RichText::new("出错了").size(18.0).strong().color(look::DANGER));
                ui.add_space(6.0);
                ui.label(e);
            }
        }
        let ok = result.is_ok();
        ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
            let label = if ok { "  完成  " } else { "  关闭  " };
            if ui.add(primary(label)).clicked() {
                if ok && self.mode == Mode::Install && self.launch {
                    install::launch(&self.opts.dir);
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).fill(look::ACCENT)
}

impl eframe::App for Setup {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(egui::Margin::same(22)))
            .show(ui, |ui| {
                self.header(ui);
                match self.page {
                    Page::Start => self.start_page(ui, &ctx),
                    Page::Working => self.working_page(ui),
                    Page::Done(_) => self.done_page(ui, &ctx),
                }
            });
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let me = std::env::current_exe().unwrap_or_default();
    let named_uninstall = me.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(install::UNINSTALLER));
    let mode = if named_uninstall || args.iter().any(|a| a == "--uninstall") { Mode::Uninstall } else { Mode::Install };
    let existing = fz_shell::read_install(&Hive::user());

    if args.iter().any(|a| a == "--quiet") {
        let opts = Options::initial(existing.as_ref());
        let r = match mode {
            Mode::Install => install::install(&opts, &|_, _| {}),
            Mode::Uninstall => install::uninstall(&|_, _| {}),
        };
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }

    let title = if mode == Mode::Install { "FerroZip 安装程序" } else { "卸载 FerroZip" };
    let viewport = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size([620.0, 420.0])
        .with_resizable(false)
        .with_maximize_button(false)
        .with_icon(std::sync::Arc::new(look::app_icon()));
    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    let app = Setup {
        mode,
        page: Page::Start,
        opts: Options::initial(existing.as_ref()),
        existing,
        rx: None,
        frac: 0.0,
        status: String::new(),
        launch: true,
    };
    let _ = eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            look::install_fonts(&cc.egui_ctx);
            look::apply_style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
}
