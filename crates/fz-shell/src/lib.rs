//! Windows integration for FerroZip, shared by the app and its installer:
//! "Open with" registration for archive types, Explorer context menus, the
//! Default Apps capabilities, the Apps & features (uninstall) entry, PATH,
//! and Start menu / desktop shortcuts.
//!
//! Everything is per-user (HKCU), so neither installing nor uninstalling
//! needs administrator rights. All registry work goes through a [`Hive`],
//! which tests point at a scratch key instead of the real one.

use std::io;
use std::path::{Path, PathBuf};

use winreg::RegKey;
use winreg::RegValue;
use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS, REG_EXPAND_SZ};

pub const APP_NAME: &str = "FerroZip";
pub const PROGID: &str = "FerroZip.Archive";

pub const ARCHIVE_EXTS: [&str; 14] =
    [".zip", ".7z", ".rar", ".tar", ".gz", ".tgz", ".zst", ".tzst", ".xz", ".txz", ".bz2", ".tbz2", ".zipx", ".001"];

const CLASSES: &str = r"Software\Classes";
const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\FerroZip";
const APP_KEY: &str = r"Software\FerroZip";
const CAPABILITIES: &str = r"Software\FerroZip\Capabilities";
const REGISTERED_APPS: &str = r"Software\RegisteredApplications";
const ENVIRONMENT: &str = "Environment";

/// Where registry changes go: the real HKCU, or a scratch key for tests.
pub struct Hive {
    root: RegKey,
    real: bool,
}

impl Hive {
    /// The current user's real registry.
    pub fn user() -> Self {
        Hive { root: RegKey::predef(HKEY_CURRENT_USER), real: true }
    }

    /// A private key under `HKCU\Software\FerroZipTest\<name>` that mirrors
    /// the layout of HKCU. Explorer never reads it.
    pub fn sandbox(name: &str) -> io::Result<Self> {
        let (root, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"Software\FerroZipTest\{name}"))?;
        Ok(Hive { root, real: false })
    }

    /// Remove a sandbox created by [`Hive::sandbox`].
    pub fn delete_sandbox(name: &str) -> io::Result<()> {
        let parent = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(r"Software\FerroZipTest", KEY_ALL_ACCESS)?;
        parent.delete_subkey_all(name)?;
        if parent.enum_keys().next().is_none() {
            RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Software", KEY_ALL_ACCESS)?.delete_subkey("FerroZipTest")?;
        }
        Ok(())
    }

    pub fn key(&self, path: &str) -> io::Result<RegKey> {
        Ok(self.root.create_subkey(path)?.0)
    }

    pub fn open(&self, path: &str) -> Option<RegKey> {
        self.root.open_subkey_with_flags(path, KEY_ALL_ACCESS).ok()
    }

    fn delete_tree(&self, path: &str) {
        let _ = self.root.delete_subkey_all(path);
    }

    /// Tell Explorer that associations changed (real registry only).
    pub fn notify(&self) {
        if self.real {
            notify_shell();
        }
    }
}

fn quoted(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn command(exe: &Path, args: &str) -> String {
    if args.is_empty() { format!("{} \"%1\"", quoted(exe)) } else { format!("{} {args} \"%1\"", quoted(exe)) }
}

fn icon(exe: &Path) -> String {
    format!("{},0", quoted(exe))
}

// ------------------------------------------------------------ associations

/// Make FerroZip appear under "Open with" for every archive type, register
/// it for Settings > Default apps, and give its file type a name and icon.
/// Does not take over the default app: Windows leaves that choice to the user.
pub fn register_associations(h: &Hive, exe: &Path) -> io::Result<()> {
    let p = h.key(&format!(r"{CLASSES}\{PROGID}"))?;
    p.set_value("", &"FerroZip 压缩文件")?;
    p.set_value("FriendlyTypeName", &"FerroZip 压缩文件")?;
    h.key(&format!(r"{CLASSES}\{PROGID}\DefaultIcon"))?.set_value("", &icon(exe))?;
    let shell = h.key(&format!(r"{CLASSES}\{PROGID}\shell"))?;
    shell.set_value("", &"open")?;
    for (verb, label, args) in [
        ("open", "用 FerroZip 打开", ""),
        ("extract_smart", "智能解压", "-xs"),
        ("extract_here", "解压到当前文件夹", "-xh"),
        ("extract_to", "解压到…", "-xd"),
    ] {
        let k = h.key(&format!(r"{CLASSES}\{PROGID}\shell\{verb}"))?;
        k.set_value("", &label)?;
        h.key(&format!(r"{CLASSES}\{PROGID}\shell\{verb}\command"))?.set_value("", &command(exe, args))?;
    }

    let app = format!(r"{CLASSES}\Applications\FerroZip.exe");
    h.key(&app)?.set_value("FriendlyAppName", &APP_NAME)?;
    h.key(&format!(r"{app}\DefaultIcon"))?.set_value("", &icon(exe))?;
    h.key(&format!(r"{app}\shell\open\command"))?.set_value("", &command(exe, ""))?;
    let types = h.key(&format!(r"{app}\SupportedTypes"))?;
    let assoc = h.key(&format!(r"{CAPABILITIES}\FileAssociations"))?;
    for ext in ARCHIVE_EXTS {
        types.set_value(ext, &"")?;
        assoc.set_value(ext, &PROGID)?;
        h.key(&format!(r"{CLASSES}\{ext}\OpenWithProgids"))?.set_value(PROGID, &"")?;
    }
    let caps = h.key(CAPABILITIES)?;
    caps.set_value("ApplicationName", &APP_NAME)?;
    caps.set_value("ApplicationDescription", &"开源、无广告的多核压缩软件")?;
    caps.set_value("ApplicationIcon", &icon(exe))?;
    h.key(REGISTERED_APPS)?.set_value(APP_NAME, &CAPABILITIES)?;
    h.notify();
    Ok(())
}

pub fn unregister_associations(h: &Hive) {
    for ext in ARCHIVE_EXTS {
        if let Some(k) = h.open(&format!(r"{CLASSES}\{ext}\OpenWithProgids")) {
            let _ = k.delete_value(PROGID);
        }
    }
    h.delete_tree(&format!(r"{CLASSES}\{PROGID}"));
    h.delete_tree(&format!(r"{CLASSES}\Applications\FerroZip.exe"));
    h.delete_tree(CAPABILITIES);
    if let Some(k) = h.open(REGISTERED_APPS) {
        let _ = k.delete_value(APP_NAME);
    }
    h.notify();
}

pub fn associations_registered(h: &Hive) -> bool {
    h.open(&format!(r"{CLASSES}\{PROGID}")).is_some()
}

// ------------------------------------------------------------ context menu

struct Verb {
    key: &'static str,
    label: &'static str,
    args: &'static str,
}

const FILE_VERBS: [Verb; 3] = [
    Verb { key: "01zip", label: "压缩为 ZIP", args: "-azip" },
    Verb { key: "02_7z", label: "压缩为 7Z", args: "-a7z" },
    Verb { key: "03add", label: "压缩到…", args: "-a" },
];

const ARCHIVE_VERBS: [Verb; 4] = [
    Verb { key: "01open", label: "用 FerroZip 打开", args: "" },
    Verb { key: "02smart", label: "智能解压", args: "-xs" },
    Verb { key: "03here", label: "解压到当前文件夹", args: "-xh" },
    Verb { key: "04to", label: "解压到…", args: "-xd" },
];

fn write_menu(h: &Hive, parent: &str, name: &str, exe: &Path, verbs: &[Verb]) -> io::Result<()> {
    let base = format!(r"{CLASSES}\{parent}\shell\{name}");
    let menu = h.key(&base)?;
    menu.set_value("MUIVerb", &APP_NAME)?;
    menu.set_value("Icon", &icon(exe))?;
    menu.set_value("SubCommands", &"")?;
    // Explorer starts one process per selected item; they merge themselves.
    menu.set_value("MultiSelectModel", &"Player")?;
    for v in verbs {
        let k = h.key(&format!(r"{base}\shell\{}", v.key))?;
        k.set_value("MUIVerb", &v.label)?;
        k.set_value("MultiSelectModel", &"Player")?;
        h.key(&format!(r"{base}\shell\{}\command", v.key))?.set_value("", &command(exe, v.args))?;
    }
    Ok(())
}

/// Classic context menu (on Windows 11 under "Show more options"):
/// compress for any file or folder, extract for archives.
pub fn register_context_menu(h: &Hive, exe: &Path) -> io::Result<()> {
    write_menu(h, "*", "FerroZip", exe, &FILE_VERBS)?;
    write_menu(h, "Directory", "FerroZip", exe, &FILE_VERBS)?;
    for ext in ARCHIVE_EXTS {
        write_menu(h, &format!(r"SystemFileAssociations\{ext}"), "FerroZip.Extract", exe, &ARCHIVE_VERBS)?;
    }
    h.notify();
    Ok(())
}

pub fn unregister_context_menu(h: &Hive) {
    h.delete_tree(&format!(r"{CLASSES}\*\shell\FerroZip"));
    h.delete_tree(&format!(r"{CLASSES}\Directory\shell\FerroZip"));
    for ext in ARCHIVE_EXTS {
        h.delete_tree(&format!(r"{CLASSES}\SystemFileAssociations\{ext}\shell\FerroZip.Extract"));
    }
    h.notify();
}

pub fn context_menu_registered(h: &Hive) -> bool {
    h.open(&format!(r"{CLASSES}\*\shell\FerroZip")).is_some()
}

// ------------------------------------------------------------ install record

/// What the installer did, so updates keep the choices and the uninstaller
/// knows what to undo.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallRecord {
    pub dir: PathBuf,
    pub version: String,
    pub associate: bool,
    pub context_menu: bool,
    pub start_menu: bool,
    pub desktop: bool,
    pub path_env: bool,
}

pub fn read_install(h: &Hive) -> Option<InstallRecord> {
    let k = h.open(APP_KEY)?;
    let dir: String = k.get_value("InstallLocation").ok()?;
    let flag = |n: &str| k.get_value::<u32, _>(n).map(|v| v != 0).unwrap_or(false);
    Some(InstallRecord {
        dir: PathBuf::from(dir),
        version: k.get_value("Version").unwrap_or_default(),
        associate: flag("Associate"),
        context_menu: flag("ContextMenu"),
        start_menu: flag("StartMenu"),
        desktop: flag("Desktop"),
        path_env: flag("PathEnv"),
    })
}

/// Record the installation and add the Apps & features entry.
pub fn write_install(h: &Hive, r: &InstallRecord, uninstaller: &Path, size_kb: u32) -> io::Result<()> {
    let k = h.key(APP_KEY)?;
    k.set_value("InstallLocation", &r.dir.display().to_string())?;
    k.set_value("Version", &r.version)?;
    for (n, v) in [
        ("Associate", r.associate),
        ("ContextMenu", r.context_menu),
        ("StartMenu", r.start_menu),
        ("Desktop", r.desktop),
        ("PathEnv", r.path_env),
    ] {
        k.set_value(n, &u32::from(v))?;
    }
    let exe = r.dir.join("FerroZip.exe");
    let u = h.key(UNINSTALL)?;
    u.set_value("DisplayName", &APP_NAME)?;
    u.set_value("DisplayVersion", &r.version)?;
    u.set_value("Publisher", &"FerroZip contributors")?;
    u.set_value("DisplayIcon", &icon(&exe))?;
    u.set_value("InstallLocation", &r.dir.display().to_string())?;
    u.set_value("UninstallString", &format!("{} --uninstall", quoted(uninstaller)))?;
    u.set_value("QuietUninstallString", &format!("{} --uninstall --quiet", quoted(uninstaller)))?;
    u.set_value("EstimatedSize", &size_kb)?;
    u.set_value("NoModify", &1u32)?;
    u.set_value("NoRepair", &1u32)?;
    Ok(())
}

pub fn remove_install(h: &Hive) {
    h.delete_tree(UNINSTALL);
    // Keep the user's settings file; only the install record goes.
    if let Some(k) = h.open(APP_KEY) {
        for n in ["InstallLocation", "Version", "Associate", "ContextMenu", "StartMenu", "Desktop", "PathEnv"] {
            let _ = k.delete_value(n);
        }
    }
    if h.open(APP_KEY).is_some_and(|k| k.enum_keys().next().is_none() && k.enum_values().next().is_none()) {
        h.delete_tree(APP_KEY);
    }
}

// ------------------------------------------------------------ PATH

fn same_dir(a: &str, b: &Path) -> bool {
    a.trim_end_matches('\\').eq_ignore_ascii_case(b.display().to_string().trim_end_matches('\\'))
}

/// Add `dir` to the user's PATH (for the `fz` command-line tool).
pub fn add_to_path(h: &Hive, dir: &Path) -> io::Result<()> {
    let env = h.key(ENVIRONMENT)?;
    let current: String = env.get_value("Path").unwrap_or_default();
    if current.split(';').any(|p| same_dir(p, dir)) {
        return Ok(());
    }
    let mut new = current.trim_end_matches(';').to_string();
    if !new.is_empty() {
        new.push(';');
    }
    new.push_str(&dir.display().to_string());
    set_expand_sz(&env, "Path", &new)?;
    if h.real {
        broadcast_environment();
    }
    Ok(())
}

pub fn remove_from_path(h: &Hive, dir: &Path) -> io::Result<()> {
    let Some(env) = h.open(ENVIRONMENT) else { return Ok(()) };
    let current: String = env.get_value("Path").unwrap_or_default();
    let kept: Vec<&str> = current.split(';').filter(|p| !p.is_empty() && !same_dir(p, dir)).collect();
    if kept.len() != current.split(';').filter(|p| !p.is_empty()).count() {
        set_expand_sz(&env, "Path", &kept.join(";"))?;
        if h.real {
            broadcast_environment();
        }
    }
    Ok(())
}

fn set_expand_sz(k: &RegKey, name: &str, value: &str) -> io::Result<()> {
    let mut bytes: Vec<u8> = value.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
    bytes.shrink_to_fit();
    k.set_raw_value(name, &RegValue { bytes: bytes.into(), vtype: REG_EXPAND_SZ })
}

fn broadcast_environment() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
    let env: Vec<u16> = "Environment\0".encode_utf16().collect();
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 3000, &mut result);
    }
}

// ------------------------------------------------------------ shortcuts

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    unsafe {
        let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
        if SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) != 0 || p.is_null() {
            return None;
        }
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        CoTaskMemFree(p as *const _);
        Some(PathBuf::from(path))
    }
}

/// `Start Menu\Programs\FerroZip.lnk` for the current user.
pub fn start_menu_shortcut() -> Option<PathBuf> {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Programs).map(|d| d.join("FerroZip.lnk"))
}

/// `FerroZip.lnk` on the (possibly redirected) desktop.
pub fn desktop_shortcut() -> Option<PathBuf> {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Desktop).map(|d| d.join("FerroZip.lnk"))
}

pub fn create_shortcut(lnk: &Path, target: &Path) -> io::Result<()> {
    let mut link = mslnk::ShellLink::new(target).map_err(|e| io::Error::other(e.to_string()))?;
    link.set_name(Some("开源、无广告的压缩软件".into()));
    if let Some(dir) = target.parent() {
        link.set_working_dir(Some(dir.display().to_string()));
    }
    link.set_icon_location(Some(target.display().to_string()));
    if let Some(dir) = lnk.parent() {
        std::fs::create_dir_all(dir)?;
    }
    link.create_lnk(lnk).map_err(|e| io::Error::other(e.to_string()))
}

// ------------------------------------------------------------ shell helpers

pub fn notify_shell() {
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}

pub fn open_path(path: &Path) {
    open_target(&path.display().to_string());
}

/// Open a file, folder or URI (such as `ms-settings:`) with its handler.
pub fn open_target(target: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), wide.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
    }
}

/// Show a file selected in Explorer, or open a folder.
pub fn reveal(path: &Path) {
    if path.is_dir() {
        open_path(path);
    } else {
        let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
    }
}

/// Settings > Default apps, opened on FerroZip's page.
pub fn open_default_apps() {
    open_target("ms-settings:defaultapps?registeredAppUser=FerroZip");
}

// ------------------------------------------------------------ app settings page

/// "Open with" + context menu in one go (the app's settings button).
pub fn install(exe: &Path) -> io::Result<()> {
    let h = Hive::user();
    register_associations(&h, exe)?;
    register_context_menu(&h, exe)
}

pub fn uninstall() -> io::Result<()> {
    let h = Hive::user();
    unregister_associations(&h);
    unregister_context_menu(&h);
    Ok(())
}

pub fn is_installed() -> bool {
    context_menu_registered(&Hive::user())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs against a scratch key, never the real associations.
    struct Sandbox(String, Hive);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let name = format!("{tag}-{}", std::process::id());
            let h = Hive::sandbox(&name).unwrap();
            Sandbox(name, h)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = Hive::delete_sandbox(&self.0);
        }
    }

    #[test]
    #[ignore = r"writes to HKCU\Software\FerroZipTest (run with --ignored)"]
    fn associations_round_trip() {
        let s = Sandbox::new("assoc");
        let exe = Path::new(r"C:\Program Files\FerroZip\FerroZip.exe");
        register_associations(&s.1, exe).unwrap();
        assert!(associations_registered(&s.1));
        let k = s.1.open(r"Software\Classes\.7z\OpenWithProgids").unwrap();
        let _: String = k.get_value(PROGID).unwrap();
        let cmd: String = s.1.open(r"Software\Classes\FerroZip.Archive\shell\open\command").unwrap().get_value("").unwrap();
        assert_eq!(cmd, r#""C:\Program Files\FerroZip\FerroZip.exe" "%1""#);
        let name: String = s.1.open(r"Software\Classes\Applications\FerroZip.exe").unwrap().get_value("FriendlyAppName").unwrap();
        assert_eq!(name, "FerroZip");
        let caps: String = s.1.open(r"Software\RegisteredApplications").unwrap().get_value("FerroZip").unwrap();
        assert_eq!(caps, CAPABILITIES);
        unregister_associations(&s.1);
        assert!(!associations_registered(&s.1));
        assert!(s.1.open(r"Software\Classes\.7z\OpenWithProgids").unwrap().get_value::<String, _>(PROGID).is_err());
    }

    #[test]
    #[ignore = r"writes to HKCU\Software\FerroZipTest (run with --ignored)"]
    fn context_menu_round_trip() {
        let s = Sandbox::new("menu");
        let exe = Path::new(r"C:\x\FerroZip.exe");
        register_context_menu(&s.1, exe).unwrap();
        assert!(context_menu_registered(&s.1));
        let cmd: String = s
            .1
            .open(r"Software\Classes\SystemFileAssociations\.rar\shell\FerroZip.Extract\shell\02smart\command")
            .unwrap()
            .get_value("")
            .unwrap();
        assert_eq!(cmd, r#""C:\x\FerroZip.exe" -xs "%1""#);
        unregister_context_menu(&s.1);
        assert!(!context_menu_registered(&s.1));
    }

    #[test]
    #[ignore = r"writes to HKCU\Software\FerroZipTest (run with --ignored)"]
    fn install_record_and_path() {
        let s = Sandbox::new("record");
        let dir = PathBuf::from(r"C:\Users\me\AppData\Local\Programs\FerroZip");
        let r = InstallRecord { dir: dir.clone(), version: "0.1.0".into(), associate: true, context_menu: false, start_menu: true, desktop: false, path_env: true };
        write_install(&s.1, &r, &dir.join("uninstall.exe"), 15000).unwrap();
        assert_eq!(read_install(&s.1), Some(r));
        let u: String = s.1.open(UNINSTALL).unwrap().get_value("UninstallString").unwrap();
        assert!(u.ends_with("uninstall.exe\" --uninstall"));

        s.1.key(ENVIRONMENT).unwrap().set_value("Path", &r"C:\tools;D:\bin").unwrap();
        add_to_path(&s.1, &dir).unwrap();
        add_to_path(&s.1, &dir).unwrap();
        let p: String = s.1.open(ENVIRONMENT).unwrap().get_value("Path").unwrap();
        assert_eq!(p, format!(r"C:\tools;D:\bin;{}", dir.display()));
        remove_from_path(&s.1, &dir).unwrap();
        let p: String = s.1.open(ENVIRONMENT).unwrap().get_value("Path").unwrap();
        assert_eq!(p, r"C:\tools;D:\bin");

        remove_install(&s.1);
        assert_eq!(read_install(&s.1), None);
        assert!(s.1.open(UNINSTALL).is_none());
    }

    #[test]
    fn shortcut_file() {
        let dir = std::env::temp_dir().join(format!("fz-lnk-{}", std::process::id()));
        let lnk = dir.join("FerroZip.lnk");
        let target = std::env::current_exe().unwrap();
        create_shortcut(&lnk, &target).unwrap();
        let bytes = std::fs::read(&lnk).unwrap();
        // ShellLinkHeader: size 0x4C and the shell link CLSID.
        assert_eq!(&bytes[..4], &[0x4C, 0, 0, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
