//! Decoding of legacy (non-UTF-8) entry names, e.g. GBK names written by
//! older Chinese Windows tools into ZIP files.

use encoding_rs::Encoding;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Codepage {
    /// UTF-8 if valid, otherwise the system OEM code page.
    #[default]
    Auto,
    Utf8,
    Gbk,
    Big5,
    ShiftJis,
    EucKr,
    Cp437,
    Cp1252,
}

impl Codepage {
    pub const ALL: [Codepage; 8] = [
        Codepage::Auto,
        Codepage::Utf8,
        Codepage::Gbk,
        Codepage::Big5,
        Codepage::ShiftJis,
        Codepage::EucKr,
        Codepage::Cp437,
        Codepage::Cp1252,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Codepage::Auto => "自动",
            Codepage::Utf8 => "UTF-8",
            Codepage::Gbk => "简体中文 (GBK)",
            Codepage::Big5 => "繁体中文 (Big5)",
            Codepage::ShiftJis => "日语 (Shift_JIS)",
            Codepage::EucKr => "韩语 (EUC-KR)",
            Codepage::Cp437 => "DOS (CP437)",
            Codepage::Cp1252 => "西欧 (Windows-1252)",
        }
    }

    fn encoding(self) -> Option<&'static Encoding> {
        Some(match self {
            Codepage::Auto | Codepage::Cp437 => return None,
            Codepage::Utf8 => encoding_rs::UTF_8,
            Codepage::Gbk => encoding_rs::GB18030,
            Codepage::Big5 => encoding_rs::BIG5,
            Codepage::ShiftJis => encoding_rs::SHIFT_JIS,
            Codepage::EucKr => encoding_rs::EUC_KR,
            Codepage::Cp1252 => encoding_rs::WINDOWS_1252,
        })
    }

    /// The OEM code page Windows tools use for ZIP names on this machine.
    pub fn system_oem() -> Codepage {
        let cp = unsafe { windows_sys::Win32::Globalization::GetOEMCP() };
        match cp {
            936 => Codepage::Gbk,
            950 => Codepage::Big5,
            932 => Codepage::ShiftJis,
            949 => Codepage::EucKr,
            1252 | 850 => Codepage::Cp1252,
            _ => Codepage::Cp437,
        }
    }
}

/// Decode a raw entry name.
pub fn decode(raw: &[u8], cp: Codepage) -> String {
    if raw.is_ascii() {
        // SAFETY-free fast path: ASCII is valid UTF-8.
        return String::from_utf8_lossy(raw).into_owned();
    }
    let cp = match cp {
        Codepage::Auto => match std::str::from_utf8(raw) {
            Ok(s) => return s.to_owned(),
            Err(_) => Codepage::system_oem(),
        },
        other => other,
    };
    match cp.encoding() {
        Some(enc) => enc.decode_without_bom_handling(raw).0.into_owned(),
        None => raw.iter().map(|&b| cp437(b)).collect(),
    }
}

fn cp437(b: u8) -> char {
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    if b < 0x80 { b as char } else { HIGH.chars().nth(usize::from(b - 0x80)).unwrap_or('?') }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gbk_names() {
        let raw = encoding_rs::GBK.encode("中文文件.txt").0.into_owned();
        assert_eq!(decode(&raw, Codepage::Gbk), "中文文件.txt");
        assert_eq!(decode("中文.txt".as_bytes(), Codepage::Auto), "中文.txt");
    }
}
