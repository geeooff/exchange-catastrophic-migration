//! Calendar items, contacts and tasks: every property of a message, named
//! properties resolved through the store's name-to-id map ([MS-PST] 2.4.7), its
//! recipients and its attachments, files and embedded messages (the exceptions of
//! a series).

use std::{
    collections::{BTreeMap, HashMap},
    io,
    rc::Rc,
};

use outlook_pst::{
    ltp::prop_context::{GuidValue, PropertyValue},
    messaging::{
        attachment::{Attachment as _, AttachmentData, UnicodeAttachment},
        message::{Message, UnicodeMessage},
        named_prop::{NamedPropertyGuid, NamedPropertyId},
        store::EntryId,
    },
    ndb::node_id::NodeId,
};

use crate::{PstSource, Result, row_properties};

/// A property set ([MS-OXPROPS]) as a number: `00062002-0000-0000-C000-000000000046`
/// is `0x00062002_0000_0000_C000_000000000046`.
pub type PropertySet = u128;

/// `PS_MAPI`: named properties that are tagged properties.
pub const PS_MAPI: PropertySet = 0x0002_0328_0000_0000_C000_0000_0000_0046;
/// `PS_PUBLIC_STRINGS`: categories (`Keywords`) among others.
pub const PS_PUBLIC_STRINGS: PropertySet = 0x0002_0329_0000_0000_C000_0000_0000_0046;
/// `PSETID_Appointment`.
pub const PSETID_APPOINTMENT: PropertySet = 0x0006_2002_0000_0000_C000_0000_0000_0046;
/// `PSETID_Task`.
pub const PSETID_TASK: PropertySet = 0x0006_2003_0000_0000_C000_0000_0000_0046;
/// `PSETID_Address`: contacts.
pub const PSETID_ADDRESS: PropertySet = 0x0006_2004_0000_0000_C000_0000_0000_0046;
/// `PSETID_Common`: reminders among others.
pub const PSETID_COMMON: PropertySet = 0x0006_2008_0000_0000_C000_0000_0000_0046;
/// `PSETID_Meeting`.
pub const PSETID_MEETING: PropertySet = 0x6ED8_DA90_450B_101B_98DA_00AA_003F_1305;

/// The message classes read, by prefix.
const CLASSES: [&str; 4] = ["IPM.Appointment", "IPM.Contact", "IPM.DistList", "IPM.Task"];

/// The folder classes read: exactly these, not the birthday calendar, the
/// recipient cache or Skype's contact lists, whose classes extend them.
const FOLDER_CLASSES: [&str; 3] = ["IPF.Appointment", "IPF.Contact", "IPF.Task"];

/// Top folders whose calendars, contact folders and task folders are not read
/// unless named: deleted, recoverable, sync conflicts, junk (English and
/// French names).
const SET_ASIDE: [&str; 8] = [
    "deleted items",
    "éléments supprimés",
    "recoverable items",
    "sync issues",
    "problèmes de synchronisation",
    "junk email",
    "courrier indésirable",
    "conflicts",
];

/// `PR_CONTAINER_CLASS`.
const PR_CONTAINER_CLASS: u16 = 0x3613;
/// `PR_MESSAGE_CLASS`.
const PR_MESSAGE_CLASS: u16 = 0x001A;
/// `PR_BODY`.
const PR_BODY: u16 = 0x1000;
/// `PR_RTF_COMPRESSED`.
const PR_RTF_COMPRESSED: u16 = 0x1009;
/// `PR_HTML`.
const PR_HTML: u16 = 0x1013;
/// `PR_INTERNET_CPID`.
const PR_INTERNET_CPID: u16 = 0x3FDE;

/// A named property's name: a number or a string in a property set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Name {
    /// A number (`LID`).
    Number(PropertySet, u32),
    /// A string.
    String(PropertySet, String),
}

/// A property value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A boolean.
    Bool(bool),
    /// An integer, of any width.
    Integer(i64),
    /// A `FILETIME`: 100-nanosecond intervals since 1601-01-01, UTC.
    Time(i64),
    /// A string, without the NUL some stores leave at its end.
    Text(String),
    /// Bytes.
    Binary(Vec<u8>),
    /// Strings.
    Texts(Vec<String>),
    /// Integers.
    Integers(Vec<i64>),
    /// Another type: floating point, currency, GUID…
    Other,
}

impl Value {
    fn from_pst(value: &PropertyValue) -> Self {
        let text = |s: String| s.trim_end_matches('\0').to_owned();
        match value {
            PropertyValue::Boolean(b) => Value::Bool(*b),
            PropertyValue::Integer16(n) => Value::Integer(i64::from(*n)),
            PropertyValue::Integer32(n) => Value::Integer(i64::from(*n)),
            PropertyValue::Integer64(n) => Value::Integer(*n),
            PropertyValue::Time(t) => Value::Time(*t),
            PropertyValue::Unicode(s) => Value::Text(text(s.to_string())),
            PropertyValue::String8(s) => Value::Text(text(s.to_string())),
            PropertyValue::Binary(b) => Value::Binary(b.buffer().to_vec()),
            PropertyValue::MultipleUnicode(list) => {
                Value::Texts(list.iter().map(|s| text(s.to_string())).collect())
            }
            PropertyValue::MultipleString8(list) => {
                Value::Texts(list.iter().map(|s| text(s.to_string())).collect())
            }
            PropertyValue::MultipleInteger16(list) => {
                Value::Integers(list.iter().map(|n| i64::from(*n)).collect())
            }
            PropertyValue::MultipleInteger32(list) => {
                Value::Integers(list.iter().map(|n| i64::from(*n)).collect())
            }
            PropertyValue::MultipleInteger64(list) => Value::Integers(list.clone()),
            _ => Value::Other,
        }
    }

    /// The string, trimmed, when not empty.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s.trim()).filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    /// The integer.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(n) => Some(*n),
            _ => None,
        }
    }

    /// The boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Integer(n) => Some(*n != 0),
            _ => None,
        }
    }

    /// The `FILETIME`.
    pub fn as_time(&self) -> Option<i64> {
        match self {
            Value::Time(t) => Some(*t),
            _ => None,
        }
    }

    /// The bytes.
    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Value::Binary(b) => Some(b),
            _ => None,
        }
    }

    /// The strings, trimmed and without empty ones; a single string too.
    pub fn as_texts(&self) -> Vec<String> {
        let list: Vec<&String> = match self {
            Value::Texts(list) => list.iter().collect(),
            Value::Text(s) => vec![s],
            _ => Vec::new(),
        };
        list.into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// The properties of a message, a recipient or an attachment, tagged and named.
#[derive(Debug, Clone, Default)]
pub struct Properties {
    values: BTreeMap<u16, Value>,
    names: Rc<HashMap<Name, u16>>,
}

impl Properties {
    /// Properties from their values, named ones resolved with `names` (the
    /// store's name-to-id map); for tests.
    pub fn new(values: BTreeMap<u16, Value>, names: Rc<HashMap<Name, u16>>) -> Self {
        Self { values, names }
    }

    fn from_pst<'a>(
        values: impl Iterator<Item = (&'a u16, &'a PropertyValue)>,
        names: &Rc<HashMap<Name, u16>>,
    ) -> Self {
        Self {
            values: values
                .map(|(id, value)| (*id, Value::from_pst(value)))
                .collect(),
            names: names.clone(),
        }
    }

    /// A tagged property, by its id (`0x0037` for the subject).
    pub fn get(&self, id: u16) -> Option<&Value> {
        self.values.get(&id)
    }

    /// A named property, by its property set and number.
    pub fn named(&self, set: PropertySet, number: u32) -> Option<&Value> {
        self.names
            .get(&Name::Number(set, number))
            .and_then(|id| self.values.get(id))
    }

    /// A named property, by its property set and string name.
    pub fn named_string(&self, set: PropertySet, name: &str) -> Option<&Value> {
        self.names
            .get(&Name::String(set, name.to_owned()))
            .and_then(|id| self.values.get(id))
    }

    /// A tagged string property, trimmed, when not empty.
    pub fn text(&self, id: u16) -> Option<&str> {
        self.get(id).and_then(Value::as_text)
    }

    /// The categories (`Keywords`).
    pub fn categories(&self) -> Vec<String> {
        self.named_string(PS_PUBLIC_STRINGS, "Keywords")
            .map(Value::as_texts)
            .unwrap_or_default()
    }

    /// Every tagged and named value, by id; for tests and diagnostics.
    pub fn values(&self) -> &BTreeMap<u16, Value> {
        &self.values
    }
}

/// What an attachment holds.
#[derive(Debug, Clone)]
pub enum AttachmentContent {
    /// A file's bytes.
    File(Vec<u8>),
    /// An embedded message: for a series, one of its exceptions.
    Message(Box<Item>),
    /// Nothing `outlook-pst` could read.
    None,
}

/// An attachment.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Its properties: name, method, flags.
    pub properties: Properties,
    /// Its content.
    pub content: AttachmentContent,
}

/// `PR_ATTACHMENT_CONTACTPHOTO`.
const PR_ATTACHMENT_CONTACTPHOTO: u16 = 0x7FFF;

impl Attachment {
    /// Whether it is a contact's photo.
    pub fn is_contact_photo(&self) -> bool {
        self.properties
            .get(PR_ATTACHMENT_CONTACTPHOTO)
            .and_then(Value::as_bool)
            == Some(true)
    }
}

/// A body: HTML or plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    /// Whether it is HTML.
    pub html: bool,
    /// The text or the HTML.
    pub content: String,
}

/// A calendar item, a contact, a contact group or a task, read whole.
#[derive(Debug, Clone, Default)]
pub struct Item {
    /// The inventory key (node id in hexadecimal); empty for an embedded message.
    pub key: String,
    /// The folder path below the IPM subtree.
    pub folder: String,
    /// Its properties.
    pub properties: Properties,
    /// Its recipients' properties: a meeting's attendees.
    pub recipients: Vec<Properties>,
    /// Its attachments.
    pub attachments: Vec<Attachment>,
}

impl Item {
    /// The message class.
    pub fn class(&self) -> &str {
        self.properties.text(PR_MESSAGE_CLASS).unwrap_or_default()
    }

    /// The body: HTML when the item has some (or its RTF encapsulates some),
    /// else the plain text, else the text of its RTF.
    pub fn body(&self) -> Option<Body> {
        let props = &self.properties;
        let code_page = props
            .get(PR_INTERNET_CPID)
            .and_then(Value::as_integer)
            .and_then(|cp| u32::try_from(cp).ok())
            .unwrap_or(65001);
        let html = match props.get(PR_HTML) {
            Some(Value::Binary(bytes)) => Some(crate::rtf::decode_html(bytes, code_page)),
            Some(Value::Text(text)) => Some(text.clone()),
            _ => None,
        }
        .map(|html| windows_1252_controls(&html));
        if let Some(html) = html.filter(|h| !h.trim().is_empty()) {
            return Some(Body {
                html: true,
                content: html,
            });
        }
        let rtf = props
            .get(PR_RTF_COMPRESSED)
            .and_then(Value::as_binary)
            .and_then(crate::rtf::body);
        if let Some(crate::rtf::Body::Html(html)) = &rtf {
            return Some(Body {
                html: true,
                content: html.clone(),
            });
        }
        let text = props
            .get(PR_BODY)
            .and_then(|v| match v {
                Value::Text(t) => Some(t.clone()),
                _ => None,
            })
            .filter(|t| !t.trim().is_empty())
            .or(match rtf {
                Some(crate::rtf::Body::Text(text)) => Some(text),
                _ => None,
            })?;
        Some(Body {
            html: false,
            content: text,
        })
    }

    /// The contact's photo, when it has one.
    pub fn contact_photo(&self) -> Option<&[u8]> {
        self.attachments
            .iter()
            .filter(|a| a.is_contact_photo())
            .find_map(|a| match &a.content {
                AttachmentContent::File(data) => Some(data.as_slice()),
                _ => None,
            })
    }

    /// The embedded messages: a series' exceptions.
    pub fn embedded(&self) -> impl Iterator<Item = &Item> {
        self.attachments.iter().filter_map(|a| match &a.content {
            AttachmentContent::Message(item) => Some(item.as_ref()),
            _ => None,
        })
    }
}

/// C1 control characters (U+0080 to U+009F) as the Windows-1252 characters of
/// the same byte, as browsers show them: text once read as Latin-1 keeps its
/// quotes and dashes there.
fn windows_1252_controls(text: &str) -> String {
    if !text.chars().any(|c| ('\u{80}'..='\u{9F}').contains(&c)) {
        return text.to_owned();
    }
    text.chars()
        .map(|c| match u8::try_from(u32::from(c)) {
            Ok(byte @ 0x80..=0x9F) => encoding_rs::WINDOWS_1252
                .decode_without_bom_handling(&[byte])
                .0
                .chars()
                .next()
                .unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// What [`PstSource::pim_items`] read.
#[derive(Debug, Default)]
pub struct PimItems {
    /// The items read.
    pub items: Vec<Item>,
    /// The items `outlook-pst` could not read, with why.
    pub unreadable: Vec<String>,
    /// The folders not read, with how many items they hold.
    pub set_aside: Vec<(String, usize)>,
}

impl PstSource {
    /// The store's name-to-id map: the property id each named property has in
    /// this file.
    pub fn named_ids(&self) -> Result<HashMap<Name, u16>> {
        let map = self
            .store
            .named_property_map()
            .map_err(|source| self.format_error(source))?;
        let props = map.properties();
        let guids = props
            .stream_guid()
            .map_err(|source| self.format_error(source))?;
        let mut names = HashMap::new();
        for entry in props
            .stream_entry()
            .map_err(|source| self.format_error(source))?
        {
            // The GUID stream starts at index 3: 0 is none, 1 PS_MAPI and 2
            // PS_PUBLIC_STRINGS (`outlook-pst` 1.2's own lookup forgets the
            // offset).
            let set = match entry.guid() {
                NamedPropertyGuid::None => continue,
                NamedPropertyGuid::Mapi => PS_MAPI,
                NamedPropertyGuid::PublicStrings => PS_PUBLIC_STRINGS,
                NamedPropertyGuid::GuidIndex(index) => match guids.get(usize::from(index)) {
                    Some(guid) => property_set(guid),
                    None => continue,
                },
            };
            let name = match entry.id() {
                NamedPropertyId::Number(number) => Name::Number(set, number),
                NamedPropertyId::StringOffset(offset) => match props.lookup_string(offset) {
                    Ok(name) => Name::String(set, name.to_string()),
                    Err(_) => continue,
                },
            };
            names.insert(name, entry.prop_id());
        }
        Ok(names)
    }

    /// Every calendar item, contact, contact group and task, read whole: of the
    /// folders named in `folders` (paths below the IPM subtree), or when none
    /// is named, of the folders of exactly the class `IPF.Appointment`,
    /// `IPF.Contact` or `IPF.Task` but those under Deleted Items, Recoverable
    /// Items, Sync Issues and Junk Email, which are listed as set aside. An
    /// item `outlook-pst` cannot read (a message without a sub-node tree) is
    /// listed, not read. Only Unicode PSTs.
    pub fn pim_items(&self, folders: &[String]) -> Result<PimItems> {
        let names = Rc::new(self.named_ids()?);
        let entry_id = self
            .store
            .properties()
            .ipm_sub_tree_entry_id()
            .map_err(|source| self.format_error(source))?;
        let mut read = PimItems::default();
        let wanted: Vec<String> = folders.iter().map(|f| f.to_lowercase()).collect();
        self.collect_pim(&entry_id, String::new(), &wanted, &names, &mut read)?;
        Ok(read)
    }

    fn collect_pim(
        &self,
        entry_id: &EntryId,
        path: String,
        wanted: &[String],
        names: &Rc<HashMap<Name, u16>>,
        read: &mut PimItems,
    ) -> Result<()> {
        let folder = self
            .store
            .open_folder(entry_id)
            .map_err(|source| self.format_error(source))?;
        let class = match folder.properties().get(PR_CONTAINER_CLASS) {
            Some(PropertyValue::Unicode(value)) => value.to_string(),
            Some(PropertyValue::String8(value)) => value.to_string(),
            _ => String::new(),
        };
        let selected = if wanted.is_empty() {
            let top = ecm_model::path::split(&path)
                .first()
                .map(|t| t.to_lowercase())
                .unwrap_or_default();
            let pim = FOLDER_CLASSES.contains(&class.as_str());
            if pim && SET_ASIDE.contains(&top.as_str()) {
                let count = folder
                    .contents_table()
                    .map_or(0, |c| c.rows_matrix().count());
                if count > 0 {
                    read.set_aside.push((path.clone(), count));
                }
                false
            } else {
                pim
            }
        } else {
            wanted.contains(&path.to_lowercase())
        };
        if selected && let Some(contents) = folder.contents_table() {
            for row in contents.rows_matrix() {
                let node = NodeId::from(u32::from(row.id()));
                let key = format!("{:08x}", u32::from(node));
                match self.pim_item(node, names) {
                    Ok(mut item) => {
                        if CLASSES.iter().any(|c| item.class().starts_with(c)) {
                            item.key = key;
                            item.folder = path.clone();
                            read.items.push(item);
                        }
                    }
                    Err(error) => read.unreadable.push(format!("{path}: {key}: {error}")),
                }
            }
        }

        let mut children = Vec::new();
        if let Some(hierarchy) = folder.hierarchy_table() {
            for row in hierarchy.rows_matrix() {
                let node = NodeId::from(u32::from(row.id()));
                let entry_id = self
                    .store
                    .properties()
                    .make_entry_id(node)
                    .map_err(|source| self.format_error(source))?;
                let name = self
                    .store
                    .open_folder(&entry_id)
                    .map_err(|source| self.format_error(source))?
                    .properties()
                    .display_name()
                    .unwrap_or_default();
                children.push((name, entry_id));
            }
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entry_id) in children {
            let child_path = ecm_model::path::join(&path, &name);
            self.collect_pim(&entry_id, child_path, wanted, names, read)?;
        }
        Ok(())
    }

    fn pim_item(&self, node: NodeId, names: &Rc<HashMap<Name, u16>>) -> io::Result<Item> {
        let unicode = self
            .unicode
            .as_ref()
            .ok_or_else(|| io::Error::other("reading whole items needs a Unicode PST"))?;
        let entry_id = self.store.properties().make_entry_id(node)?;
        let message = UnicodeMessage::read(unicode.clone(), &entry_id, None)?;
        let mut item = item_from(message.as_ref(), names)?;
        if let Some(table) = message.attachment_table() {
            for row in table.rows_matrix() {
                let sub_node = NodeId::from(u32::from(row.id()));
                let Ok(attachment) = UnicodeAttachment::read(message.clone(), sub_node, None)
                else {
                    let properties = row_properties(table.as_ref(), row)?;
                    item.attachments.push(Attachment {
                        properties: Properties::from_pst(properties.iter(), names),
                        content: AttachmentContent::None,
                    });
                    continue;
                };
                let content = match attachment.data() {
                    Some(AttachmentData::Binary(data)) => {
                        AttachmentContent::File(data.buffer().to_vec())
                    }
                    Some(AttachmentData::Message(embedded)) => {
                        AttachmentContent::Message(Box::new(item_from(embedded.as_ref(), names)?))
                    }
                    None => AttachmentContent::None,
                };
                item.attachments.push(Attachment {
                    properties: Properties::from_pst(attachment.properties().iter(), names),
                    content,
                });
            }
        }
        Ok(item)
    }
}

/// The properties and recipients of a message; its attachments are read by the
/// caller (an embedded message's are not read).
fn item_from(message: &dyn Message, names: &Rc<HashMap<Name, u16>>) -> io::Result<Item> {
    let mut item = Item {
        properties: Properties::from_pst(message.properties().iter(), names),
        ..Item::default()
    };
    if let Some(table) = message.recipient_table() {
        for row in table.rows_matrix() {
            let properties = row_properties(table.as_ref(), row)?;
            item.recipients
                .push(Properties::from_pst(properties.iter(), names));
        }
    }
    Ok(item)
}

/// A GUID as a [`PropertySet`].
fn property_set(guid: &GuidValue) -> PropertySet {
    (u128::from(guid.data1()) << 96)
        | (u128::from(guid.data2()) << 80)
        | (u128::from(guid.data3()) << 64)
        | u128::from(u64::from_be_bytes(*guid.data4()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_utf8_html_whatever_the_code_page_says() {
        let body = |html: &[u8]| {
            Item {
                properties: Properties::new(
                    [
                        (PR_HTML, Value::Binary(html.to_vec())),
                        (PR_INTERNET_CPID, Value::Integer(1252)),
                    ]
                    .into_iter()
                    .collect(),
                    Rc::default(),
                ),
                ..Item::default()
            }
            .body()
            .unwrap()
            .content
        };
        assert_eq!(body("<p>confirmé</p>".as_bytes()), "<p>confirmé</p>");
        assert_eq!(body(b"<p>confirm\xe9</p>"), "<p>confirmé</p>");
        assert_eq!(
            body("<p>\u{93}repas\u{94}</p>".as_bytes()),
            "<p>“repas”</p>"
        );
    }

    #[test]
    fn reads_property_sets_as_numbers() {
        let guid = GuidValue::new(0x0006_2002, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
        assert_eq!(property_set(&guid), PSETID_APPOINTMENT);
    }

    #[test]
    fn finds_named_properties_by_set_and_name() {
        let names: HashMap<Name, u16> = [
            (Name::Number(PSETID_APPOINTMENT, 0x8208), 0x8001),
            (Name::String(PS_PUBLIC_STRINGS, "Keywords".into()), 0x8002),
        ]
        .into_iter()
        .collect();
        let values = [
            (0x8001, Value::Text(" Studio ".into())),
            (
                0x8002,
                Value::Texts(vec!["Famille".into(), " ".into(), "Amis".into()]),
            ),
        ]
        .into_iter()
        .collect();
        let props = Properties::new(values, Rc::new(names));
        assert_eq!(
            props
                .named(PSETID_APPOINTMENT, 0x8208)
                .and_then(Value::as_text),
            Some("Studio")
        );
        assert_eq!(props.categories(), ["Famille", "Amis"]);
        assert_eq!(props.named(PSETID_ADDRESS, 0x8208), None);
    }
}
