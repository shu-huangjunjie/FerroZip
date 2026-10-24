//! Print the folder/coder structure of a .7z archive (diagnostics).
//!
//!   cargo run --release -p fz-core --example sz7info -- archive.7z [password]

use std::io::BufReader;

use sevenz_rust2::{Archive, EncoderMethod, Password};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pw = args.get(2).map(|p| Password::from(p.as_str())).unwrap_or_else(Password::empty);
    let mut f = BufReader::new(std::fs::File::open(&args[1]).unwrap());
    let a = Archive::read(&mut f, &pw).unwrap();
    println!("files {}, folders {}, solid {}", a.files.len(), a.blocks.len(), a.is_solid);
    let mut sizes: Vec<u64> = a.blocks.iter().map(|b| b.get_unpack_size()).collect();
    for (i, b) in a.blocks.iter().enumerate().take(5) {
        let coders: Vec<String> = b
            .coders
            .iter()
            .map(|c| {
                let name = EncoderMethod::by_id(c.encoder_method_id()).map(|m| m.name().to_string()).unwrap_or_else(|| format!("{:02x?}", c.encoder_method_id()));
                format!("{name}{:02x?}", c.properties())
            })
            .collect();
        println!("folder {i}: {} -> {} bytes", coders.join(" + "), b.get_unpack_size());
    }
    // Walk the LZMA2 chunk headers of the first folder's packed stream and
    // report where the dictionary is reset.
    if let Some(b) = a.blocks.first() {
        if b.coders.len() == 1 && b.coders[0].encoder_method_id() == EncoderMethod::ID_LZMA2 {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&args[1]).unwrap();
            f.seek(SeekFrom::Start(32 + a.pack_pos())).unwrap();
            let mut data = vec![0u8; a.pack_sizes()[0] as usize];
            f.read_exact(&mut data).unwrap();
            let (mut i, mut unpacked, mut resets, mut chunks) = (0usize, 0u64, Vec::new(), 0u64);
            let mut kinds = [0u64; 4];
            while i < data.len() {
                let c = data[i];
                if c == 0 {
                    break;
                }
                chunks += 1;
                if c == 1 || c == 2 {
                    let n = ((data[i + 1] as usize) << 8 | data[i + 2] as usize) + 1;
                    if c == 1 {
                        resets.push(unpacked);
                    }
                    unpacked += n as u64;
                    i += 3 + n;
                } else {
                    let u = (((c & 0x1F) as u64) << 16 | (data[i + 1] as u64) << 8 | data[i + 2] as u64) + 1;
                    let p = ((data[i + 3] as usize) << 8 | data[i + 4] as usize) + 1;
                    let mode = (c >> 5) & 3;
                    kinds[mode as usize] += 1;
                    if mode == 3 {
                        resets.push(unpacked);
                    }
                    unpacked += u;
                    i += 5 + usize::from(mode >= 2) + p;
                }
            }
            println!("LZMA2 chunks {chunks}, kinds (none/state/state+props/dict) {kinds:?}, dict resets {}", resets.len());
            let gaps: Vec<u64> = resets.windows(2).map(|w| (w[1] - w[0]) >> 20).collect();
            println!("dict reset spacing (MiB): {:?}", &gaps[..gaps.len().min(12)]);
        }
    }
    sizes.sort_unstable();
    if let (Some(min), Some(max)) = (sizes.first(), sizes.last()) {
        println!("folder sizes: min {min}, max {max}");
    }
}
