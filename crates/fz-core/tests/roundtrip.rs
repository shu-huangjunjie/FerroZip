use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fz_core::*;

/// A tree with small text files, a binary blob crossing several chunk
/// boundaries, an empty file, an empty folder and non-ASCII names.
fn make_tree(root: &Path) -> PathBuf {
    let src = root.join("源文件夹");
    fs::create_dir_all(src.join("子目录/深层")).unwrap();
    fs::create_dir_all(src.join("空目录")).unwrap();
    for i in 0..200 {
        fs::write(src.join(format!("子目录/文件{i}.txt")), format!("hello {i} ").repeat(i * 13 + 1)).unwrap();
    }
    let big: Vec<u8> = (0..9_500_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8 & if i % 3 == 0 { 0xff } else { 0x0f }).collect();
    fs::write(src.join("子目录/深层/big.bin"), &big).unwrap();
    fs::write(src.join("empty.dat"), b"").unwrap();
    fs::write(src.join("photo.jpg"), vec![7u8; 3000]).unwrap();
    src
}

fn assert_same_tree(a: &Path, b: &Path) {
    let list = |root: &Path| {
        let mut v: Vec<(String, Option<Vec<u8>>)> = walkdir_like(root)
            .into_iter()
            .map(|p| {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                let data = p.is_file().then(|| fs::read(&p).unwrap());
                (rel, data)
            })
            .collect();
        v.sort();
        v
    };
    let (la, lb) = (list(a), list(b));
    assert_eq!(la.len(), lb.len(), "entry count differs");
    for (x, y) in la.iter().zip(&lb) {
        assert_eq!(x.0, y.0);
        assert!(x.1 == y.1, "content differs: {}", x.0);
    }
}

fn walkdir_like(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out
}

fn roundtrip(opts: CreateOptions, file: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let archive_path = tmp.path().join(file);
    create_archive(&archive_path, &[src.clone()], &opts, Arc::new(Progress::new())).unwrap();

    let open = OpenOptions { password: opts.password.clone(), ..Default::default() };
    let archive = open_archive(&archive_path, &open, &Progress::new()).unwrap();
    assert!(archive.entries.iter().any(|e| e.name == "源文件夹/子目录/深层/big.bin"), "{file}: {:?} {:?}", archive.kind, archive.entries.iter().map(|e| &e.name).filter(|n| !n.contains("文件")).collect::<Vec<_>>());
    assert!(archive.entries.iter().any(|e| e.name == "源文件夹/空目录" && e.is_dir));

    test_archive(&archive, opts.password.as_deref(), 0, &Arc::new(Progress::new())).unwrap();

    let out = tmp.path().join("out");
    let x = ExtractOptions { dest: out.clone(), password: opts.password.clone(), ..Default::default() };
    extract(&archive, &x, &Arc::new(Progress::new())).unwrap();
    assert_same_tree(&src, &out.join("源文件夹"));
}

#[test]
fn zip_deflate() {
    roundtrip(CreateOptions::default(), "a.zip");
}

#[test]
fn zip_store_and_max() {
    roundtrip(CreateOptions { level: 0, ..Default::default() }, "s.zip");
    roundtrip(CreateOptions { level: 9, threads: 3, ..Default::default() }, "m.zip");
}

#[test]
fn zip_zstd() {
    roundtrip(CreateOptions { zip_method: ZipMethod::Zstd, ..Default::default() }, "z.zip");
}

#[test]
fn zip_aes() {
    roundtrip(CreateOptions { password: Some("密码pass".into()), ..Default::default() }, "e.zip");
}

#[test]
fn zip_wrong_password() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let p = tmp.path().join("e.zip");
    let opts = CreateOptions { password: Some("right".into()), ..Default::default() };
    create_archive(&p, &[src], &opts, Arc::new(Progress::new())).unwrap();
    let archive = open_archive(&p, &OpenOptions::default(), &Progress::new()).unwrap();
    assert!(archive.has_encrypted());
    let x = ExtractOptions { dest: tmp.path().join("o"), ..Default::default() };
    assert!(matches!(extract(&archive, &x, &Arc::new(Progress::new())), Err(Error::PasswordRequired)));
    let x = ExtractOptions { dest: tmp.path().join("o"), password: Some("wrong".into()), ..Default::default() };
    assert!(matches!(extract(&archive, &x, &Arc::new(Progress::new())), Err(Error::WrongPassword)));
}

#[test]
fn sevenz() {
    roundtrip(CreateOptions { format: CreateFormat::SevenZ, level: 3, ..Default::default() }, "a.7z");
}

#[test]
fn sevenz_copy() {
    roundtrip(CreateOptions { format: CreateFormat::SevenZ, level: 0, ..Default::default() }, "c.7z");
}

#[test]
fn sevenz_encrypted() {
    roundtrip(CreateOptions { format: CreateFormat::SevenZ, level: 1, password: Some("secret".into()), ..Default::default() }, "e.7z");
}

#[test]
fn tars() {
    for f in [CreateFormat::Tar, CreateFormat::TarGz, CreateFormat::TarZst, CreateFormat::TarXz, CreateFormat::TarBz2] {
        roundtrip(CreateOptions { format: f, level: 3, ..Default::default() }, &format!("a.{}", f.extension()));
    }
}

#[test]
fn partial_extract_with_strip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let p = tmp.path().join("a.zip");
    create_archive(&p, &[src], &CreateOptions::default(), Arc::new(Progress::new())).unwrap();
    let archive = open_archive(&p, &OpenOptions::default(), &Progress::new()).unwrap();
    let idx = archive.entries.iter().position(|e| e.name == "源文件夹/子目录/深层").unwrap();
    let x = ExtractOptions {
        dest: tmp.path().join("o"),
        selection: Some(vec![idx]),
        strip_prefix: "源文件夹/子目录/".into(),
        ..Default::default()
    };
    extract(&archive, &x, &Arc::new(Progress::new())).unwrap();
    assert!(tmp.path().join("o/深层/big.bin").is_file());
    assert!(!tmp.path().join("o/文件1.txt").exists());
}

#[test]
fn smart_dest() {
    let tmp = tempfile::tempdir().unwrap();
    let src = make_tree(tmp.path());
    let p = tmp.path().join("a.zip");
    create_archive(&p, &[src], &CreateOptions::default(), Arc::new(Progress::new())).unwrap();
    let archive = open_archive(&p, &OpenOptions::default(), &Progress::new()).unwrap();
    let parent = tmp.path().join("dest");
    fs::create_dir_all(&parent).unwrap();
    // One top-level folder: extract right here.
    assert_eq!(smart_destination(&archive, &parent), parent);
}

/// Files above the 7z block size are split into LZMA2 pieces joined at
/// dictionary resets; with a password the AES-CBC stream runs across pieces.
#[test]
fn sevenz_large_file_pieces() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big");
    fs::create_dir_all(&src).unwrap();
    let data: Vec<u8> = (0..70_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 21) as u8 & 0x3f).collect();
    fs::write(src.join("large.bin"), &data).unwrap();
    fs::write(src.join("clip.mp4"), &data[..40_000_000]).unwrap();
    fs::write(src.join("small.txt"), b"small").unwrap();
    for password in [None, Some("pw")] {
        let p = tmp.path().join("big.7z");
        let opts = CreateOptions { format: CreateFormat::SevenZ, level: 1, password: password.map(String::from), ..Default::default() };
        create_archive(&p, &[src.clone()], &opts, Arc::new(Progress::new())).unwrap();
        let open = OpenOptions { password: opts.password.clone(), ..Default::default() };
        let archive = open_archive(&p, &open, &Progress::new()).unwrap();
        let out = tmp.path().join("o");
        let _ = fs::remove_dir_all(&out);
        let x = ExtractOptions { dest: out.clone(), password: opts.password.clone(), ..Default::default() };
        extract(&archive, &x, &Arc::new(Progress::new())).unwrap();
        assert!(fs::read(out.join("big/large.bin")).unwrap() == data);
        assert!(fs::read(out.join("big/clip.mp4")).unwrap()[..] == data[..40_000_000]);
        assert_eq!(fs::read(out.join("big/small.txt")).unwrap(), b"small");
    }
}
