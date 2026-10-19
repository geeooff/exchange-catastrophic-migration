//! A message's full content from a PST, and its rebuilding as MIME.
//!
//! A PST keeps no MIME: only the message's properties, recipients and
//! attachments. [`to_mime`] rebuilds an Internet message from them, keeping the
//! original Internet headers (`PR_TRANSPORT_MESSAGE_HEADERS`) when the message
//! has them, so that the `Message-ID`, sender and dates survive.

use std::{collections::BTreeMap, io};

use base64::Engine as _;
use outlook_pst::{
    ltp::prop_context::PropertyValue,
    messaging::{
        attachment::{Attachment as _, AttachmentData, UnicodeAttachment},
        message::{Message as _, UnicodeMessage},
    },
    ndb::node_id::NodeId,
};

use crate::{Error, PstSource, Result, filetime_to_unix, row_properties};

const PR_SUBJECT: u16 = 0x0037;
const PR_CLIENT_SUBMIT_TIME: u16 = 0x0039;
const PR_TRANSPORT_MESSAGE_HEADERS: u16 = 0x007D;
const PR_RECIPIENT_TYPE: u16 = 0x0C15;
const PR_SENDER_NAME: u16 = 0x0C1A;
const PR_SENDER_EMAIL_ADDRESS: u16 = 0x0C1F;
const PR_MESSAGE_DELIVERY_TIME: u16 = 0x0E06;
const PR_MESSAGE_FLAGS: u16 = 0x0E07;
const PR_BODY: u16 = 0x1000;
const PR_HTML: u16 = 0x1013;
const PR_RTF_COMPRESSED: u16 = 0x1009;
const PR_INTERNET_MESSAGE_ID: u16 = 0x1035;
const PR_DISPLAY_NAME: u16 = 0x3001;
const PR_EMAIL_ADDRESS: u16 = 0x3003;
const PR_ATTACH_DATA_BIN: u16 = 0x3701;
const PR_ATTACH_FILENAME: u16 = 0x3704;
const PR_ATTACH_LONG_FILENAME: u16 = 0x3707;
const PR_ATTACH_MIME_TAG: u16 = 0x370E;
const PR_ATTACH_CONTENT_ID: u16 = 0x3712;
const PR_SMTP_ADDRESS: u16 = 0x39FE;
const PR_INTERNET_CPID: u16 = 0x3FDE;
const PR_SENDER_SMTP_ADDRESS: u16 = 0x5D01;

/// Who a recipient is to the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientKind {
    /// `To`.
    To,
    /// `Cc`.
    Cc,
    /// `Bcc`.
    Bcc,
}

/// A recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    /// `To`, `Cc` or `Bcc`.
    pub kind: RecipientKind,
    /// Display name.
    pub name: Option<String>,
    /// SMTP address.
    pub address: Option<String>,
}

/// A file attached to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedFile {
    /// File name.
    pub name: String,
    /// MIME type, when the PST knows it.
    pub mime_type: Option<String>,
    /// `Content-ID`, for images an HTML body shows inline.
    pub content_id: Option<String>,
    /// The bytes.
    pub data: Vec<u8>,
}

/// Everything needed to rebuild a message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageContent {
    /// Original Internet headers, when the message came from the Internet.
    pub headers: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// Sender display name.
    pub sender_name: Option<String>,
    /// Sender address.
    pub sender_address: Option<String>,
    /// `Message-ID`.
    pub message_id: Option<String>,
    /// Sent, Unix seconds.
    pub sent: Option<i64>,
    /// Received, Unix seconds.
    pub received: Option<i64>,
    /// Whether it is read.
    pub read: Option<bool>,
    /// Plain text body.
    pub text: Option<String>,
    /// HTML body, in `code_page`.
    pub html: Option<Vec<u8>>,
    /// Windows code page of the HTML body (`PR_INTERNET_CPID`).
    pub code_page: Option<i32>,
    /// Whether the HTML or text body was recovered from the RTF one.
    pub body_from_rtf: bool,
    /// An RTF body nothing could be recovered from, and no other body: MIME
    /// rebuilt from the message would lose it.
    pub rtf_lost: bool,
    /// Recipients.
    pub recipients: Vec<Recipient>,
    /// File attachments.
    pub attachments: Vec<AttachedFile>,
    /// Attachments left out: embedded messages, attachments without data, and
    /// attachments the PST library cannot open.
    pub skipped_attachments: usize,
}

impl PstSource {
    /// The full content of the message with this inventory key (its node id in
    /// hexadecimal). Only Unicode PSTs are supported. Only reads.
    pub fn message_content(&self, key: &str) -> Result<MessageContent> {
        let unicode = self.unicode.as_ref().ok_or_else(|| Error::Format {
            path: self.path.clone(),
            source: io::Error::other("reading full messages needs a Unicode PST"),
        })?;
        let node = u32::from_str_radix(key, 16).map_err(|e| Error::Format {
            path: self.path.clone(),
            source: io::Error::other(format!("invalid message key {key:?}: {e}")),
        })?;
        let format_error = |source| Error::Format {
            path: self.path.clone(),
            source,
        };
        let entry_id = self
            .store
            .properties()
            .make_entry_id(NodeId::from(node))
            .map_err(format_error)?;
        let message =
            UnicodeMessage::read(unicode.clone(), &entry_id, None).map_err(format_error)?;
        let properties: BTreeMap<u16, PropertyValue> = message
            .properties()
            .iter()
            .map(|(id, value)| (*id, value.clone()))
            .collect();
        let mut content = content_from(&properties);

        if let Some(table) = message.recipient_table() {
            for row in table.rows_matrix() {
                let row = row_properties(table.as_ref(), row).map_err(format_error)?;
                let kind = match integer(&row, PR_RECIPIENT_TYPE) {
                    Some(2) => RecipientKind::Cc,
                    Some(3) => RecipientKind::Bcc,
                    _ => RecipientKind::To,
                };
                content.recipients.push(Recipient {
                    kind,
                    name: text(&row, PR_DISPLAY_NAME),
                    address: text(&row, PR_SMTP_ADDRESS)
                        .or_else(|| text(&row, PR_EMAIL_ADDRESS).filter(|a| a.contains('@'))),
                });
            }
        }
        if let Some(table) = message.attachment_table() {
            for row in table.rows_matrix() {
                let sub_node = NodeId::from(u32::from(row.id()));
                // Some attachments `outlook-pst` cannot open (an embedded message
                // without its data object, no attach method): the message is still
                // rebuilt, without them.
                let Ok(attachment) = UnicodeAttachment::read(message.clone(), sub_node, None)
                else {
                    content.skipped_attachments += 1;
                    continue;
                };
                let properties: BTreeMap<u16, PropertyValue> = attachment
                    .properties()
                    .iter()
                    .map(|(id, value)| (*id, value.clone()))
                    .collect();
                let data = match attachment.data() {
                    Some(AttachmentData::Binary(data)) => Some(data.buffer().to_vec()),
                    _ => match properties.get(&PR_ATTACH_DATA_BIN) {
                        Some(PropertyValue::Binary(data)) => Some(data.buffer().to_vec()),
                        _ => None,
                    },
                };
                let Some(data) = data else {
                    content.skipped_attachments += 1;
                    continue;
                };
                content.attachments.push(AttachedFile {
                    name: text(&properties, PR_ATTACH_LONG_FILENAME)
                        .or_else(|| text(&properties, PR_ATTACH_FILENAME))
                        .or_else(|| text(&properties, PR_DISPLAY_NAME))
                        .unwrap_or_else(|| "attachment".into()),
                    mime_type: text(&properties, PR_ATTACH_MIME_TAG),
                    content_id: text(&properties, PR_ATTACH_CONTENT_ID),
                    data,
                });
            }
        }
        // A body kept only as RTF, or HTML a client kept only inside its RTF.
        if let Some(PropertyValue::Binary(rtf)) = properties.get(&PR_RTF_COMPRESSED)
            && content.html.is_none()
            && signed_entity(&content).is_none()
        {
            match crate::rtf::body(rtf.buffer()) {
                Some(crate::rtf::Body::Html(html)) => {
                    content.html = Some(html.into_bytes());
                    content.code_page = Some(65001);
                    content.body_from_rtf = true;
                }
                Some(crate::rtf::Body::Text(text))
                    if content.text.as_deref().is_none_or(|t| t.trim().is_empty()) =>
                {
                    content.text = Some(text);
                    content.body_from_rtf = true;
                }
                Some(_) => {}
                None => content.rtf_lost = !has_body(&content) && !rtf.buffer().is_empty(),
            }
        }
        Ok(content)
    }
}

fn text(properties: &BTreeMap<u16, PropertyValue>, id: u16) -> Option<String> {
    match properties.get(&id) {
        Some(PropertyValue::Unicode(value)) => Some(value.to_string()),
        Some(PropertyValue::String8(value)) => Some(value.to_string()),
        _ => None,
    }
    .map(|value| value.trim_end_matches('\0').trim().to_owned())
    .filter(|value| !value.is_empty())
}

fn integer(properties: &BTreeMap<u16, PropertyValue>, id: u16) -> Option<i64> {
    match properties.get(&id) {
        Some(PropertyValue::Integer32(value)) => Some(i64::from(*value)),
        Some(PropertyValue::Integer64(value)) => Some(*value),
        _ => None,
    }
}

fn content_from(properties: &BTreeMap<u16, PropertyValue>) -> MessageContent {
    let time = |id| match properties.get(&id) {
        Some(PropertyValue::Time(value)) => Some(filetime_to_unix(*value)),
        _ => None,
    };
    let html = match properties.get(&PR_HTML) {
        Some(PropertyValue::Binary(value)) => Some(value.buffer().to_vec()),
        Some(PropertyValue::Unicode(value)) => Some(value.to_string().into_bytes()),
        Some(PropertyValue::String8(value)) => Some(value.buffer().to_vec()),
        _ => None,
    }
    .filter(|html| !html.is_empty());
    MessageContent {
        headers: properties
            .get(&PR_TRANSPORT_MESSAGE_HEADERS)
            .and_then(|_| text(properties, PR_TRANSPORT_MESSAGE_HEADERS)),
        subject: text(properties, PR_SUBJECT).map(|s| crate::subject_text(&s).to_owned()),
        sender_name: text(properties, PR_SENDER_NAME),
        sender_address: text(properties, PR_SENDER_SMTP_ADDRESS)
            .or_else(|| text(properties, PR_SENDER_EMAIL_ADDRESS).filter(|a| a.contains('@'))),
        message_id: text(properties, PR_INTERNET_MESSAGE_ID),
        sent: time(PR_CLIENT_SUBMIT_TIME),
        received: time(PR_MESSAGE_DELIVERY_TIME),
        read: integer(properties, PR_MESSAGE_FLAGS).map(|flags| flags & 1 != 0),
        text: text(properties, PR_BODY),
        // With UTF-16 HTML the bytes were re-encoded as UTF-8 above.
        code_page: match properties.get(&PR_HTML) {
            Some(PropertyValue::Unicode(_)) => Some(65001),
            _ => integer(properties, PR_INTERNET_CPID).and_then(|cp| i32::try_from(cp).ok()),
        },
        html,
        ..Default::default()
    }
}

/// Header fields a rebuilt message sets itself, dropped from the original ones.
const REBUILT_FIELDS: &[&str] = &[
    "content-type",
    "content-transfer-encoding",
    "content-disposition",
    "content-id",
    "content-description",
    "mime-version",
];

/// Rebuilds the message as MIME: the original Internet headers when there are
/// some (else headers made from the properties), a `multipart/alternative` body
/// with the text and HTML versions, and the attachments.
pub fn to_mime(content: &MessageContent) -> Vec<u8> {
    to_mime_with(content, None)
}

/// Like [`to_mime`], writing `sender` (a mailbox, e.g. `Name <address>`) as the
/// `From` field when the message has none, an empty one or one without an
/// address, as sent items synchronized by Outlook sometimes do.
pub fn to_mime_with(content: &MessageContent, sender: Option<&str>) -> Vec<u8> {
    let mut out = String::new();
    match content.headers.as_deref() {
        Some(headers) => {
            let mut has_message_id = false;
            let mut has_from = false;
            for field in header_fields(headers) {
                let name = field
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase();
                if REBUILT_FIELDS.contains(&name.as_str()) {
                    continue;
                }
                has_message_id |= name == "message-id";
                if name == "from" {
                    // Empty, or a display name without an address.
                    let empty = field.split_once(':').is_none_or(|(_, v)| !v.contains('@'));
                    match (empty, sender) {
                        (true, Some(sender)) => {
                            out.push_str(&format!("From: {sender}\r\n"));
                            has_from = true;
                            continue;
                        }
                        (true, None) => {}
                        (false, _) => has_from = true,
                    }
                }
                out.push_str(&field);
                out.push_str("\r\n");
            }
            if !has_from && let Some(sender) = sender {
                out.push_str(&format!("From: {sender}\r\n"));
            }
            if !has_message_id && let Some(id) = &content.message_id {
                out.push_str(&format!("Message-ID: {id}\r\n"));
            }
        }
        None => {
            let headers = synthesized_headers(content);
            let mut has_from = false;
            for line in headers.split_inclusive("\r\n") {
                if let Some(value) = line.strip_prefix("From: ") {
                    match sender.filter(|_| !value.contains('@')) {
                        Some(sender) => out.push_str(&format!("From: {sender}\r\n")),
                        None => out.push_str(line),
                    }
                    has_from = true;
                } else {
                    out.push_str(line);
                }
            }
            if !has_from && let Some(sender) = sender {
                out.push_str(&format!("From: {sender}\r\n"));
            }
        }
    }
    // A clear-signed (S/MIME) message: the store keeps the whole signed MIME
    // entity as its only attachment, which is the body to send back.
    if let Some(signed) = signed_entity(content) {
        out.push_str(&String::from_utf8_lossy(signed));
        return out.into_bytes();
    }
    out.push_str("MIME-Version: 1.0\r\n");

    let alternative = alternative_body(content);
    if content.attachments.is_empty() {
        out.push_str(&alternative);
        return out.into_bytes();
    }
    let boundary = "=_ecm_mixed_0";
    out.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"
    ));
    out.push_str(&format!("--{boundary}\r\n"));
    out.push_str(&alternative);
    for file in &content.attachments {
        out.push_str(&format!("\r\n--{boundary}\r\n"));
        let mime_type = file
            .mime_type
            .as_deref()
            .unwrap_or("application/octet-stream");
        let name = encoded_word(&file.name);
        out.push_str(&format!("Content-Type: {mime_type}; name=\"{name}\"\r\n"));
        out.push_str("Content-Transfer-Encoding: base64\r\n");
        // Inline only when the HTML shows it: a Content-ID alone does not hide a
        // file the body never uses.
        let disposition = if shown_content_id(content, file).is_some() {
            "inline"
        } else {
            "attachment"
        };
        out.push_str(&format!(
            "Content-Disposition: {disposition}; filename=\"{name}\"\r\n"
        ));
        if let Some(id) = &file.content_id {
            out.push_str(&format!(
                "Content-ID: <{}>\r\n",
                id.trim_matches(['<', '>'])
            ));
        }
        out.push_str("\r\n");
        out.push_str(&base64_lines(&file.data));
    }
    out.push_str(&format!("\r\n--{boundary}--\r\n"));
    out.into_bytes()
}

/// The `Content-ID` of a file the HTML body shows (`cid:`), else `None`: many
/// files carry a `Content-ID` the body never uses, and a target would hide them
/// as inline images.
pub fn shown_content_id<'a>(content: &MessageContent, file: &'a AttachedFile) -> Option<&'a str> {
    let id = file.content_id.as_deref()?.trim_matches(['<', '>']);
    let html = content.html.as_ref()?;
    let needle = format!("cid:{id}").to_ascii_lowercase().into_bytes();
    html.to_ascii_lowercase()
        .windows(needle.len())
        .any(|w| w == needle)
        .then_some(id)
}

/// The HTML body as text, decoded from its code page (Western European when
/// unknown).
pub fn html_text(content: &MessageContent) -> Option<String> {
    let html = content.html.as_ref()?;
    let code_page = content
        .code_page
        .and_then(|cp| u32::try_from(cp).ok())
        .unwrap_or(65001);
    Some(crate::rtf::encoding(code_page).decode(html).0.into_owned())
}

/// Whether rebuilding `content` as MIME would lose its body: an RTF body nothing
/// could be recovered from, since MIME has no RTF. A message whose body is empty
/// (only attachments) loses nothing.
pub fn loses_body(content: &MessageContent) -> bool {
    content.rtf_lost
}

/// Whether the message has a body: a text or HTML one (maybe recovered from the
/// RTF body), or a signed entity.
pub fn has_body(content: &MessageContent) -> bool {
    content
        .text
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
        || content.html.is_some()
        || signed_entity(content).is_some()
}

/// The signed MIME entity of a clear-signed message, kept as an attachment.
fn signed_entity(content: &MessageContent) -> Option<&[u8]> {
    if content.text.is_some() || content.html.is_some() {
        return None;
    }
    content
        .attachments
        .iter()
        .map(|file| file.data.as_slice())
        .find(|data| {
            let head = String::from_utf8_lossy(&data[..data.len().min(512)]).to_lowercase();
            head.starts_with("mime-version:") && head.contains("multipart/signed")
        })
}

/// The `Content-Type` header and body of the text and HTML versions.
fn alternative_body(content: &MessageContent) -> String {
    let text = content.text.as_deref().unwrap_or_default();
    let text_part = format!(
        "Content-Type: text/plain; charset=\"utf-8\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
        base64_lines(text.as_bytes())
    );
    let Some(html) = &content.html else {
        return text_part;
    };
    let html_part = format!(
        "Content-Type: text/html; charset=\"{}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
        charset(content.code_page),
        base64_lines(html)
    );
    let boundary = "=_ecm_alt_0";
    format!(
        "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n{text_part}\r\n--{boundary}\r\n{html_part}\r\n--{boundary}--\r\n"
    )
}

/// The MIME charset of a Windows code page.
fn charset(code_page: Option<i32>) -> &'static str {
    match code_page {
        Some(1252) => "windows-1252",
        Some(28591) => "iso-8859-1",
        Some(28605) => "iso-8859-15",
        Some(20127) => "us-ascii",
        Some(1250) => "windows-1250",
        Some(50220) => "iso-2022-jp",
        _ => "utf-8",
    }
}

/// Header fields, each with its continuation lines.
fn header_fields(headers: &str) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    for line in headers.lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(last) = fields.last_mut() {
                last.push_str("\r\n");
                last.push_str(line);
            }
        } else {
            fields.push(line.to_owned());
        }
    }
    fields
}

fn synthesized_headers(content: &MessageContent) -> String {
    let mut out = String::new();
    let mailbox = |name: Option<&str>, address: Option<&str>| match (name, address) {
        (Some(name), Some(address)) => format!("{} <{address}>", encoded_word(name)),
        (None, Some(address)) => format!("<{address}>"),
        (Some(name), None) => encoded_word(name),
        (None, None) => String::new(),
    };
    let from = mailbox(
        content.sender_name.as_deref(),
        content.sender_address.as_deref(),
    );
    if !from.is_empty() {
        out.push_str(&format!("From: {from}\r\n"));
    }
    for (kind, name) in [
        (RecipientKind::To, "To"),
        (RecipientKind::Cc, "Cc"),
        (RecipientKind::Bcc, "Bcc"),
    ] {
        let list: Vec<String> = content
            .recipients
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| mailbox(r.name.as_deref(), r.address.as_deref()))
            .filter(|m| !m.is_empty())
            .collect();
        if !list.is_empty() {
            out.push_str(&format!("{name}: {}\r\n", list.join(",\r\n ")));
        }
    }
    if let Some(subject) = &content.subject {
        out.push_str(&format!("Subject: {}\r\n", encoded_word(subject)));
    }
    if let Some(sent) = content.sent.or(content.received) {
        out.push_str(&format!("Date: {}\r\n", rfc2822(sent)));
    }
    if let Some(id) = &content.message_id {
        out.push_str(&format!("Message-ID: {id}\r\n"));
    }
    out
}

/// `text` as an RFC 2047 encoded word when it is not plain ASCII.
fn encoded_word(text: &str) -> String {
    if text.is_ascii() && !text.contains(['"', '\r', '\n']) {
        return text.to_owned();
    }
    format!(
        "=?utf-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    )
}

fn base64_lines(data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2 + 2);
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push_str("\r\n");
    }
    out
}

/// An RFC 2822 date, in UTC.
fn rfc2822(seconds: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} +0000",
        DAYS[days.rem_euclid(7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_inline_only_when_the_html_shows_it() {
        let file = |id: &str| AttachedFile {
            name: "a.jpg".into(),
            mime_type: Some("image/jpeg".into()),
            content_id: Some(id.into()),
            data: vec![1],
        };
        let content = MessageContent {
            html: Some(br#"<img src="CID:logo@x">"#.to_vec()),
            ..Default::default()
        };
        assert_eq!(
            shown_content_id(&content, &file("<logo@x>")),
            Some("logo@x")
        );
        assert_eq!(shown_content_id(&content, &file("photo@x")), None);
        let text_only = MessageContent::default();
        assert_eq!(shown_content_id(&text_only, &file("logo@x")), None);
    }

    fn decode_part(mime: &str, after: &str) -> Vec<u8> {
        let start = mime.find(after).expect("part header") + after.len();
        let body: String = mime[start..]
            .lines()
            .skip_while(|l| l.is_empty())
            .take_while(|l| !l.is_empty() && !l.starts_with("--"))
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap()
    }

    #[test]
    fn keeps_the_original_headers_but_not_their_mime_fields() {
        let content = MessageContent {
            headers: Some(
                "Received: from mx.example.test\r\n by mx2; Mon, 1 Jan 2024\r\nFrom: News <news@example.test>\r\nMessage-ID: <a@example.test>\r\nContent-Type: multipart/alternative;\r\n boundary=\"x\"\r\nMIME-Version: 1.0\r\nSubject: Hello\r\n\r\n".into(),
            ),
            text: Some("Hi".into()),
            html: Some(b"<p>Caf\xe9</p>".to_vec()),
            code_page: Some(1252),
            ..Default::default()
        };
        let mime = String::from_utf8(to_mime(&content)).unwrap();
        assert!(mime.starts_with("Received: from mx.example.test\r\n by mx2; Mon, 1 Jan 2024\r\nFrom: News <news@example.test>\r\nMessage-ID: <a@example.test>\r\nSubject: Hello\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative;"));
        assert!(!mime.contains("boundary=\"x\""));
        assert!(mime.contains("Content-Type: text/html; charset=\"windows-1252\""));
        assert_eq!(
            decode_part(
                &mime,
                "charset=\"windows-1252\"\r\nContent-Transfer-Encoding: base64\r\n"
            ),
            b"<p>Caf\xe9</p>"
        );
        assert_eq!(
            decode_part(
                &mime,
                "charset=\"utf-8\"\r\nContent-Transfer-Encoding: base64\r\n"
            ),
            b"Hi"
        );
    }

    #[test]
    fn makes_headers_and_keeps_attachments_byte_for_byte() {
        let photo: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let content = MessageContent {
            subject: Some("Photos d'été".into()),
            sender_name: Some("Me".into()),
            sender_address: Some("me@example.test".into()),
            message_id: Some("<m@example.test>".into()),
            sent: Some(1_073_246_631),
            recipients: vec![Recipient {
                kind: RecipientKind::To,
                name: None,
                address: Some("you@example.test".into()),
            }],
            text: Some("See attached".into()),
            attachments: vec![AttachedFile {
                name: "nouvel an.jpg".into(),
                mime_type: Some("image/jpeg".into()),
                content_id: None,
                data: photo.clone(),
            }],
            ..Default::default()
        };
        let mime = String::from_utf8(to_mime(&content)).unwrap();
        assert!(mime.starts_with(
            "From: Me <me@example.test>\r\nTo: <you@example.test>\r\nSubject: =?utf-8?B?"
        ));
        assert!(mime.contains("Date: Sun, 04 Jan 2004 20:03:51 +0000\r\nMessage-ID: <m@example.test>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed;"));
        assert!(mime.contains("Content-Disposition: attachment; filename=\"nouvel an.jpg\""));
        assert_eq!(decode_part(&mime, "filename=\"nouvel an.jpg\"\r\n"), photo);
        assert!(mime.ends_with("--=_ecm_mixed_0--\r\n"));
    }

    #[test]
    fn sends_back_the_signed_entity_of_a_signed_message() {
        let signed = b"MIME-Version: 1.0\r\nContent-Type: multipart/signed;\r\n\tprotocol=\"application/pkcs7-signature\"; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>Done</p>\r\n--b--\r\n".to_vec();
        let content = MessageContent {
            headers: Some("From: bank@example.test\r\nContent-Type: multipart/signed; boundary=\"b\"\r\nSubject: Done\r\n\r\n".into()),
            attachments: vec![AttachedFile {
                name: "ATT1.txt".into(),
                mime_type: Some("text/plain".into()),
                content_id: None,
                data: signed.clone(),
            }],
            ..Default::default()
        };
        let mime = to_mime(&content);
        let mut expected = b"From: bank@example.test\r\nSubject: Done\r\n".to_vec();
        expected.extend(signed);
        assert_eq!(mime, expected);
    }

    #[test]
    fn fills_an_empty_from_field() {
        let content = MessageContent {
            headers: Some("To: <a@example.test>\r\nFrom:\r\nSubject: Zeus\r\n\r\n".into()),
            text: Some("x".into()),
            ..Default::default()
        };
        let mime = String::from_utf8(to_mime_with(&content, Some("Me <me@example.test>"))).unwrap();
        assert!(mime.starts_with(
            "To: <a@example.test>\r\nFrom: Me <me@example.test>\r\nSubject: Zeus\r\n"
        ));
        let kept = String::from_utf8(to_mime(&content)).unwrap();
        assert!(kept.starts_with("To: <a@example.test>\r\nFrom:\r\nSubject: Zeus\r\n"));
        let named = MessageContent {
            headers: Some("From: Me\r\nSubject: Zeus\r\n\r\n".into()),
            ..content
        };
        let mime = String::from_utf8(to_mime_with(&named, Some("Me <me@example.test>"))).unwrap();
        assert!(mime.starts_with("From: Me <me@example.test>\r\nSubject: Zeus\r\n"));
        let bare = MessageContent {
            headers: None,
            sender_name: Some("Me".into()),
            subject: Some("Zeus".into()),
            text: Some("x".into()),
            ..Default::default()
        };
        let mime = String::from_utf8(to_mime_with(&bare, Some("Me <me@example.test>"))).unwrap();
        assert!(mime.starts_with("From: Me <me@example.test>\r\nSubject: Zeus\r\n"));
    }

    #[test]
    fn drops_the_subject_prefix_marker() {
        assert_eq!(crate::subject_text("\u{1}\u{1}Zeus"), "Zeus");
        assert_eq!(crate::subject_text("\u{1}\u{5}TR: Zeus"), "TR: Zeus");
        assert_eq!(crate::subject_text("Zeus"), "Zeus");
    }

    #[test]
    fn formats_dates_like_rfc_2822() {
        assert_eq!(rfc2822(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(rfc2822(1_709_208_000), "Thu, 29 Feb 2024 12:00:00 +0000");
    }
}
