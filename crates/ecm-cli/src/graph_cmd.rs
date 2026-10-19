//! `ecm fidelity-graph`: re-create a few PST messages in the `ecm-test` folder of
//! a mailbox through Microsoft Graph, two ways, and compare each copy with its
//! reference: a copy known to be good (the Exchange target's), and the PST's files.
//!
//! It only creates items, in [`TEST_FOLDER`]; nothing else in the mailbox is
//! changed.

use std::{collections::HashSet, fs, path::Path};

use ecm_model::{compare::PropertyMap, item::format_iso};
use ecm_pst::content::{MessageContent, RecipientKind};
use ecm_store::read_inventory;
use serde_json::{Value, json};

use crate::{
    EwsArgs, audit_cmd::GraphArgs, connect, fidelity_cmd::TEST_FOLDER,
    verify_cmd::VERIFIED_INVENTORY,
};

/// `MSGFLAG_READ`. `MSGFLAG_UNSENT` is never set, so messages are not drafts.
const MSGFLAG_READ: i64 = 0x1;

/// Above this, a message goes without its files, which follow one by one: Graph
/// takes requests of about 4 MB, and base64 makes the MIME a third larger.
const MAX_REQUEST: usize = 2 * 1024 * 1024;

/// A message as Graph's JSON resource, with what only creation can set: the
/// message flags (read, and not a draft), the send and receive times, the
/// Internet headers. Files go separately.
pub fn graph_message(content: &MessageContent, sender: Option<&str>) -> Value {
    let address = |name: Option<&String>, address: Option<&String>| {
        let mut email = json!({ "address": address.cloned().unwrap_or_default() });
        if let Some(name) = name {
            email["name"] = json!(name);
        }
        json!({ "emailAddress": email })
    };
    let recipients = |kind: RecipientKind| {
        content
            .recipients
            .iter()
            .filter(|r| r.kind == kind && r.address.is_some())
            .map(|r| address(r.name.as_ref(), r.address.as_ref()))
            .collect::<Vec<_>>()
    };
    let (body_type, body) = match ecm_pst::content::html_text(content) {
        Some(html) => ("html", html),
        None => ("text", content.text.clone().unwrap_or_default()),
    };
    let mut message = json!({
        "subject": content.subject.clone().unwrap_or_default(),
        "body": { "contentType": body_type, "content": body },
        "toRecipients": recipients(RecipientKind::To),
        "ccRecipients": recipients(RecipientKind::Cc),
        "bccRecipients": recipients(RecipientKind::Bcc),
    });
    let from = match (&content.sender_address, sender) {
        (Some(_), _) => Some(address(
            content.sender_name.as_ref(),
            content.sender_address.as_ref(),
        )),
        (None, Some(mailbox)) => {
            let (name, addr) = match mailbox.rsplit_once('<') {
                Some((name, addr)) => (
                    Some(name.trim().to_owned()),
                    addr.trim_end_matches('>').to_owned(),
                ),
                None => (None, mailbox.to_owned()),
            };
            Some(address(name.as_ref(), Some(&addr)))
        }
        (None, None) => None,
    };
    if let Some(from) = from {
        message["from"] = from.clone();
        message["sender"] = from;
    }
    if let Some(id) = &content.message_id {
        message["internetMessageId"] = json!(id);
    }
    let flags = if content.read.unwrap_or(true) {
        MSGFLAG_READ
    } else {
        0
    };
    let mut extended = vec![json!({ "id": "Integer 0x0E07", "value": flags.to_string() })];
    if let Some(received) = content.received {
        extended.push(json!({ "id": "SystemTime 0x0E06", "value": format_iso(received) }));
    }
    if let Some(sent) = content.sent {
        extended.push(json!({ "id": "SystemTime 0x0039", "value": format_iso(sent) }));
    }
    if let Some(headers) = &content.headers {
        extended.push(json!({ "id": "String 0x007D", "value": headers }));
    }
    message["singleValueExtendedProperties"] = Value::Array(extended);
    message
}

/// SHA-256 of a file, short, to compare files by content.
fn digest(data: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(data)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Re-creates each PST message twice in `ecm-test` (from MIME, and from JSON with
/// the creation-only properties) and prints how each copy compares.
#[cfg(windows)]
pub fn fidelity_graph(
    graph: &GraphArgs,
    pst: &Path,
    keys: &Path,
    sender: Option<&str>,
    reference: Option<(&EwsArgs, &Path)>,
) -> Result<(), String> {
    let text = fs::read_to_string(keys).map_err(|e| format!("{}: {e}", keys.display()))?;
    let keys: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .collect();

    // The reference copies, found by Message-ID in the target's inventory.
    let reference = match reference {
        Some((ews, work)) => {
            let target =
                read_inventory(&work.join(VERIFIED_INVENTORY)).map_err(|e| e.to_string())?;
            Some((connect(ews)?, target))
        }
        None => None,
    };

    let transport = ecm_graph::WinHttpTransport::default();
    let tokens = ecm_graph::auth::sign_in_to_write(
        &transport,
        &graph.client_id,
        &graph.tenant,
        &mut |prompt| {
            eprintln!();
            eprintln!(
                "Sign in to Outlook.com (to write in {TEST_FOLDER} only): open {} and enter the code {}",
                prompt.verification_uri, prompt.user_code
            );
            eprintln!("(Waiting for you to sign in…)");
        },
    )
    .map_err(|e| e.to_string())?;
    let client = ecm_graph::GraphClient::new(transport, tokens.access);
    let folder = client
        .ensure_folder(None, TEST_FOLDER)
        .map_err(|e| e.to_string())?;
    println!("Test folder: {TEST_FOLDER} (items are only created there).");

    let before = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    let reader = ecm_pst::PstSource::open(pst).map_err(|e| e.to_string())?;
    for key in keys {
        let content = reader.message_content(key).map_err(|e| e.to_string())?;
        println!();
        println!(
            "== {key}: {:?} ({} files, {} bytes of files) ==",
            content.subject.as_deref().unwrap_or(""),
            content.attachments.len(),
            content
                .attachments
                .iter()
                .map(|a| a.data.len())
                .sum::<usize>()
        );
        let mut bare = content.clone();
        bare.attachments.clear();
        let full_mime = ecm_pst::content::to_mime_with(&content, sender);
        let split = full_mime.len() > MAX_REQUEST;

        let mut copies: Vec<(&str, Result<String, String>)> = Vec::new();
        // A: from MIME.
        let mime = if split {
            ecm_pst::content::to_mime_with(&bare, sender)
        } else {
            full_mime
        };
        let a = client
            .create_from_mime(&folder, &mime)
            .map_err(|e| e.to_string())
            .and_then(|id| attach(&client, &id, &content, split).map(|()| id));
        copies.push(("A: MIME", a));
        // B: from JSON with the creation-only properties.
        let b = client
            .create_message(&folder, &graph_message(&content, sender))
            .map_err(|e| e.to_string())
            .and_then(|id| attach(&client, &id, &content, true).map(|()| id));
        copies.push(("B: JSON", b));

        let reference_props = match &reference {
            Some((ews, target)) => {
                let copy = target.iter().find(|item| {
                    item.message_id.is_some() && item.message_id == content.message_id
                });
                match copy {
                    Some(copy) => ews
                        .item_properties(&[copy.key.as_str()])
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .next()
                        .and_then(Result::ok),
                    None => None,
                }
            }
            None => None,
        };
        let expected: HashSet<String> = content
            .attachments
            .iter()
            .map(|a| digest(&a.data))
            .collect();
        let mut props: Vec<(&str, PropertyMap)> = Vec::new();
        for (name, result) in &copies {
            match result {
                Err(error) => println!("  {name}: FAILED: {error}"),
                Ok(id) => {
                    let mut map = client.message_properties(id).map_err(|e| e.to_string())?;
                    let draft = client
                        .get(&format!(
                            "/me/messages/{}?$select=isDraft",
                            ecm_graph::encode(id)
                        ))
                        .map_err(|e| e.to_string())?["isDraft"]
                        .as_bool();
                    map.insert("draft".into(), format!("{draft:?}"));
                    let files = client.attachments(id).map_err(|e| e.to_string())?;
                    let got: HashSet<String> = files.iter().map(|f| digest(&f.content)).collect();
                    println!(
                        "  {name}: draft {draft:?}, files identical to the PST: {} of {}{}",
                        expected.intersection(&got).count(),
                        expected.len(),
                        if got.len() > expected.len() {
                            format!(" ({} more)", got.len() - expected.len())
                        } else {
                            String::new()
                        }
                    );
                    props.push((name, map));
                }
            }
        }
        if let Some(reference) = &reference_props {
            let copies: Vec<(&str, &PropertyMap)> = props.iter().map(|(n, p)| (*n, p)).collect();
            crate::fidelity_cmd::print_comparison("OVH copy", reference, &copies);
        }
    }
    let after = ecm_pst::sha256_file(pst).map_err(|e| e.to_string())?;
    if before != after {
        return Err(format!("{} changed while it was read", pst.display()));
    }
    println!();
    println!("Only {TEST_FOLDER} was written to; the PST is unchanged.");
    Ok(())
}

/// Gives a message its files when they were left out of the request.
#[cfg(windows)]
fn attach<T: ecm_graph::Transport>(
    client: &ecm_graph::GraphClient<T>,
    id: &str,
    content: &MessageContent,
    left_out: bool,
) -> Result<(), String> {
    if !left_out {
        return Ok(());
    }
    for file in &content.attachments {
        client
            .attach_file(
                id,
                &file.name,
                file.mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
                ecm_pst::content::shown_content_id(content, file),
                &file.data,
            )
            .map_err(|e| format!("{}: {e}", file.name))?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn fidelity_graph(
    _: &GraphArgs,
    _: &Path,
    _: &Path,
    _: Option<&str>,
    _: Option<(&EwsArgs, &Path)>,
) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

#[cfg(test)]
mod tests {
    use ecm_pst::content::{MessageContent, Recipient, RecipientKind};

    use super::graph_message;

    #[test]
    fn a_json_message_is_not_a_draft_and_keeps_its_dates_and_headers() {
        let content = MessageContent {
            subject: Some("Vélo".into()),
            sender_name: None,
            sender_address: None,
            message_id: Some("<1@x.test>".into()),
            sent: Some(1_452_101_861),
            received: Some(1_452_101_921),
            read: Some(true),
            text: Some("Salut".into()),
            headers: Some("Received: from x\r\n".into()),
            recipients: vec![Recipient {
                kind: RecipientKind::To,
                name: Some("Gael".into()),
                address: Some("g@x.test".into()),
            }],
            ..Default::default()
        };
        let message = graph_message(&content, Some("Geoffrey <me@x.test>"));
        assert_eq!(message["from"]["emailAddress"]["address"], "me@x.test");
        assert_eq!(message["from"]["emailAddress"]["name"], "Geoffrey");
        assert_eq!(message["body"]["contentType"], "text");
        assert_eq!(
            message["toRecipients"][0]["emailAddress"]["address"],
            "g@x.test"
        );
        assert_eq!(message["internetMessageId"], "<1@x.test>");
        let extended = message["singleValueExtendedProperties"].as_array().unwrap();
        assert_eq!(extended[0]["id"], "Integer 0x0E07");
        assert_eq!(extended[0]["value"], "1");
        assert_eq!(extended[1]["id"], "SystemTime 0x0E06");
        assert_eq!(extended[1]["value"], "2016-01-06T17:38:41Z");
        assert_eq!(extended[3]["id"], "String 0x007D");
    }
}
