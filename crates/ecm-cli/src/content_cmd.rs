//! `ecm source show` and `ecm check-content`: a PST message with its files, and
//! target copies compared with the PST body and file by file. Only read.

use std::{collections::HashMap, fs, path::Path};

use ecm_audit::verify::is_parked;
use ecm_model::item::format_utc;
use ecm_store::{SOURCE_INVENTORY, read_inventory, write_csv};

use crate::{EwsArgs, connect, verify_cmd::VERIFIED_INVENTORY};

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
            (
                "rebuildable",
                ecm_pst::content::has_body(&content).to_string(),
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

/// What differs between the source's files and a copy's, matched by name
/// (case-insensitive): files missing from the copy, files whose every LF became
/// CRLF, other differences, and files only the copy has.
fn attachment_issues(source: &[(&str, &[u8])], copy: &[(&str, &[u8])]) -> Vec<String> {
    let mut issues = Vec::new();
    let mut used = vec![false; copy.len()];
    for (name, data) in source {
        let found = copy
            .iter()
            .enumerate()
            .find(|(i, (n, _))| !used[*i] && n.eq_ignore_ascii_case(name));
        match found {
            None => issues.push(format!("missing {name}")),
            Some((i, (_, copied))) => {
                used[i] = true;
                if copied == data {
                    continue;
                }
                if *copied == lf_to_crlf(data).as_slice() {
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

/// Compares every target copy found by Message-ID with its PST message: body
/// lost, and each file attachment. Writes the copies with an issue as CSV.
pub fn check_content_command(
    ews: &EwsArgs,
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
    let target: Vec<_> = read_inventory(&work.join(VERIFIED_INVENTORY))
        .map_err(|e| e.to_string())?
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

    let client = connect(ews)?;
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
            let body_lost = ecm_pst::content::has_body(&content) && copy.body_empty == Some(true);
            // A signed message keeps its signed entity as a file: its copy has the
            // entity's own files instead.
            let signed = item.class.to_lowercase().starts_with("ipm.note.smime");
            let mut issues = Vec::new();
            if !signed && (!content.attachments.is_empty() || copy.has_attachments == Some(true)) {
                let files = client.attachments(&copy.key).map_err(|e| e.to_string())?;
                let ours: Vec<(&str, &[u8])> = content
                    .attachments
                    .iter()
                    .map(|a| (a.name.as_str(), a.data.as_slice()))
                    .collect();
                let theirs: Vec<(&str, &[u8])> = files
                    .iter()
                    .map(|a| (a.name.as_str(), a.content.as_slice()))
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
}
