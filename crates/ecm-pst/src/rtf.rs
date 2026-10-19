//! Bodies kept as compressed RTF (`PR_RTF_COMPRESSED`): decompressed with
//! Microsoft's `compressed-rtf`, then de-encapsulated as [MS-OXRTFEX] specifies
//! (the HTML or plain text a client wrapped in RTF), or turned into plain text
//! when the RTF is the original.

use std::collections::HashMap;

/// A body recovered from RTF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// The HTML the RTF encapsulated.
    Html(String),
    /// Plain text: encapsulated, or the text of an original RTF document.
    Text(String),
}

/// What the RTF holds, from its first tokens ([MS-OXRTFEX] 2.2.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Html,
    Text,
    Rtf,
}

/// The body of a `PR_RTF_COMPRESSED` value; `None` when it does not decompress
/// or holds no text.
pub fn body(compressed: &[u8]) -> Option<Body> {
    // The header alone is 16 bytes; `decompress_rtf` panics on less.
    if compressed.len() < 16 {
        return None;
    }
    let rtf = compressed_rtf::decompress_rtf(compressed).ok()?;
    let rtf = rtf.as_bytes();
    if !rtf.starts_with(b"{\\rtf1") {
        return None;
    }
    let kind = kind(rtf);
    let out = convert(rtf, kind);
    if out.trim().is_empty() {
        return None;
    }
    Some(match kind {
        Kind::Html => Body::Html(out),
        Kind::Text | Kind::Rtf => Body::Text(out),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Open,
    Close,
    Word(&'a str, Option<i32>),
    Symbol(u8),
    Hex(u8),
    Text(u8),
}

/// RTF tokens; `\binN` data is skipped, line ends between tokens dropped.
struct Tokens<'a> {
    rtf: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        loop {
            let b = *self.rtf.get(self.at)?;
            self.at += 1;
            return Some(match b {
                b'{' => Token::Open,
                b'}' => Token::Close,
                b'\r' | b'\n' => continue,
                b'\\' => {
                    let c = *self.rtf.get(self.at)?;
                    if c.is_ascii_alphabetic() {
                        let start = self.at;
                        while self.rtf.get(self.at).is_some_and(u8::is_ascii_alphabetic) {
                            self.at += 1;
                        }
                        let name = std::str::from_utf8(&self.rtf[start..self.at]).unwrap_or("");
                        let digits = self.at;
                        if self.rtf.get(self.at) == Some(&b'-') {
                            self.at += 1;
                        }
                        while self.rtf.get(self.at).is_some_and(u8::is_ascii_digit) {
                            self.at += 1;
                        }
                        let param = std::str::from_utf8(&self.rtf[digits..self.at])
                            .ok()
                            .and_then(|p| p.parse().ok());
                        if self.rtf.get(self.at) == Some(&b' ') {
                            self.at += 1;
                        }
                        if name == "bin" {
                            let skip = usize::try_from(param.unwrap_or(0)).unwrap_or(0);
                            self.at = (self.at + skip).min(self.rtf.len());
                            continue;
                        }
                        Token::Word(name, param)
                    } else if c == b'\'' {
                        let hex = self.rtf.get(self.at + 1..self.at + 3)?;
                        self.at += 3;
                        let hex = std::str::from_utf8(hex).ok()?;
                        Token::Hex(u8::from_str_radix(hex, 16).ok()?)
                    } else {
                        self.at += 1;
                        Token::Symbol(c)
                    }
                }
                b => Token::Text(b),
            });
        }
    }
}

fn tokens(rtf: &[u8]) -> Tokens<'_> {
    Tokens { rtf, at: 0 }
}

/// Encapsulated HTML or text when one of the first ten tokens, all group starts
/// or control words, says so.
fn kind(rtf: &[u8]) -> Kind {
    for token in tokens(rtf).take(10) {
        match token {
            Token::Word("fromhtml", Some(1)) => return Kind::Html,
            Token::Word("fromtext", _) => return Kind::Text,
            Token::Open | Token::Word(..) => {}
            _ => return Kind::Rtf,
        }
    }
    Kind::Rtf
}

/// Destinations that hold no text of the body.
const SKIPPED: &[&str] = &[
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "object",
    "objdata",
    "header",
    "headerl",
    "headerr",
    "headerf",
    "footer",
    "footerl",
    "footerr",
    "footerf",
    "fldinst",
    "listtable",
    "listoverridetable",
    "revtbl",
    "rsidtbl",
    "filetbl",
    "xmlnstbl",
    "themedata",
    "colorschememapping",
    "datastore",
    "latentstyles",
];

#[derive(Debug, Clone, Copy, Default)]
struct Group {
    /// A destination with nothing to copy.
    skip: bool,
    /// The font table.
    fonts: bool,
    /// An `\*\htmltag` destination.
    tag: bool,
    /// Text the encapsulation marked as RTF only (`\htmlrtf`).
    htmlrtf: bool,
    /// Characters standing in for each `\uN` (`\ucN`).
    uc: usize,
    /// The current font.
    font: Option<i32>,
    /// No token read yet in this group: the next one may name a destination.
    first: bool,
    /// After `\*`: the next control word names an ignorable destination.
    star: bool,
}

/// Text with the bytes of `\'hh` escapes and raw text decoded in their code page.
struct Out {
    text: String,
    bytes: Vec<u8>,
    code_page: u32,
}

impl Out {
    fn byte(&mut self, b: u8, code_page: u32) {
        if code_page != self.code_page {
            self.flush();
            self.code_page = code_page;
        }
        self.bytes.push(b);
    }

    fn push(&mut self, s: &str) {
        self.flush();
        self.text.push_str(s);
    }

    fn flush(&mut self) {
        if !self.bytes.is_empty() {
            let (text, _, _) = encoding(self.code_page).decode(&self.bytes);
            self.text.push_str(&text);
            self.bytes.clear();
        }
    }
}

/// The encoding of a Windows code page; Western European when unknown.
pub(crate) fn encoding(code_page: u32) -> &'static encoding_rs::Encoding {
    use encoding_rs::*;
    match code_page {
        65001 => UTF_8,
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        10000 => MACINTOSH,
        28592 => ISO_8859_2,
        28605 => ISO_8859_15,
        _ => WINDOWS_1252,
    }
}

/// The code page of an RTF `\fcharsetN`; `None` for the default.
fn charset_code_page(charset: i32) -> Option<u32> {
    Some(match charset {
        0 => 1252,
        77 => 10000,
        128 => 932,
        129 => 949,
        134 => 936,
        136 => 950,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        _ => return None,
    })
}

fn convert(rtf: &[u8], kind: Kind) -> String {
    let mut out = Out {
        text: String::new(),
        bytes: Vec::new(),
        code_page: 1252,
    };
    let mut default_code_page = 1252;
    let mut font_code_pages: HashMap<i32, u32> = HashMap::new();
    let mut font_defined: Option<i32> = None;
    let mut stack: Vec<Group> = Vec::new();
    let mut group = Group {
        uc: 1,
        ..Group::default()
    };
    // Characters still to skip after a `\uN`.
    let mut pending_skip = 0usize;

    for token in tokens(rtf) {
        let first = std::mem::replace(&mut group.first, false);
        let star = std::mem::replace(&mut group.star, false);
        match token {
            Token::Open => {
                stack.push(group);
                group.first = true;
                pending_skip = 0;
                continue;
            }
            Token::Close => {
                group = stack.pop().unwrap_or_default();
                pending_skip = 0;
                continue;
            }
            Token::Symbol(b'*') if first => {
                group.star = true;
                continue;
            }
            Token::Word(name, _) if star => {
                if name == "htmltag" && kind == Kind::Html && !group.skip {
                    group.tag = true;
                } else {
                    group.skip = true;
                }
                continue;
            }
            Token::Word("fonttbl", _) if first => {
                group.fonts = true;
                continue;
            }
            Token::Word(name, _) if first && SKIPPED.contains(&name) => {
                group.skip = true;
                continue;
            }
            _ => {}
        }
        if group.skip {
            continue;
        }
        if group.fonts {
            match token {
                Token::Word("f", n) => font_defined = n,
                Token::Word("fcharset", Some(charset)) => {
                    if let (Some(font), Some(cp)) = (font_defined, charset_code_page(charset)) {
                        font_code_pages.insert(font, cp);
                    }
                }
                Token::Word("cpg", Some(cp)) => {
                    if let (Some(font), Ok(cp)) = (font_defined, u32::try_from(cp)) {
                        font_code_pages.insert(font, cp);
                    }
                }
                _ => {}
            }
            continue;
        }
        // Header and state words count even where text is not copied.
        match token {
            Token::Word("ansicpg", Some(cp)) => {
                default_code_page = u32::try_from(cp).unwrap_or(1252);
                continue;
            }
            Token::Word("f", n) => {
                group.font = n;
                continue;
            }
            Token::Word("uc", Some(n)) => {
                group.uc = usize::try_from(n).unwrap_or(1);
                continue;
            }
            Token::Word("htmlrtf", n) => {
                group.htmlrtf = n != Some(0);
                continue;
            }
            _ => {}
        }
        let copy = match kind {
            Kind::Rtf => true,
            Kind::Html => group.tag || !group.htmlrtf,
            Kind::Text => !group.htmlrtf,
        };
        if !copy {
            continue;
        }
        let code_page = if group.tag {
            default_code_page
        } else {
            group
                .font
                .and_then(|f| font_code_pages.get(&f).copied())
                .unwrap_or(default_code_page)
        };
        match token {
            Token::Text(b) | Token::Hex(b) => {
                if pending_skip > 0 {
                    pending_skip -= 1;
                } else {
                    out.byte(b, code_page);
                }
            }
            Token::Word("u", Some(n)) => {
                let n = if n < 0 { n + 65536 } else { n };
                if let Some(c) = u32::try_from(n).ok().and_then(char::from_u32) {
                    out.push(c.encode_utf8(&mut [0; 4]));
                }
                pending_skip = group.uc;
            }
            Token::Word("par" | "line", _) | Token::Symbol(b'\n' | b'\r') => out.push("\r\n"),
            Token::Word("row" | "sect" | "page", _) if kind == Kind::Rtf => out.push("\r\n"),
            Token::Word("tab", _) => out.push("\t"),
            Token::Word("cell", _) if kind == Kind::Rtf => out.push("\t"),
            Token::Word("lquote", _) => out.push("\u{2018}"),
            Token::Word("rquote", _) => out.push("\u{2019}"),
            Token::Word("ldblquote", _) => out.push("\u{201C}"),
            Token::Word("rdblquote", _) => out.push("\u{201D}"),
            Token::Word("bullet", _) => out.push("\u{2022}"),
            Token::Word("endash", _) => out.push("\u{2013}"),
            Token::Word("emdash", _) => out.push("\u{2014}"),
            Token::Symbol(b'~') => out.push("\u{A0}"),
            Token::Symbol(b @ (b'\\' | b'{' | b'}')) => out.byte(b, code_page),
            _ => {}
        }
    }
    out.flush();
    out.text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn de_encapsulates_html() {
        let rtf = br#"{\rtf1\ansi\ansicpg1252\fromhtml1 \deff0{\fonttbl
{\f0\fswiss Arial;}}
{\colortbl\red0\green0\blue0;}
{\*\htmltag19 <html>}{\*\htmltag34 <head>}{\*\htmltag41 <body>}
\htmlrtf {\htmlrtf0 {\*\htmltag64 <p>}Caf\'e9 \{ok\}\u8364?\htmlrtf\par\htmlrtf0 {\*\htmltag72 </p>}}\htmlrtf}
{\*\mhtmltag84 <img src="cid:x">}{\*\htmltag84 <img src="cid:y">}
{\*\htmltag49 </body>}{\*\htmltag27 </html>}}"#;
        assert_eq!(kind(rtf), Kind::Html);
        assert_eq!(
            convert(rtf, Kind::Html),
            "<html><head><body><p>Café {ok}€</p><img src=\"cid:y\"></body></html>"
        );
    }

    #[test]
    fn de_encapsulates_text_and_reads_plain_rtf() {
        let text = br#"{\rtf1\ansi\ansicpg1252\fromtext \deff0{\fonttbl{\f0\fswiss Arial;}}\pard\plain\f0 Bonjour,\par \tab \'e0 bient\'f4t\par}"#;
        assert_eq!(kind(text), Kind::Text);
        assert_eq!(convert(text, Kind::Text), "Bonjour,\r\n\tà bientôt\r\n");

        let rtf = br#"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\froman\fcharset0 Times;}{\f1\fcharset204 Arial Cyr;}}{\*\generator Word;}{\info{\author X}}\pard\f0 Joyeux anniversaire !\par{\f1 \'cf\'f0\'e8\'e2\'e5\'f2}\par}"#;
        assert_eq!(kind(rtf), Kind::Rtf);
        assert_eq!(
            convert(rtf, Kind::Rtf),
            "Joyeux anniversaire !\r\nПривет\r\n"
        );
    }

    #[test]
    fn a_body_round_trips_through_compression() {
        let rtf = r"{\rtf1\ansi\ansicpg1252\fromtext \deff0{\fonttbl{\f0 Arial;}}\f0 Salut\par}";
        let compressed = compressed_rtf::compress_rtf(rtf).unwrap();
        assert_eq!(body(&compressed), Some(Body::Text("Salut\r\n".into())));
        assert_eq!(body(b"not rtf"), None);
    }
}
