//! `fz` — FerroZip command line.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fz_core::codepage::Codepage;
use fz_core::util::human_bytes;
use fz_core::*;

const USAGE: &str = "\
FerroZip 命令行

用法:
  fz a [选项] <压缩文件> <文件或文件夹>...   创建压缩文件 (格式由扩展名决定)
  fz x [选项] <压缩文件> [-o <目录>]         解压
  fz l [选项] <压缩文件>                    列出内容
  fz t [选项] <压缩文件>                    测试完整性

选项:
  -l <0-9>      压缩级别 (0=仅存储, 5=标准, 9=最大)     默认 5
  -t <N>        线程数 (0=全部 CPU)                      默认 0
  -p <密码>     密码 (ZIP 使用 AES-256)
  -m zstd       ZIP 使用 Zstandard 压缩方法 (默认 Deflate)
  -o <目录>     解压目标目录 (默认: 智能解压到压缩文件旁)
  -y / -s / -r  已存在的文件: 覆盖 / 跳过 / 自动重命名     默认 覆盖
  -cp <名称>    文件名编码: auto utf8 gbk big5 sjis euckr cp437
";

struct Args {
    cmd: String,
    archive: PathBuf,
    inputs: Vec<PathBuf>,
    level: u32,
    threads: usize,
    password: Option<String>,
    zstd: bool,
    out: Option<PathBuf>,
    overwrite: Overwrite,
    codepage: Codepage,
}

fn parse() -> std::result::Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(|| USAGE.to_string())?;
    let mut a = Args {
        cmd,
        archive: PathBuf::new(),
        inputs: Vec::new(),
        level: 5,
        threads: 0,
        password: None,
        zstd: false,
        out: None,
        overwrite: Overwrite::Always,
        codepage: Codepage::Auto,
    };
    let mut positional = Vec::new();
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} 需要一个参数"));
        match arg.as_str() {
            "-l" => a.level = val("-l")?.parse().map_err(|_| "无效的级别")?,
            "-t" => a.threads = val("-t")?.parse().map_err(|_| "无效的线程数")?,
            "-p" => a.password = Some(val("-p")?),
            "-m" => a.zstd = val("-m")?.eq_ignore_ascii_case("zstd"),
            "-o" => a.out = Some(val("-o")?.into()),
            "-y" => a.overwrite = Overwrite::Always,
            "-s" => a.overwrite = Overwrite::Skip,
            "-r" => a.overwrite = Overwrite::Rename,
            "-cp" => {
                a.codepage = match val("-cp")?.to_ascii_lowercase().as_str() {
                    "utf8" | "utf-8" => Codepage::Utf8,
                    "gbk" | "936" => Codepage::Gbk,
                    "big5" | "950" => Codepage::Big5,
                    "sjis" | "932" => Codepage::ShiftJis,
                    "euckr" | "949" => Codepage::EucKr,
                    "cp437" | "437" => Codepage::Cp437,
                    _ => Codepage::Auto,
                }
            }
            "-h" | "--help" => return Err(USAGE.into()),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    if positional.is_empty() {
        return Err(USAGE.into());
    }
    a.archive = positional.remove(0);
    a.inputs = positional;
    Ok(a)
}

/// Run `f` on a worker thread while drawing a progress line.
fn with_progress<T: Send>(progress: &Arc<Progress>, f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    std::thread::scope(|s| {
        let h = s.spawn(f);
        let start = Instant::now();
        while !h.is_finished() {
            std::thread::sleep(Duration::from_millis(200));
            let p = progress.snapshot();
            if p.total_bytes > 0 {
                let pct = p.done_bytes as f64 * 100.0 / p.total_bytes as f64;
                let speed = p.done_bytes as f64 / start.elapsed().as_secs_f64().max(0.001);
                eprint!("\r  {pct:5.1}%  {}/s     ", human_bytes(speed as u64));
            }
        }
        eprint!("\r                              \r");
        h.join().expect("worker panicked")
    })
}

fn open(a: &Args) -> Result<Archive> {
    let opts = OpenOptions { password: a.password.clone(), codepage: a.codepage };
    open_archive(&a.archive, &opts, &Progress::new())
}

fn run(a: Args) -> Result<()> {
    let progress = Arc::new(Progress::new());
    let start = Instant::now();
    match a.cmd.as_str() {
        "a" => {
            if a.inputs.is_empty() {
                return Err(Error::Other("没有要压缩的文件".into()));
            }
            let format = CreateFormat::from_path(&a.archive)
                .ok_or_else(|| Error::Unsupported("请使用 .zip .7z .tar .tar.gz .tar.zst .tar.xz .tar.bz2 扩展名".into()))?;
            let opts = CreateOptions {
                format,
                level: a.level,
                threads: a.threads,
                password: a.password.clone(),
                zip_method: if a.zstd { ZipMethod::Zstd } else { ZipMethod::Deflate },
                ..Default::default()
            };
            with_progress(&progress, || create_archive(&a.archive, &a.inputs, &opts, progress.clone()))?;
            let input = progress.snapshot().total_bytes;
            let output = std::fs::metadata(&a.archive)?.len();
            let secs = start.elapsed().as_secs_f64();
            println!(
                "完成: {} -> {} ({:.1}%)，用时 {secs:.2} 秒，{}/s",
                human_bytes(input),
                human_bytes(output),
                output as f64 * 100.0 / input.max(1) as f64,
                human_bytes((input as f64 / secs) as u64)
            );
        }
        "x" | "t" => {
            let archive = open(&a)?;
            let summary = if a.cmd == "t" {
                with_progress(&progress, || test_archive(&archive, a.password.as_deref(), a.threads, &progress))?
            } else {
                let dest = match &a.out {
                    Some(o) => o.clone(),
                    None => {
                        let parent = std::path::absolute(&a.archive)?.parent().map(PathBuf::from).unwrap_or_default();
                        smart_destination(&archive, &parent)
                    }
                };
                let opts = ExtractOptions {
                    dest: dest.clone(),
                    overwrite: a.overwrite,
                    password: a.password.clone(),
                    threads: a.threads,
                    ..Default::default()
                };
                let s = with_progress(&progress, || extract(&archive, &opts, &progress))?;
                println!("解压到: {}", dest.display());
                s
            };
            let secs = start.elapsed().as_secs_f64();
            let total = archive.total_size();
            println!(
                "完成: {} 个文件，{}{}，用时 {secs:.2} 秒，{}/s",
                summary.files.max(if a.cmd == "t" { archive.entries.iter().filter(|e| !e.is_dir).count() as u64 } else { 0 }),
                human_bytes(total),
                if summary.skipped > 0 { format!("，跳过 {}", summary.skipped) } else { String::new() },
                human_bytes((total as f64 / secs) as u64)
            );
        }
        "l" => {
            let archive = open(&a)?;
            println!("格式: {}{}   条目: {}", archive.kind.label(), if archive.solid { " (固实)" } else { "" }, archive.entries.len());
            println!("{:>14} {:>14}  {:<10} {:<19}  名称", "大小", "压缩后", "方法", "修改时间");
            for e in &archive.entries {
                let t = e
                    .mtime
                    .map(|t| {
                        let d: time::OffsetDateTime = t.into();
                        let d = d.to_offset(time::UtcOffset::local_offset_at(d).unwrap_or(time::UtcOffset::UTC));
                        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", d.year(), d.month() as u8, d.day(), d.hour(), d.minute(), d.second())
                    })
                    .unwrap_or_default();
                println!(
                    "{:>14} {:>14}  {:<10} {:<19}  {}{}{}",
                    if e.is_dir { "<DIR>".to_string() } else { e.size.to_string() },
                    e.packed.map(|p| p.to_string()).unwrap_or_default(),
                    e.method,
                    t,
                    e.name,
                    if e.is_dir { "/" } else { "" },
                    if e.encrypted { " *" } else { "" }
                );
            }
        }
        other => return Err(Error::Other(format!("未知命令 {other}\n\n{USAGE}"))),
    }
    Ok(())
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}
