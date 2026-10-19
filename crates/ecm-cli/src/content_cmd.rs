//! `ecm source show` and `ecm check-content`: a PST message with its files, and
//! target copies compared with the PST body and file by file. Only read.

use std::{collections::HashMap, fs, path::Path};

use ecm_audit::verify::is_parked;
use ecm_model::item::format_utc;
use ecm_store::{SOURCE_INVENTORY, read_inventory, write_csv};

use crate::{
    EwsArgs,
    audit_cmd::{GraphArgs, OUTLOOK_INVENTORY},
    connect,
    verify_cmd::VERIFIED_INVENTORY,
};

/// The issues file `check-content` writes, read back by `plan-pst --damaged`.
pub const DAMAGED_HEADER: [&str; 7] = [
    "target_key",
    "source_key",
    "folder",
    "sent",
    "subject",
    "body_lost",
    "attachments",
];

/// Prints one file: name, type, size, first and last bytes, whether it looks
/// like its type, SHA-256; and saves it into `save` when given.
pub fn print_file(
    name: &str,
    content_type: &str,
    content: &[u8],
    save: Option<&Path>,
) -> Result<(), String> {
    let head: String = content.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let tail: String = content[content.len().saturating_sub(4)..]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!(
        "  file {name:<40} {content_type:<24} {:>9} bytes  starts {head}  ends {tail}  {}  sha256 {}",
        content.len(),
        content_check(name, content),
        sha256_hex(content)
    );
    if let Some(dir) = save {
        let name: String = name
            .chars()
            .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
            .collect();
        let path = dir.join(name);
        fs::create_dir_all(dir)
            .and_then(|()| fs::write(&path, content))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether a file's first bytes match what its extension promises, for common
/// types.
fn content_check(name: &str, content: &[u8]) -> &'static str {
    let lower = name.to_lowercase();
    let expected: &[&[u8]] = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        &[b"\xFF\xD8\xFF"]
    } else if lower.ends_with(".png") {
        &[b"\x89PNG"]
    } else if lower.ends_with(".gif") {
        &[b"GIF8"]
    } else if lower.ends_with(".pdf") {
        &[b"%PDF"]
    } else if lower.ends_with(".zip") || lower.ends_with(".docx") || lower.ends_with(".xlsx") {
        &[b"PK"]
    } else {
        return "type not checked";
    };
    if expected.iter().any(|magic| content.starts_with(magic)) {
        "looks valid"
    } else {
        "DOES NOT LOOK VALID"
    }
}

/// Shows PST messages by inventory key: sender, dates, bodies, files.
pub fn source_show(
    pst: &Path,
    keys: &[String],
    attachments: bool,
    save: Option<&Path>,
) -> Result<(), String> {
    let reader = ecm_pst::PstSource::open(pst).map_err(|e| e.to_string())?;
    for key in keys {
        println!("{key}");
        let content = match reader.message_content(key) {
            Ok(content) => content,
            Err(error) => {
                println!("  error: {error}");
                continue;
            }
        };
        let time = |t: Option<i64>| t.map(format_utc).unwrap_or_default();
        let from = match (&content.sender_name, &content.sender_address) {
            (Some(name), Some(address)) => format!("{name} <{address}>"),
            (name, address) => name.clone().or(address.clone()).unwrap_or_default(),
        };
        let rows = [
            ("subject", content.subject.clone().unwrap_or_default()),
            ("from", from),
            ("message_id", content.message_id.clone().unwrap_or_default()),
            ("sent", time(content.sent)),
            ("received", time(content.received)),
            ("read", format!("{:?}", content.read)),
            ("headers", content.headers.is_some().to_string()),
            (
                "text_length",
                content.text.as_ref().map_or(0, String::len).to_string(),
            ),
            (
                "html_length",
                content.html.as_ref().map_or(0, Vec::len).to_string(),
            ),
            ("body_from_rtf", content.body_from_rtf.to_string()),
            (
                "rebuildable",
                (!ecm_pst::content::loses_body(&content)).to_string(),
            ),
            ("recipients", content.recipients.len().to_string()),
            (
                "attachments",
                content
                    .attachments
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(";"),
            ),
            (
                "skipped_attachments",
                content.skipped_attachments.to_string(),
            ),
        ];
        for (name, value) in rows {
            let value: String = value.chars().take(120).collect();
            println!("  {name:<24} {value}");
        }
        if attachments {
            for file in &content.attachments {
                print_file(
                    &file.name,
                    file.mime_type.as_deref().unwrap_or(""),
                    &file.data,
                    save,
                )?;
            }
        }
    }
    Ok(())
}

/// `data` with every LF byte turned into CRLF, the damage some migrations do to
/// attachments.
fn lf_to_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64);
    for &b in data {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// `data` with every CRLF turned into LF, and without one line end at the end.
fn lf_only(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for (i, &b) in data.iter().enumerate() {
        if !(b == b'\r' && data.get(i + 1) == Some(&b'\n')) {
            out.push(b);
        }
    }
    if out.last() == Some(&b'\n') {
        out.pop();
    }
    out
}

/// Whether `copy` is `source` with its line ends changed, as some migrations do
/// to attachments: every LF turned into CRLF, or only those not after a CR, with
/// maybe a line end added at the end.
fn line_ends_changed(source: &[u8], copy: &[u8]) -> bool {
    copy.len() > source.len() && (copy == lf_to_crlf(source) || lf_only(copy) == lf_only(source))
}

/// What differs between the source's files and a copy's, matched by name
/// (case-insensitive): files missing from the copy, files whose line ends
/// changed, other differences, and files only the copy has.
fn attachment_issues(source: &[(&str, &[u8])], copy: &[(&str, &[u8])]) -> Vec<String> {
    let mut issues = Vec::new();
    let mut used = vec![false; copy.len()];
    // By name first, then a file renamed on the way (a space for an
    // underscore) by its content.
    let mut pairs: Vec<Option<usize>> = Vec::new();
    for (name, _) in source {
        let found = (0..copy.len()).find(|&i| !used[i] && copy[i].0.eq_ignore_ascii_case(name));
        if let Some(i) = found {
            used[i] = true;
        }
        pairs.push(found);
    }
    for (pair, (_, data)) in pairs.iter_mut().zip(source) {
        if pair.is_none() {
            *pair = (0..copy.len())
                .find(|&i| !used[i] && (copy[i].1 == *data || line_ends_changed(data, copy[i].1)));
            if let Some(i) = *pair {
                used[i] = true;
            }
        }
    }
    for (pair, (name, data)) in pairs.into_iter().zip(source) {
        match pair {
            None => issues.push(format!("missing {name}")),
            Some(i) => {
                let copied = &copy[i].1;
                if copied == data {
                    continue;
                }
                if line_ends_changed(data, copied) {
                    issues.push(format!("LF to CRLF {name}"));
                } else {
                    issues.push(format!(
                        "differs {name} ({} -> {} bytes)",
                        data.len(),
                        copied.len()
                    ));
                }
            }
        }
    }
    for (i, (name, _)) in copy.iter().enumerate() {
        if !used[i] {
            issues.push(format!("extra {name}"));
        }
    }
    issues
}

/// Whether a PST message's body shows something: text, or an image in its HTML.
/// An HTML body with neither, as meeting replies have, loses nothing when empty.
fn shows_something(content: &ecm_pst::content::MessageContent) -> bool {
    body_shows(content.text.as_deref(), content.html.as_deref())
}

/// Whether a body shows something: text, or an image or text in its HTML.
fn body_shows(text: Option<&str>, html: Option<&[u8]>) -> bool {
    if text.is_some_and(|t| !t.trim().is_empty()) {
        return true;
    }
    let Some(html) = html else {
        return false;
    };
    let lower = html.to_ascii_lowercase();
    if lower.windows(4).any(|w| w == b"<img") {
        return true;
    }
    // Text outside tags, styles, scripts and comments, other than spaces.
    let mut text = Vec::new();
    let mut in_tag = false;
    let mut skip_until: Option<&[u8]> = None;
    let mut i = 0;
    while i < lower.len() {
        if let Some(end) = skip_until {
            if lower[i..].starts_with(end) {
                skip_until = None;
                i += end.len();
            } else {
                i += 1;
            }
            continue;
        }
        if lower[i..].starts_with(b"<style") {
            skip_until = Some(b"</style>");
        } else if lower[i..].starts_with(b"<script") {
            skip_until = Some(b"</script>");
        } else if lower[i..].starts_with(b"<!--") {
            skip_until = Some(b"-->");
        } else if lower[i] == b'<' {
            in_tag = true;
        } else if lower[i] == b'>' {
            in_tag = false;
        } else if !in_tag {
            text.push(lower[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&text)
        .replace("&nbsp;", " ")
        .chars()
        .any(|c| !c.is_whitespace() && c != '\u{a0}')
}

/// A copy's files, as name and content.
pub type Files = Vec<(String, Vec<u8>)>;

/// Where the copies compared with the PST are read: an Exchange mailbox (EWS) or
/// an Outlook.com one (Graph). Each answers `None` for a copy deleted or moved
/// since the inventory.
pub trait Copies {
    /// The copy's files, as name and content.
    fn files(&self, id: &str) -> Result<Option<Files>, String>;
    /// Whether the copy's body shows something.
    fn body_shows(&self, id: &str) -> Result<Option<bool>, String>;
}

impl<T: ecm_ews::Transport> Copies for ecm_ews::EwsClient<T> {
    fn files(&self, id: &str) -> Result<Option<Files>, String> {
        match self.attachments(id) {
            Ok(files) => Ok(Some(
                files.into_iter().map(|f| (f.name, f.content)).collect(),
            )),
            Err(error) if ews_gone(&error) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn body_shows(&self, id: &str) -> Result<Option<bool>, String> {
        // Exchange gives an empty body as no content at all.
        let properties = match self.item_properties(&[id]) {
            Ok(items) => items.into_iter().next(),
            Err(error) if ews_gone(&error) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        match properties {
            None => Err(format!("{id}: GetItem returned no item")),
            Some(Err(error)) if ews_gone(&error) => Ok(None),
            Some(Err(error)) => Err(error.to_string()),
            Some(Ok(properties)) => Ok(Some(
                properties
                    .get("body_length")
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0)
                    > 0,
            )),
        }
    }
}

impl<T: ecm_graph::Transport> Copies for ecm_graph::GraphClient<T> {
    fn files(&self, id: &str) -> Result<Option<Files>, String> {
        match self.attachments(id) {
            Ok(files) => Ok(Some(
                files.into_iter().map(|f| (f.name, f.content)).collect(),
            )),
            Err(error) if graph_gone(&error) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn body_shows(&self, id: &str) -> Result<Option<bool>, String> {
        // Graph gives an empty HTML body as an HTML skeleton: what it shows decides.
        match self.body(id) {
            Ok((kind, content)) if kind == "html" => {
                Ok(Some(body_shows(None, Some(content.as_bytes()))))
            }
            Ok((_, content)) => Ok(Some(body_shows(Some(&content), None))),
            Err(error) if graph_gone(&error) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Whether an EWS error says the item is no longer there.
fn ews_gone(error: &ecm_ews::Error) -> bool {
    matches!(error, ecm_ews::Error::Response { code, .. } if code == "ErrorItemNotFound")
}

/// Whether a Graph error says the message is no longer there.
fn graph_gone(error: &ecm_graph::Error) -> bool {
    matches!(error, ecm_graph::Error::Graph { status: 404, .. })
}

/// Where `check-content` reads the copies, and which inventory lists them.
pub enum CopiesArgs<'a> {
    /// The target Exchange mailbox, listed in `target.after.jsonl`.
    Ews(&'a EwsArgs),
    /// An Outlook.com mailbox through Graph, listed in `outlook.jsonl`.
    Graph(&'a GraphArgs),
}

/// Compares every copy found by Message-ID with its PST message: body lost,
/// and each file attachment. Writes the copies with an issue as CSV.
pub fn check_content_command(
    copies: &CopiesArgs<'_>,
    work: &Path,
    pst: &Path,
    keys: Option<&Path>,
    out: &Path,
) -> Result<(), String> {
    match copies {
        CopiesArgs::Ews(ews) => {
            let client = connect(ews)?;
            check_content(
                &client,
                &work.join(VERIFIED_INVENTORY),
                work,
                pst,
                keys,
                out,
            )
        }
        #[cfg(windows)]
        CopiesArgs::Graph(graph) => {
            let client = crate::pim_cmd::sign_in(graph)?;
            check_content(&client, &work.join(OUTLOOK_INVENTORY), work, pst, keys, out)
        }
        #[cfg(not(windows))]
        CopiesArgs::Graph(_) => {
            Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
        }
    }
}

fn check_content(
    client: &dyn Copies,
    copy_inventory: &Path,
    work: &Path,
    pst: &Path,
    keys: Option<&Path>,
    out: &Path,
) -> Result<(), String> {
    let mut source = read_inventory(&work.join(SOURCE_INVENTORY)).map_err(|e| e.to_string())?;
    if let Some(keys) = keys {
        let text = fs::read_to_string(keys).map_err(|e| format!("{}: {e}", keys.display()))?;
        let keys: std::collections::HashSet<&str> = text.lines().map(str::trim).collect();
        source.retain(|item| keys.contains(item.key.as_str()));
    }
    let target: Vec<_> = read_inventory(copy_inventory)
        .map_err(|e| format!("{}: {e}", copy_inventory.display()))?
        .into_iter()
        .filter(|item| !is_parked(&item.folder))
        .collect();
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, item) in target.iter().enumerate() {
        if let Some(id) = item.message_id.as_deref().filter(|_| item.is_message()) {
            by_id.entry(id).or_default().push(index);
        }
    }
    let pairs: Vec<(usize, usize)> = source
        .iter()
        .enumerate()
        .filter(|(_, item)| item.is_message())
        .filter_map(|(s, item)| Some((s, by_id.get(item.message_id.as_deref()?)?)))
        .flat_map(|(s, copies)| copies.iter().map(move |&c| (s, c)))
        .collect();

    let before = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    {
        let reader = ecm_pst::PstSource::open(pst).map_err(|e| e.to_string())?;
        for (n, &(s, c)) in pairs.iter().enumerate() {
            if n % 100 == 0 {
                eprint!("\r\x1b[K  {n}/{} copies", pairs.len());
            }
            let (item, copy) = (&source[s], &target[c]);
            let content = match reader.message_content(&item.key) {
                Ok(content) => content,
                Err(error) => {
                    *counts.entry("unreadable in the PST").or_default() += 1;
                    eprintln!("\r\x1b[K  {}: {error}", item.key);
                    continue;
                }
            };
            // The inventory says a body is empty from the server's preview, which
            // is empty too for an HTML body without text (only photos) or not yet
            // previewed: the copy's own body decides. A copy deleted or moved since
            // the inventory is not compared.
            let body_lost = if shows_something(&content) && copy.body_empty == Some(true) {
                match client.body_shows(&copy.key)? {
                    Some(shows) => !shows,
                    None => {
                        *counts.entry("gone since the inventory").or_default() += 1;
                        continue;
                    }
                }
            } else {
                false
            };
            // A signed message keeps its signed entity as a file: its copy has the
            // entity's own files instead.
            let signed = item.class.to_lowercase().starts_with("ipm.note.smime");
            let mut issues = Vec::new();
            if !signed && (!content.attachments.is_empty() || copy.has_attachments == Some(true)) {
                let Some(files) = client.files(&copy.key)? else {
                    *counts.entry("gone since the inventory").or_default() += 1;
                    continue;
                };
                let ours: Vec<(&str, &[u8])> = content
                    .attachments
                    .iter()
                    .map(|a| (a.name.as_str(), a.data.as_slice()))
                    .collect();
                let theirs: Vec<(&str, &[u8])> = files
                    .iter()
                    .map(|(name, content)| (name.as_str(), content.as_slice()))
                    .collect();
                issues = attachment_issues(&ours, &theirs);
            }
            *counts.entry("compared").or_default() += 1;
            if content.skipped_attachments > 0 {
                *counts.entry("PST files left out").or_default() += 1;
            }
            if body_lost {
                *counts.entry("body lost").or_default() += 1;
            }
            if issues.iter().any(|i| i.starts_with("LF to CRLF")) {
                *counts.entry("LF to CRLF").or_default() += 1;
            }
            if issues.iter().any(|i| i.starts_with("differs")) {
                *counts.entry("file differs").or_default() += 1;
            }
            if issues.iter().any(|i| i.starts_with("missing")) {
                *counts.entry("file missing").or_default() += 1;
            }
            if body_lost || issues.iter().any(|i| !i.starts_with("extra")) {
                rows.push(vec![
                    copy.key.clone(),
                    item.key.clone(),
                    copy.folder.clone(),
                    item.sent.map(format_utc).unwrap_or_default(),
                    item.subject.clone().unwrap_or_default(),
                    body_lost.to_string(),
                    issues.join("; "),
                ]);
            }
        }
        eprintln!("\r\x1b[K  {}/{} copies", pairs.len(), pairs.len());
    }
    let after = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    if before != after {
        return Err(format!("{} changed while it was read", pst.display()));
    }
    write_csv(out, &DAMAGED_HEADER, &rows).map_err(|e| e.to_string())?;
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort();
    for (what, n) in counts {
        println!("  {what:<22} {n:>7}");
    }
    println!(
        "{} copies to replace into {}. Nothing was changed; the PST is unchanged.",
        rows.len(),
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_lf_to_crlf_damage_from_other_differences() {
        let source: &[(&str, &[u8])] = &[
            ("a.jpg", b"\xff\xd8\n\x00\n"),
            ("b.pdf", b"%PDF"),
            ("c.txt", b"same"),
            ("gone.doc", b"x"),
        ];
        let copy: &[(&str, &[u8])] = &[
            ("A.JPG", b"\xff\xd8\r\n\x00\r\n"),
            ("b.pdf", b"%PDF-1"),
            ("c.txt", b"same"),
            ("new.png", b"y"),
        ];
        assert_eq!(
            attachment_issues(source, copy),
            [
                "LF to CRLF a.jpg",
                "differs b.pdf (4 -> 6 bytes)",
                "missing gone.doc",
                "extra new.png"
            ]
        );
    }

    #[test]
    fn a_body_with_neither_text_nor_image_shows_nothing() {
        let html = |h: &str| ecm_pst::content::MessageContent {
            html: Some(h.as_bytes().to_vec()),
            ..Default::default()
        };
        assert!(!shows_something(&html(
            "<html><head><style>p { color: red }</style></head><body><p>&nbsp;</p><!-- x --></body></html>"
        )));
        assert!(shows_something(&html("<p>Bonjour</p>")));
        assert!(shows_something(&html(r#"<IMG src="cid:photo">"#)));
        assert!(shows_something(&ecm_pst::content::MessageContent {
            text: Some("Salut".into()),
            ..Default::default()
        }));
    }

    #[test]
    fn finds_a_file_renamed_on_the_way_by_its_content() {
        let source: &[(&str, &[u8])] = &[("Logo_01.png", b"png"), ("a.txt", b"a")];
        let copy: &[(&str, &[u8])] = &[("a.txt", b"a"), ("Logo 01.png", b"png")];
        assert!(attachment_issues(source, copy).is_empty());
        let other: &[(&str, &[u8])] = &[("a.txt", b"a"), ("Logo 01.png", b"gif")];
        assert_eq!(
            attachment_issues(source, other),
            ["missing Logo_01.png", "extra Logo 01.png"]
        );
    }

    #[test]
    fn sees_line_ends_changed_only_where_an_lf_stood_alone() {
        // As found on a real photo: the LFs not after a CR became CRLF, and a line
        // end was added at the end.
        assert!(line_ends_changed(
            b"\xff\xd8\r\n\x01\n\x02",
            b"\xff\xd8\r\n\x01\r\n\x02\r\n"
        ));
        assert!(line_ends_changed(b"a\r\nb", b"a\r\r\nb"));
        assert!(!line_ends_changed(b"a\nb", b"a\nb"));
        assert!(!line_ends_changed(b"a\nb", b"a\r\nc"));
    }
}
