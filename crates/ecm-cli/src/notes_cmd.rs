//! `ecm plan-notes`: plan copying the PST's sticky notes into the target's Notes
//! folder. Graph has no API for notes, and MIME cannot carry them.

use std::{collections::HashMap, fs, path::Path};

use ecm_audit::plan::{Action, counts};
use ecm_ews::xml::escape;
use ecm_store::{SOURCE_INVENTORY, read_inventory};

use crate::{EwsArgs, connect};

/// `PSETID_Note`, the property set of sticky note properties.
const PSETID_NOTE: &str = "0006200E-0000-0000-C000-000000000046";
/// `PidLidNoteColor` (0x8B00) value for yellow, the default.
const NOTE_COLOR_YELLOW: u32 = 3;

/// Writes the plan. Reads the PST, read-only, and checks it did not change.
pub fn plan_notes_command(
    ews: &EwsArgs,
    work: &Path,
    pst: &Path,
    out: &Path,
) -> Result<(), String> {
    let notes: Vec<_> = read_inventory(&work.join(SOURCE_INVENTORY))
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|item| item.class.starts_with("IPM.StickyNote"))
        .collect();

    let client = connect(ews)?;
    let folder = client
        .get_distinguished_folder("notes")
        .map_err(|e| e.to_string())?;
    let tree = client
        .folder_tree("msgfolderroot")
        .map_err(|e| e.to_string())?;
    let paths: HashMap<String, String> = tree
        .with_paths()
        .into_iter()
        .map(|(path, f)| (f.id.clone(), path))
        .collect();
    let to = paths
        .get(&folder.id)
        .cloned()
        .ok_or("the Notes folder is not under the mailbox top")?;
    let existing: Vec<String> = client
        .inventory_items(&folder.id, &to)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|item| item.subject.map(|s| s.to_lowercase()))
        .collect();

    let before = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    let mut actions = Vec::new();
    let mut left_out = Vec::new();
    {
        let reader = ecm_pst::PstSource::open(pst).map_err(|e| e.to_string())?;
        for note in &notes {
            let subject = note.subject.clone().unwrap_or_default();
            if existing.contains(&subject.to_lowercase()) {
                left_out.push(format!("{subject:?}: already in the target"));
                continue;
            }
            match reader.message_content(&note.key) {
                Ok(content) => actions.push(Action::Create {
                    kind: "note".into(),
                    to: to.clone(),
                    subject: Some(subject.clone()),
                    item: note_xml(&subject, &note_body(&content)),
                }),
                Err(error) => left_out.push(format!("{subject:?}: {error}")),
            }
        }
    }
    let after = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    if before != after {
        return Err(format!("{} changed while it was read", pst.display()));
    }

    let mut text = String::new();
    for action in &actions {
        text.push_str(&serde_json::to_string(action).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "{} notes in the PST. Plan: {} steps into {}",
        notes.len(),
        actions.len(),
        out.display()
    );
    for (kind, n) in counts(&actions) {
        println!("  {kind:<14} {n:>7}");
    }
    for line in &left_out {
        println!("  left out: {line}");
    }
    println!(
        "Nothing was changed; the PST is unchanged. Review the plan, then run `ecm apply --plan {} --dry-run`.",
        out.display()
    );
    Ok(())
}

/// A note's body: its text, or its HTML when it has no text (notes synced from
/// Outlook.com keep only HTML).
pub enum NoteBody {
    /// Plain text.
    Text(String),
    /// HTML.
    Html(String),
}

fn note_body(content: &ecm_pst::content::MessageContent) -> NoteBody {
    match (&content.text, &content.html) {
        (Some(text), _) if !text.trim().is_empty() => NoteBody::Text(text.clone()),
        (_, Some(html)) => NoteBody::Html(match content.code_page {
            // Windows-1252 and Latin-1 bytes map to the same code points, the
            // C1 range aside, which notes do not use.
            Some(1252 | 28591) => html.iter().map(|&b| char::from(b)).collect(),
            _ => String::from_utf8_lossy(html).into_owned(),
        }),
        _ => NoteBody::Text(String::new()),
    }
}

/// The EWS item for a sticky note: class, subject, body and a yellow colour.
pub fn note_xml(subject: &str, body: &NoteBody) -> String {
    let (kind, text) = match body {
        NoteBody::Text(text) => ("Text", text),
        NoteBody::Html(html) => ("HTML", html),
    };
    format!(
        r#"<t:Item><t:ItemClass>IPM.StickyNote</t:ItemClass><t:Subject>{}</t:Subject><t:Body BodyType="{kind}">{}</t:Body><t:ExtendedProperty><t:ExtendedFieldURI PropertySetId="{PSETID_NOTE}" PropertyId="35584" PropertyType="Integer"/><t:Value>{NOTE_COLOR_YELLOW}</t:Value></t:ExtendedProperty></t:Item>"#,
        escape(subject),
        escape(text)
    )
}

#[cfg(test)]
mod tests {
    use super::{NoteBody, note_xml};

    #[test]
    fn writes_a_yellow_sticky_note() {
        assert_eq!(
            note_xml("Codes & co", &NoteBody::Text("A <B>".into())),
            concat!(
                "<t:Item><t:ItemClass>IPM.StickyNote</t:ItemClass><t:Subject>Codes &amp; co</t:Subject>",
                r#"<t:Body BodyType="Text">A &lt;B&gt;</t:Body>"#,
                r#"<t:ExtendedProperty><t:ExtendedFieldURI PropertySetId="0006200E-0000-0000-C000-000000000046" PropertyId="35584" PropertyType="Integer"/><t:Value>3</t:Value></t:ExtendedProperty></t:Item>"#
            )
        );
    }

    #[test]
    fn keeps_an_html_body() {
        assert!(
            note_xml("N", &NoteBody::Html("<p>x</p>".into()))
                .contains(r#"<t:Body BodyType="HTML">&lt;p&gt;x&lt;/p&gt;</t:Body>"#)
        );
    }
}
