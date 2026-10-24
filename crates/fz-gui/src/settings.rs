//! Persistent preferences in %APPDATA%\FerroZip\settings.ini (plain key=value).

use std::path::PathBuf;

use fz_core::codepage::Codepage;
use fz_core::{CreateFormat, Overwrite, ZipMethod};

#[derive(Clone, Debug)]
pub struct Settings {
    pub format: CreateFormat,
    pub level: u32,
    pub threads: usize,
    pub zip_method: ZipMethod,
    pub overwrite: Overwrite,
    pub codepage: Codepage,
    pub open_folder_after_extract: bool,
    pub close_after_extract: bool,
    pub store_precompressed: bool,
    pub recent: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: CreateFormat::Zip,
            level: 5,
            threads: 0,
            zip_method: ZipMethod::Deflate,
            overwrite: Overwrite::Always,
            codepage: Codepage::Auto,
            open_folder_after_extract: true,
            close_after_extract: false,
            store_precompressed: true,
            recent: Vec::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("FerroZip").join("settings.ini"))
}

fn format_key(f: CreateFormat) -> &'static str {
    f.extension()
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Settings::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "format" => {
                    if let Some(f) = CreateFormat::ALL.into_iter().find(|f| format_key(*f) == v) {
                        s.format = f;
                    }
                }
                "level" => s.level = v.parse().unwrap_or(5).min(9),
                "threads" => s.threads = v.parse().unwrap_or(0),
                "zip_method" => s.zip_method = if v == "zstd" { ZipMethod::Zstd } else { ZipMethod::Deflate },
                "overwrite" => {
                    s.overwrite = match v {
                        "skip" => Overwrite::Skip,
                        "rename" => Overwrite::Rename,
                        _ => Overwrite::Always,
                    }
                }
                "codepage" => {
                    if let Some(c) = Codepage::ALL.into_iter().find(|c| format!("{c:?}") == v) {
                        s.codepage = c;
                    }
                }
                "open_folder_after_extract" => s.open_folder_after_extract = v == "1",
                "close_after_extract" => s.close_after_extract = v == "1",
                "store_precompressed" => s.store_precompressed = v == "1",
                "recent" => s.recent.push(PathBuf::from(v)),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        let b = |x: bool| if x { "1" } else { "0" };
        let mut out = format!(
            "format={}\nlevel={}\nthreads={}\nzip_method={}\noverwrite={}\ncodepage={:?}\nopen_folder_after_extract={}\nclose_after_extract={}\nstore_precompressed={}\n",
            format_key(self.format),
            self.level,
            self.threads,
            if self.zip_method == ZipMethod::Zstd { "zstd" } else { "deflate" },
            match self.overwrite {
                Overwrite::Always => "always",
                Overwrite::Skip => "skip",
                Overwrite::Rename => "rename",
            },
            self.codepage,
            b(self.open_folder_after_extract),
            b(self.close_after_extract),
            b(self.store_precompressed),
        );
        for r in &self.recent {
            out.push_str(&format!("recent={}\n", r.display()));
        }
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, out);
    }

    pub fn push_recent(&mut self, path: &std::path::Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(10);
        self.save();
    }
}
