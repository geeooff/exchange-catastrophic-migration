//! Mail folders and messages, read-only.

use ecm_model::{
    FolderSummary, ItemRecord,
    compare::{PropertyMap, address_list},
    item::parse_utc,
};
use serde_json::Value;

use crate::{Error, GraphClient, Result, Transport, encode};

/// A mail folder as Graph describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFolder {
    /// Graph folder id.
    pub id: String,
    /// Path from the top of the mailbox, `/`-separated.
    pub path: String,
    /// Display name.
    pub name: String,
    /// Number of items.
    pub total: u64,
    /// Number of unread items.
    pub unread: u64,
    /// Sub-folders, sorted by name.
    pub children: Vec<GraphFolder>,
}

impl GraphFolder {
    /// The tree as a [`FolderSummary`].
    pub fn summary(&self) -> FolderSummary {
        FolderSummary {
            name: self.name.clone(),
            class: None,
            item_count: self.total,
            unread_count: self.unread,
            associated_count: 0,
            size: None,
            children: self.children.iter().map(GraphFolder::summary).collect(),
        }
    }

    /// This folder and every folder below it, depth first.
    pub fn flatten(&self) -> Vec<&GraphFolder> {
        let mut out = vec![self];
        for child in &self.children {
            out.extend(child.flatten());
        }
        out
    }
}

const FOLDER_SELECT: &str = "id,displayName,childFolderCount,totalItemCount,unreadItemCount";

/// `PR_MESSAGE_CLASS`, `PR_MESSAGE_SIZE`, `PR_NATIVE_BODY_INFO`.
const EXTENDED: &str = "id eq 'String 0x001A' or id eq 'Integer 0x0E08' or id eq 'Integer 0x1016'";

impl<T: Transport> GraphClient<T> {
    /// The signed-in mailbox's address.
    pub fn mailbox(&self) -> Result<String> {
        let me = self.get("/me?$select=mail,userPrincipalName")?;
        Ok(me["mail"]
            .as_str()
            .or_else(|| me["userPrincipalName"].as_str())
            .unwrap_or_default()
            .to_owned())
    }

    /// Every mail folder, hidden ones included, under a root without items of its
    /// own (the top of the mailbox).
    pub fn mail_folders(&self) -> Result<GraphFolder> {
        let children = self.child_folders("/me/mailFolders", "")?;
        Ok(GraphFolder {
            id: String::new(),
            path: String::new(),
            name: String::new(),
            total: 0,
            unread: 0,
            children,
        })
    }

    fn child_folders(&self, collection: &str, parent_path: &str) -> Result<Vec<GraphFolder>> {
        let url =
            format!("{collection}?includeHiddenFolders=true&$top=100&$select={FOLDER_SELECT}");
        let mut folders = Vec::new();
        for value in self.get_all(&url)? {
            let id = value["id"]
                .as_str()
                .ok_or_else(|| Error::MalformedResponse("folder without id".into()))?
                .to_owned();
            let name = value["displayName"].as_str().unwrap_or_default().to_owned();
            let path = ecm_model::path::join(parent_path, &name);
            let children = if value["childFolderCount"].as_u64().unwrap_or(0) > 0 {
                self.child_folders(
                    &format!("/me/mailFolders/{}/childFolders", encode(&id)),
                    &path,
                )?
            } else {
                Vec::new()
            };
            folders.push(GraphFolder {
                total: value["totalItemCount"].as_u64().unwrap_or(0),
                unread: value["unreadItemCount"].as_u64().unwrap_or(0),
                id,
                path,
                name,
                children,
            });
        }
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(folders)
    }

    /// Every message of a folder, with the properties an audit needs.
    pub fn inventory_messages(&self, folder: &GraphFolder) -> Result<Vec<ItemRecord>> {
        let url = format!(
            "/me/mailFolders/{}/messages?$top=500&$select={}&$expand={}",
            encode(&folder.id),
            "id,subject,internetMessageId,from,sentDateTime,receivedDateTime,createdDateTime,isRead,hasAttachments",
            encode(&format!(
                "singleValueExtendedProperties($filter={EXTENDED})"
            ))
        );
        self.get_all(&url)?
            .iter()
            .map(|message| record(message, &folder.path))
            .collect()
    }
}

impl<T: Transport> GraphClient<T> {
    /// A message's MIME content, as Outlook.com stores it.
    pub fn mime(&self, message_id: &str) -> Result<Vec<u8>> {
        self.get_raw(&format!("/me/messages/{}/$value", encode(message_id)))
    }

    /// A message's detailed, normalized properties, named as in
    /// [`ecm_model::compare::COMPARED`].
    pub fn message_properties(&self, message_id: &str) -> Result<PropertyMap> {
        let message = self.get(&format!(
            "/me/messages/{}?$select={}&$expand={}",
            encode(message_id),
            "subject,body,from,toRecipients,ccRecipients,sentDateTime,receivedDateTime,isRead,hasAttachments,internetMessageId,internetMessageHeaders",
            encode(&format!(
                "attachments($select=name),singleValueExtendedProperties($filter={EXTENDED})"
            ))
        ))?;
        Ok(message_properties(&message))
    }
}

/// Normalizes a Graph message into a [`PropertyMap`].
pub fn message_properties(message: &Value) -> PropertyMap {
    let mut map = PropertyMap::new();
    let text = |value: &Value| {
        value
            .as_str()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    };
    let addresses = |value: &Value| {
        address_list(
            value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r["emailAddress"]["address"].as_str()),
        )
    };
    let record = record(message, "").unwrap_or_default();
    map.insert("class".into(), record.class);
    if let Some(subject) = text(&message["subject"]) {
        map.insert("subject".into(), subject);
    }
    if let Some(from) = text(&message["from"]["emailAddress"]["address"]) {
        map.insert("from".into(), from.to_lowercase());
    }
    map.insert("to".into(), addresses(&message["toRecipients"]));
    map.insert("cc".into(), addresses(&message["ccRecipients"]));
    for (name, value) in [("sent", record.sent), ("received", record.received)] {
        if let Some(value) = value {
            map.insert(name.into(), value.to_string());
        }
    }
    if let Some(id) = record.message_id {
        map.insert("message_id".into(), id);
    }
    if let Some(read) = record.read {
        map.insert("read".into(), read.to_string());
    }
    if let Some(has) = record.has_attachments {
        map.insert("has_attachments".into(), has.to_string());
    }
    let mut names: Vec<String> = message["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str())
        .map(|n| n.trim().to_owned())
        .collect();
    names.sort();
    map.insert("attachments".into(), names.join(";"));
    let body = &message["body"];
    if let Some(kind) = body["contentType"].as_str() {
        map.insert("body_type".into(), kind.to_lowercase());
    }
    if let Some(content) = body["content"].as_str() {
        map.insert("body_length".into(), content.chars().count().to_string());
    }
    map.insert(
        "headers".into(),
        message["internetMessageHeaders"]
            .as_array()
            .is_some_and(|h| !h.is_empty())
            .to_string(),
    );
    if let Some(native) = record.native_body {
        map.insert("native_body".into(), native.to_string());
    }
    map
}

fn record(message: &Value, folder: &str) -> Result<ItemRecord> {
    let text = |value: &Value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let mut class = None;
    let mut size = None;
    let mut native_body = None;
    for property in message["singleValueExtendedProperties"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let id = property["id"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let value = property["value"].as_str().unwrap_or_default();
        // Graph writes tags back without leading zeros: "Integer 0xe08".
        let tag = id
            .rsplit_once("0x")
            .and_then(|(_, hex)| u32::from_str_radix(hex, 16).ok());
        match tag {
            Some(0x001A) => class = Some(value.to_owned()),
            Some(0x0E08) => size = value.parse().ok(),
            Some(0x1016) => native_body = value.parse().ok(),
            _ => {}
        }
    }
    let address = &message["from"]["emailAddress"];
    let sender = text(&address["address"])
        .filter(|address| address.contains('@'))
        .map(|address| address.to_lowercase())
        .or_else(|| text(&address["name"]));
    Ok(ItemRecord {
        key: text(&message["id"])
            .ok_or_else(|| Error::MalformedResponse("message without id".into()))?,
        folder: folder.to_owned(),
        class: class.unwrap_or_else(|| "IPM.Note".to_owned()),
        message_id: text(&message["internetMessageId"]),
        subject: text(&message["subject"]),
        sender,
        sent: text(&message["sentDateTime"]).and_then(|t| parse_utc(&t)),
        received: text(&message["receivedDateTime"]).and_then(|t| parse_utc(&t)),
        created: text(&message["createdDateTime"]).and_then(|t| parse_utc(&t)),
        size,
        has_attachments: message["hasAttachments"].as_bool(),
        read: message["isRead"].as_bool(),
        has_headers: None,
        native_body,
    })
}

#[cfg(test)]
mod tests {
    use crate::{GraphClient, testing::FakeTransport};

    #[test]
    fn normalizes_message_properties() {
        let message: serde_json::Value = serde_json::from_str(r#"{"id":"M","subject":"Hi","body":{"contentType":"html","content":"<p>Hi</p>"},
            "from":{"emailAddress":{"address":"A@x.test"}},"toRecipients":[{"emailAddress":{"address":"c@x.test"}},{"emailAddress":{"address":"B@x.test"}}],
            "ccRecipients":[],"sentDateTime":"2019-11-14T10:00:00Z","receivedDateTime":"2019-11-14T10:00:01Z","isRead":false,"hasAttachments":true,
            "internetMessageId":"<1@x>","internetMessageHeaders":[{"name":"Received","value":"x"}],"attachments":[{"name":"b.pdf"},{"name":"a.jpg"}]}"#).unwrap();
        let map = super::message_properties(&message);
        assert_eq!(map["body_type"], "html");
        assert_eq!(map["body_length"], "9");
        assert_eq!(map["to"], "b@x.test;c@x.test");
        assert_eq!(map["attachments"], "a.jpg;b.pdf");
        assert_eq!(
            (map["read"].as_str(), map["headers"].as_str()),
            ("false", "true")
        );
        assert_eq!(
            map["sent"],
            ecm_model::item::parse_utc("2019-11-14T10:00:00Z")
                .unwrap()
                .to_string()
        );
    }

    #[test]
    fn reads_the_folder_tree_and_messages() {
        let fake = FakeTransport::new([
            (
                200,
                r#"{"value":[
                    {"id":"SENT","displayName":"Éléments envoyés","childFolderCount":0,"totalItemCount":4400,"unreadItemCount":0},
                    {"id":"ARCH","displayName":"Archive","childFolderCount":1,"totalItemCount":67,"unreadItemCount":0}]}"#
                    .into(),
            ),
            (
                200,
                r#"{"value":[{"id":"BANK","displayName":"Banque","childFolderCount":0,"totalItemCount":83,"unreadItemCount":1}]}"#.into(),
            ),
            (
                200,
                r#"{"value":[{"id":"M1","subject":"RE: Urgent.","internetMessageId":"<1@example.test>",
                    "from":{"emailAddress":{"name":"Me","address":"Me@Example.TEST"}},
                    "sentDateTime":"2019-11-14T10:00:00Z","receivedDateTime":"2019-11-14T10:00:01Z","createdDateTime":"2019-11-14T10:00:01Z",
                    "isRead":true,"hasAttachments":false,
                    "singleValueExtendedProperties":[{"id":"String 0x1a","value":"IPM.Note"},{"id":"Integer 0xe08","value":"5120"},{"id":"Integer 0x1016","value":"3"}]}]}"#
                    .into(),
            ),
        ]);
        let client = GraphClient::new(&fake, "t".into());
        let tree = client.mail_folders().unwrap();
        let paths: Vec<_> = tree.flatten().iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths, ["", "Archive", "Archive/Banque", "Éléments envoyés"]);
        assert_eq!(tree.summary().total_item_count(), 4400 + 67 + 83);

        let sent = tree.flatten()[3].clone();
        let items = client.inventory_messages(&sent).unwrap();
        let item = &items[0];
        assert_eq!(item.folder, "Éléments envoyés");
        assert_eq!(item.sender.as_deref(), Some("me@example.test"));
        assert_eq!(
            (item.size, item.native_body, item.read),
            (Some(5120), Some(3), Some(true))
        );
        assert_eq!(item.class, "IPM.Note");

        let requests = fake.requests.borrow();
        assert!(requests[0].1.starts_with(
            "https://graph.microsoft.com/v1.0/me/mailFolders?includeHiddenFolders=true"
        ));
        assert!(requests[1].1.contains("/me/mailFolders/ARCH/childFolders"));
        assert!(
            requests[2]
                .1
                .contains("/me/mailFolders/SENT/messages?$top=500")
        );
        assert!(requests[2].1.contains(
            "singleValueExtendedProperties%28%24filter%3Did%20eq%20%27String%200x001A%27"
        ));
    }
}
