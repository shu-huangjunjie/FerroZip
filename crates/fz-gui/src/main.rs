#![windows_subsystem = "windows"]

mod app;
mod collect;
mod icon_pixels;
mod look;
mod settings;
mod task;
mod tree;

use std::path::PathBuf;

use eframe::egui;
use fz_core::CreateFormat;

use app::{App, StartMode};

/// Command line (also what the Explorer context menu calls):
///   FerroZip.exe [archive]           browse
///   FerroZip.exe -xs  <archives…>    smart extract
///   FerroZip.exe -xh  <archives…>    extract here
///   FerroZip.exe -xd  <archives…>    extract to… (dialog)
///   FerroZip.exe -a   <files…>       compress to… (dialog)
///   FerroZip.exe -azip|-a7z <files…> compress next to the files
fn parse_args() -> Option<StartMode> {
    let mut args = std::env::args_os().skip(1);
    let first = match args.next() {
        Some(a) => a,
        None => return Some(StartMode::Browse(None)),
    };
    let rest: Vec<PathBuf> = args.map(PathBuf::from).collect();
    let verb = first.to_string_lossy().to_string();
    let gather = |files: Vec<PathBuf>| collect::collect(&verb, files);
    Some(match verb.as_str() {
        "-xs" => StartMode::SmartExtract(gather(rest)?),
        "-xh" => StartMode::ExtractHere(gather(rest)?),
        "-xd" => StartMode::ExtractTo(gather(rest)?),
        "-a" => StartMode::Compress(gather(rest)?),
        "-azip" => StartMode::QuickCompress(gather(rest)?, CreateFormat::Zip),
        "-a7z" => StartMode::QuickCompress(gather(rest)?, CreateFormat::SevenZ),
        _ => StartMode::Browse(Some(PathBuf::from(first))),
    })
}

fn main() -> eframe::Result {
    let Some(mode) = parse_args() else { return Ok(()) };
    let compact = !matches!(mode, StartMode::Browse(_) | StartMode::Compress(_) | StartMode::ExtractTo(_));
    let size = match mode {
        StartMode::Browse(_) => [1040.0, 680.0],
        StartMode::Compress(_) | StartMode::ExtractTo(_) => [600.0, 460.0],
        _ => [480.0, 200.0],
    };
    let viewport = egui::ViewportBuilder::default()
        .with_title("FerroZip")
        .with_inner_size(size)
        .with_min_inner_size(if compact { [420.0, 180.0] } else { [560.0, 380.0] })
        .with_resizable(!compact)
        .with_icon(std::sync::Arc::new(look::app_icon()))
        .with_drag_and_drop(true);
    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    let settings = settings::Settings::load();
    eframe::run_native(
        "FerroZip",
        options,
        Box::new(move |cc| {
            look::install_fonts(&cc.egui_ctx);
            look::apply_style(&cc.egui_ctx);
            Ok(Box::new(App::new(cc, mode, settings)))
        }),
    )
}
