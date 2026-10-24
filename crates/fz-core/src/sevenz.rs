//! 7z listing and extraction (via sevenz-rust2); writing is in sevenz_write.rs.

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use rayon::prelude::*;
use sevenz_rust2::{BlockDecoder, EncoderMethod, Password};

use crate::error::{Error, IoContext, Result};
use crate::extract::{OFFLOAD_MAX, Target, pump, with_writers};
use crate::progress::Progress;
use crate::read::{Archive, EntryInfo, normalize_name};

fn password_of(pw: Option<&str>) -> Password {
    pw.map(Password::from).unwrap_or_else(Password::empty)
}

fn read_meta(path: &std::path::Path, password: Option<&str>) -> Result<sevenz_rust2::Archive> {
    let mut f = BufReader::new(File::open(path).at(path)?);
    Ok(sevenz_rust2::Archive::read(&mut f, &password_of(password))?)
}

pub(crate) fn list(archive: &mut Archive) -> Result<()> {
    let a = read_meta(&archive.path, archive.password.as_deref())?;
    archive.solid = a.is_solid;
    let block_info: Vec<(String, bool)> = a
        .blocks
        .iter()
        .map(|b| {
            let names: Vec<&str> = b
                .coders
                .iter()
                .rev()
                .filter_map(|c| EncoderMethod::by_id(c.encoder_method_id()).map(|m| m.name()))
                .collect();
            let encrypted = b.coders.iter().any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256);
            (names.join(" "), encrypted)
        })
        .collect();
    archive.entries = a
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let block = a.stream_map.file_block_index.get(i).copied().flatten();
            let (method, encrypted) = block.map(|b| block_info[b].clone()).unwrap_or_default();
            EntryInfo {
                name: normalize_name(f.name()),
                is_dir: f.is_directory(),
                size: f.size(),
                packed: None,
                mtime: f.has_last_modified_date.then(|| f.last_modified_date().into()),
                crc: f.has_crc.then_some(f.crc as u32),
                encrypted,
                method,
            }
        })
        .collect();
    Ok(())
}

pub(crate) fn extract(
    archive: &Archive,
    mask: &[bool],
    target: &Target,
    password: Option<&str>,
    threads: usize,
    progress: &Arc<Progress>,
) -> Result<()> {
    let a = read_meta(&archive.path, password)?;
    if a.files.len() != archive.entries.len() {
        return Err(Error::Corrupt("压缩文件在打开后被修改".into()));
    }
    let pw = password_of(password);
    for (i, f) in a.files.iter().enumerate() {
        if mask[i] && a.stream_map.file_block_index[i].is_none() {
            if f.is_directory() {
                target.make_dir(&archive.entries[i].name)?;
            } else {
                target.write_file(&archive.entries[i].name, &[], archive.entries[i].mtime)?;
                progress.file_done();
            }
        }
    }
    let mut blocks: Vec<usize> = (0..a.blocks.len())
        .filter(|&b| {
            let first = a.stream_map.block_first_file_index[b];
            (first..a.files.len()).take_while(|&i| a.stream_map.file_block_index[i] == Some(b)).any(|i| mask[i])
        })
        .collect();
    if blocks.is_empty() {
        return Ok(());
    }
    if blocks.iter().any(|&b| a.blocks[b].coders.iter().any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256))
        && password.is_none()
    {
        return Err(Error::PasswordRequired);
    }
    blocks.sort_unstable_by_key(|&b| std::cmp::Reverse(a.blocks[b].get_unpack_size()));
    let parallel = blocks.len().min(threads).max(1);
    let per_block = (threads / parallel).max(1) as u32;

    let decode_block = |b: usize| -> Result<()> {
        let mut src = BufReader::with_capacity(1 << 20, File::open(&archive.path).at(&archive.path)?);
        let decoder = BlockDecoder::new(per_block, b, &a, &pw, &mut src);
        let mut next = a.stream_map.block_first_file_index[b];
        let mut buf = vec![0u8; 512 * 1024];
        let writers = if parallel == 1 { threads.min(8) } else { 1 };
        with_writers(target, writers, |w| {
            let mut failure: Option<Error> = None;
            let r = decoder.for_each_entries(&mut |_entry, reader| {
                let i = next;
                next += 1;
                let e = &archive.entries[i];
                let step = (|| -> Result<()> {
                    progress.check()?;
                    if !mask[i] {
                        // Solid stream: still has to be decoded to reach later entries.
                        std::io::copy(reader, &mut std::io::sink()).map_err(crate::error::from_codec_io)?;
                        return Ok(());
                    }
                    progress.set_current(&e.name);
                    if parallel == 1 && e.size <= OFFLOAD_MAX && !target.is_test() {
                        let mut data = Vec::with_capacity(e.size as usize);
                        reader.read_to_end(&mut data).map_err(crate::error::from_codec_io)?;
                        progress.add_bytes(data.len() as u64);
                        w.write(e.name.clone(), data, e.mtime)?;
                    } else if let Some(mut sink) = target.open(&e.name)? {
                        pump(reader, &mut sink, &mut buf, progress)?;
                        sink.finish(e.mtime)?;
                        target.files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        std::io::copy(reader, &mut std::io::sink()).map_err(crate::error::from_codec_io)?;
                    }
                    progress.file_done();
                    Ok(())
                })();
                match step {
                    Ok(()) => Ok(true),
                    Err(err) => {
                        failure = Some(err);
                        Ok(false)
                    }
                }
            });
            if let Some(err) = failure {
                return Err(err);
            }
            match r {
                Ok(_) => Ok(()),
                Err(e) => Err(Error::from(e)),
            }
        })
    };

    if parallel == 1 {
        for b in blocks {
            decode_block(b)?;
        }
        Ok(())
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(parallel)
            .build()
            .map_err(|e| Error::Other(e.to_string()))?;
        pool.install(|| blocks.par_iter().try_for_each(|&b| decode_block(b)))
    }
}
