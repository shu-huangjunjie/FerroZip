//! What the installer does: unpack the program, register it with Windows,
//! and the reverse. Per-user, so no administrator rights are involved.

use std::path::{Path, PathBuf};

use fz_shell::{Hive, InstallRecord};

static PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.zst"));

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const UNINSTALLER: &str = "uninstall.exe";

#[derive(Clone, Debug)]
pub struct Options {
    pub dir: PathBuf,
    pub associate: bool,
    pub context_menu: bool,
    pub start_menu: bool,
    pub desktop: bool,
    pub path_env: bool,
}

impl Options {
    /// Defaults, or the choices of an existing installation (update).
    pub fn initial(existing: Option<&InstallRecord>) -> Self {
        match existing {
            Some(r) => Options {
                dir: r.dir.clone(),
                associate: r.associate,
                context_menu: r.context_menu,
                start_menu: r.start_menu,
                desktop: r.desktop,
                path_env: r.path_env,
            },
            None => Options {
                dir: default_dir(),
                associate: true,
                context_menu: true,
                start_menu: true,
                desktop: false,
                path_env: false,
            },
        }
    }
}

/// `%LOCALAPPDATA%\Programs\FerroZip`, where per-user apps live.
pub fn default_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\FerroZip")).join("Programs").join("FerroZip")
}

fn payload() -> Result<Vec<(String, Vec<u8>)>, String> {
    let raw = zstd::decode_all(PAYLOAD).map_err(|e| format!("安装包已损坏：{e}"))?;
    let mut files = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let field = |i: usize, n: usize| raw.get(i..i + n).ok_or_else(|| "安装包已损坏".to_string());
        let name_len = u16::from_le_bytes(field(i, 2)?.try_into().unwrap()) as usize;
        let name = String::from_utf8_lossy(field(i + 2, name_len)?).into_owned();
        let size = u64::from_le_bytes(field(i + 2 + name_len, 8)?.try_into().unwrap()) as usize;
        let data = field(i + 10 + name_len, size)?.to_vec();
        i += 10 + name_len + size;
        files.push((name, data));
    }
    if files.is_empty() {
        return Err("这个安装包不含程序文件：请用 package.ps1 构建".into());
    }
    Ok(files)
}

/// Replace `path` with `data`. A running FerroZip.exe cannot be overwritten,
/// but it can be renamed, so updating works while the app is open; the old
/// copy is removed the next time this runs.
fn replace_file(path: &Path, data: &[u8]) -> Result<(), String> {
    if path.exists() && std::fs::remove_file(path).is_err() {
        let stale = path.with_extension(format!("old-{}", std::process::id()));
        std::fs::rename(path, &stale).map_err(|e| format!("无法替换 {}：{e}", path.display()))?;
    }
    std::fs::write(path, data).map_err(|e| format!("无法写入 {}：{e}", path.display()))
}

fn remove_stale(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().contains(".old-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn dir_size_kb(dir: &Path) -> u32 {
    let bytes: u64 = std::fs::read_dir(dir).map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0);
    (bytes / 1024) as u32
}

/// Install (or update) into `opts.dir`. `step` reports (fraction, message).
pub fn install(opts: &Options, step: &dyn Fn(f32, &str)) -> Result<(), String> {
    let hive = Hive::user();
    let previous = fz_shell::read_install(&hive);
    step(0.05, "正在解压程序文件…");
    let files = payload()?;
    std::fs::create_dir_all(&opts.dir).map_err(|e| format!("无法创建 {}：{e}", opts.dir.display()))?;
    remove_stale(&opts.dir);
    for (k, (name, data)) in files.iter().enumerate() {
        step(0.1 + 0.5 * k as f32 / files.len() as f32, &format!("正在复制 {name}…"));
        replace_file(&opts.dir.join(name), data)?;
    }

    step(0.65, "正在安装卸载程序…");
    let uninstaller = opts.dir.join(UNINSTALLER);
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if !same_file(&me, &uninstaller) {
        let data = std::fs::read(&me).map_err(|e| format!("无法读取安装程序：{e}"))?;
        replace_file(&uninstaller, &data)?;
    }

    step(0.75, "正在注册到 Windows…");
    let exe = opts.dir.join("FerroZip.exe");
    // An update that moved the program or dropped an option undoes the old one.
    if let Some(p) = &previous {
        if p.path_env && (!opts.path_env || p.dir != opts.dir) {
            let _ = fz_shell::remove_from_path(&hive, &p.dir);
        }
    }
    if opts.associate {
        fz_shell::register_associations(&hive, &exe).map_err(|e| format!("关联文件失败：{e}"))?;
    } else {
        fz_shell::unregister_associations(&hive);
    }
    if opts.context_menu {
        fz_shell::register_context_menu(&hive, &exe).map_err(|e| format!("添加右键菜单失败：{e}"))?;
    } else {
        fz_shell::unregister_context_menu(&hive);
    }
    if opts.path_env {
        fz_shell::add_to_path(&hive, &opts.dir).map_err(|e| format!("设置 PATH 失败：{e}"))?;
    }

    step(0.85, "正在创建快捷方式…");
    for (wanted, lnk) in [(opts.start_menu, fz_shell::start_menu_shortcut()), (opts.desktop, fz_shell::desktop_shortcut())] {
        let Some(lnk) = lnk else { continue };
        if wanted {
            fz_shell::create_shortcut(&lnk, &exe).map_err(|e| format!("创建快捷方式失败：{e}"))?;
        } else {
            let _ = std::fs::remove_file(&lnk);
        }
    }

    let record = InstallRecord {
        dir: opts.dir.clone(),
        version: VERSION.into(),
        associate: opts.associate,
        context_menu: opts.context_menu,
        start_menu: opts.start_menu,
        desktop: opts.desktop,
        path_env: opts.path_env,
    };
    fz_shell::write_install(&hive, &record, &uninstaller, dir_size_kb(&opts.dir)).map_err(|e| format!("写入安装信息失败：{e}"))?;
    if let Some(p) = previous.filter(|p| p.dir != opts.dir) {
        // Moved to a new folder: the old copy is no longer referenced.
        for name in ["FerroZip.exe", "fz.exe"] {
            let _ = std::fs::remove_file(p.dir.join(name));
        }
    }
    fz_shell::notify_shell();
    step(1.0, "完成");
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Undo everything `install` did. The user's settings file stays.
pub fn uninstall(step: &dyn Fn(f32, &str)) -> Result<(), String> {
    let hive = Hive::user();
    let record = fz_shell::read_install(&hive);
    let dir = match &record {
        Some(r) => r.dir.clone(),
        None => std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).ok_or("找不到安装位置")?,
    };
    step(0.1, "正在检查 FerroZip 是否在运行…");
    let exe = dir.join("FerroZip.exe");
    if exe.exists() && std::fs::remove_file(&exe).is_err() {
        return Err("FerroZip 正在运行，请先关闭它再卸载。".into());
    }

    step(0.3, "正在删除文件关联和右键菜单…");
    fz_shell::unregister_associations(&hive);
    fz_shell::unregister_context_menu(&hive);
    let _ = fz_shell::remove_from_path(&hive, &dir);
    for lnk in [fz_shell::start_menu_shortcut(), fz_shell::desktop_shortcut()].into_iter().flatten() {
        let _ = std::fs::remove_file(lnk);
    }
    fz_shell::remove_install(&hive);

    step(0.7, "正在删除程序文件…");
    let _ = std::fs::remove_file(dir.join("fz.exe"));
    remove_stale(&dir);
    fz_shell::notify_shell();

    // This program is uninstall.exe inside `dir` and cannot delete itself
    // while running: a detached shell removes it and the folder afterwards.
    let me = std::env::current_exe().ok();
    if me.as_deref().is_some_and(|m| m.parent() == Some(dir.as_path())) {
        schedule_removal(&dir);
    } else {
        let _ = std::fs::remove_file(dir.join(UNINSTALLER));
        let _ = std::fs::remove_dir(&dir);
    }
    step(1.0, "完成");
    Ok(())
}

fn schedule_removal(dir: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let script = format!(
        "ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"",
        dir.join(UNINSTALLER).display(),
        dir.display()
    );
    let _ = std::process::Command::new("cmd.exe").args(["/d", "/c"]).raw_arg(&script).creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS).spawn();
}

/// Start the installed app without waiting for it.
pub fn launch(dir: &Path) {
    let _ = std::process::Command::new(dir.join("FerroZip.exe")).current_dir(dir).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_format() {
        // Whatever this build embedded must parse: either the real files or
        // the explicit "built without payload" error.
        match payload() {
            Ok(files) => assert!(files.iter().any(|(n, d)| n == "FerroZip.exe" && d.starts_with(b"MZ"))),
            Err(e) => assert!(e.contains("package.ps1"), "{e}"),
        }
    }

    #[test]
    fn replace_works_on_new_and_existing_files() {
        let dir = std::env::temp_dir().join(format!("fz-setup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.exe");
        replace_file(&f, b"one").unwrap();
        replace_file(&f, b"two").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
