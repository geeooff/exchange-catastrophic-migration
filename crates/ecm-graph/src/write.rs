//! Every change this crate makes to a mailbox: folders, messages, their files,
//! moves; calendars, events, contact folders, contacts and their photos, To Do
//! lists and tasks, inbox rules, categories. Nothing is ever deleted.

use base64::Engine as _;
use serde_json::{Value, json};

use crate::{Error, GraphClient, Result, Transport, encode};

/// Files up to this size go in one request; larger ones through an upload
/// session (Graph takes requests of about 4 MB).
pub const INLINE_FILE_LIMIT: usize = 3 * 1024 * 1024;

/// Bytes per upload session request: a multiple of 320 KiB, as Graph asks.
const UPLOAD_CHUNK: usize = 10 * 320 * 1024;

impl<T: Transport> GraphClient<T> {
    fn send_json(&self, method: &str, url: &str, body: &Value) -> Result<Value> {
        let body = serde_json::to_vec(body).map_err(|e| Error::MalformedResponse(e.to_string()))?;
        let (_, answer) = self.request(
            method,
            url,
            "application/json",
            Some("application/json"),
            &body,
            None,
        )?;
        if answer.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&answer).map_err(|e| Error::MalformedResponse(e.to_string()))
    }

    fn id_of(value: &Value) -> Result<String> {
        value["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::MalformedResponse("created item without id".into()))
    }

    /// The id of the folder `name` under `parent` (the top of the mailbox when
    /// `None`), created when missing.
    pub fn ensure_folder(&self, parent: Option<&str>, name: &str) -> Result<String> {
        let collection = match parent {
            Some(parent) => format!("/me/mailFolders/{}/childFolders", encode(parent)),
            None => "/me/mailFolders".to_owned(),
        };
        let filter = format!("displayName eq '{}'", name.replace('\'', "''"));
        let found = self.get_all(&format!(
            "{collection}?includeHiddenFolders=true&$select=id,displayName&$filter={}",
            encode(&filter)
        ))?;
        if let Some(folder) = found.first() {
            return Self::id_of(folder);
        }
        Self::id_of(&self.send_json("POST", &collection, &json!({ "displayName": name }))?)
    }

    /// Creates a message in a folder from its MIME content. Graph may keep such a
    /// message as a draft: see [`create_message`](Self::create_message).
    pub fn create_from_mime(&self, folder_id: &str, mime: &[u8]) -> Result<String> {
        let body = base64::engine::general_purpose::STANDARD.encode(mime);
        let (_, answer) = self.request(
            "POST",
            &format!("/me/mailFolders/{}/messages", encode(folder_id)),
            "application/json",
            Some("text/plain"),
            body.as_bytes(),
            None,
        )?;
        let value: Value =
            serde_json::from_slice(&answer).map_err(|e| Error::MalformedResponse(e.to_string()))?;
        Self::id_of(&value)
    }

    /// Creates a message in a folder from its JSON resource, which can carry
    /// extended properties only settable at creation (the message flags that
    /// make it a received or sent message rather than a draft).
    pub fn create_message(&self, folder_id: &str, message: &Value) -> Result<String> {
        Self::id_of(&self.send_json(
            "POST",
            &format!("/me/mailFolders/{}/messages", encode(folder_id)),
            message,
        )?)
    }

    /// Attaches a file to a message: in one request up to
    /// [`INLINE_FILE_LIMIT`], else through an upload session. A file with a
    /// `Content-ID` is inline: the HTML body shows it.
    ///
    /// Right after a file, the mailbox may still be storing it: the change key
    /// conflict it answers then passes after a pause (5 s, then 10, 20, 40).
    pub fn attach_file(
        &self,
        message_id: &str,
        name: &str,
        content_type: &str,
        content_id: Option<&str>,
        data: &[u8],
    ) -> Result<()> {
        let mut wait = std::time::Duration::from_secs(5);
        loop {
            match self.attach_once(message_id, name, content_type, content_id, data) {
                Err(Error::Graph { code, message, .. })
                    if (code == "ErrorIrresolvableConflict"
                        || message.contains("ErrorIrresolvableConflict"))
                        && wait <= std::time::Duration::from_secs(40) =>
                {
                    (self.sleep)(wait);
                    wait *= 2;
                }
                other => return other,
            }
        }
    }

    fn attach_once(
        &self,
        message_id: &str,
        name: &str,
        content_type: &str,
        content_id: Option<&str>,
        data: &[u8],
    ) -> Result<()> {
        let base = format!("/me/messages/{}/attachments", encode(message_id));
        if data.len() <= INLINE_FILE_LIMIT {
            let mut file = json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": name,
                "contentType": content_type,
                "contentBytes": base64::engine::general_purpose::STANDARD.encode(data),
                "isInline": content_id.is_some(),
            });
            if let Some(id) = content_id {
                file["contentId"] = json!(id);
            }
            self.send_json("POST", &base, &file)?;
            return Ok(());
        }
        let mut item = json!({
            "attachmentType": "file",
            "name": name,
            "size": data.len(),
            "contentType": content_type,
            "isInline": content_id.is_some(),
        });
        if let Some(id) = content_id {
            item["contentId"] = json!(id);
        }
        let session = self.send_json(
            "POST",
            &format!("{base}/createUploadSession"),
            &json!({ "AttachmentItem": item }),
        )?;
        let upload = session["uploadUrl"]
            .as_str()
            .ok_or_else(|| Error::MalformedResponse("upload session without uploadUrl".into()))?;
        // The upload URL carries its own authorization: no bearer token.
        for (index, chunk) in data.chunks(UPLOAD_CHUNK).enumerate() {
            let start = index * UPLOAD_CHUNK;
            let range = format!("bytes {start}-{}/{}", start + chunk.len() - 1, data.len());
            let headers = [
                ("Content-Type", "application/octet-stream"),
                ("Content-Range", range.as_str()),
            ];
            let response = self.transport.send("PUT", upload, &headers, chunk)?;
            if !(200..300).contains(&response.status) {
                return Err(Error::Graph {
                    status: response.status,
                    code: "UploadFailed".into(),
                    message: String::from_utf8_lossy(&response.body)
                        .chars()
                        .take(300)
                        .collect(),
                });
            }
        }
        Ok(())
    }

    /// Moves a message to a folder and returns its new id.
    pub fn move_message(&self, message_id: &str, folder_id: &str) -> Result<String> {
        Self::id_of(&self.send_json(
            "POST",
            &format!("/me/messages/{}/move", encode(message_id)),
            &json!({ "destinationId": folder_id }),
        )?)
    }

    /// The id of the element of `collection` whose `field` is `name`, created
    /// when missing.
    fn ensure_named(&self, collection: &str, field: &str, name: &str) -> Result<String> {
        if let Some(id) = self.find_named(collection, field, name)? {
            return Ok(id);
        }
        Self::id_of(&self.send_json("POST", collection, &json!({ field: name }))?)
    }

    /// The id of the calendar `name`, created when missing.
    pub fn ensure_calendar(&self, name: &str) -> Result<String> {
        self.ensure_named("/me/calendars", "name", name)
    }

    /// The id of the contact folder `name`, created when missing.
    pub fn ensure_contact_folder(&self, name: &str) -> Result<String> {
        self.ensure_named("/me/contactFolders", "displayName", name)
    }

    /// The id of the To Do list `name`, created when missing.
    pub fn ensure_todo_list(&self, name: &str) -> Result<String> {
        self.ensure_named("/me/todo/lists", "displayName", name)
    }

    /// Creates an event in a calendar, the default one when `None`. An event
    /// without attendees sends no invitation; with attendees, Graph would send
    /// them one.
    pub fn create_event(&self, calendar_id: Option<&str>, event: &Value) -> Result<String> {
        let url = match calendar_id {
            Some(id) => format!("/me/calendars/{}/events", encode(id)),
            None => "/me/events".to_owned(),
        };
        Self::id_of(&self.send_json("POST", &url, event)?)
    }

    /// Creates a contact in a contact folder, the default one when `None`.
    pub fn create_contact(&self, folder_id: Option<&str>, contact: &Value) -> Result<String> {
        let url = match folder_id {
            Some(id) => format!("/me/contactFolders/{}/contacts", encode(id)),
            None => "/me/contacts".to_owned(),
        };
        Self::id_of(&self.send_json("POST", &url, contact)?)
    }

    /// Sets the photo (JPEG or PNG bytes) of a contact of a contact folder, the
    /// default one when `None`.
    pub fn set_contact_photo(
        &self,
        folder_id: Option<&str>,
        contact_id: &str,
        photo: &[u8],
    ) -> Result<()> {
        let content_type = if photo.starts_with(b"\x89PNG") {
            "image/png"
        } else {
            "image/jpeg"
        };
        let url = match folder_id {
            Some(folder) => format!(
                "/me/contactFolders/{}/contacts/{}/photo/$value",
                encode(folder),
                encode(contact_id)
            ),
            None => format!("/me/contacts/{}/photo/$value", encode(contact_id)),
        };
        self.request(
            "PUT",
            &url,
            "application/json",
            Some(content_type),
            photo,
            None,
        )
        .map(|_| ())
    }

    /// Creates an inbox rule. Needs the `MailboxSettings.ReadWrite` scope.
    pub fn create_inbox_rule(&self, rule: &Value) -> Result<String> {
        Self::id_of(&self.send_json("POST", "/me/mailFolders/inbox/messageRules", rule)?)
    }

    /// Creates a category of the master list, with a color `none` or `preset0`
    /// to `preset24`. Needs the `MailboxSettings.ReadWrite` scope.
    pub fn create_category(&self, name: &str, color: &str) -> Result<String> {
        Self::id_of(&self.send_json(
            "POST",
            "/me/outlook/masterCategories",
            &json!({ "displayName": name, "color": color }),
        )?)
    }

    /// Creates a task in a To Do list.
    pub fn create_task(&self, list_id: &str, task: &Value) -> Result<String> {
        Self::id_of(&self.send_json(
            "POST",
            &format!("/me/todo/lists/{}/tasks", encode(list_id)),
            task,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{GraphClient, testing::FakeTransport};

    #[test]
    fn finds_or_creates_a_folder() {
        let fake = FakeTransport::new([
            (200, r#"{"value":[]}"#.into()),
            (201, r#"{"id":"F1","displayName":"ecm-test"}"#.into()),
            (200, r#"{"value":[{"id":"F2","displayName":"Sub"}]}"#.into()),
        ]);
        let client = GraphClient::new(&fake, "t".into());
        assert_eq!(client.ensure_folder(None, "ecm-test").unwrap(), "F1");
        assert_eq!(client.ensure_folder(Some("F1"), "Sub").unwrap(), "F2");
        let requests = fake.requests.borrow();
        assert!(
            requests[0]
                .1
                .contains("/me/mailFolders?includeHiddenFolders=true")
        );
        assert!(requests[0].1.contains("displayName%20eq%20%27ecm-test%27"));
        assert_eq!(requests[1].0, "POST");
        assert_eq!(requests[1].2, r#"{"displayName":"ecm-test"}"#);
        assert!(requests[2].1.contains("/me/mailFolders/F1/childFolders?"));
    }

    #[test]
    fn creates_messages_from_mime_and_json() {
        let fake = FakeTransport::new([
            (201, r#"{"id":"M1"}"#.into()),
            (201, r#"{"id":"M2"}"#.into()),
        ]);
        let client = GraphClient::new(&fake, "t".into());
        assert_eq!(
            client
                .create_from_mime("F", b"Subject: x\r\n\r\nhi")
                .unwrap(),
            "M1"
        );
        assert_eq!(
            client
                .create_message("F", &json!({ "subject": "x" }))
                .unwrap(),
            "M2"
        );
        let requests = fake.requests.borrow();
        assert_eq!(requests[0].2, "U3ViamVjdDogeA0KDQpoaQ==");
        assert!(
            requests[0]
                .3
                .contains(&("Content-Type".into(), "text/plain".into()))
        );
        assert!(
            requests[1]
                .3
                .contains(&("Content-Type".into(), "application/json".into()))
        );
        assert!(requests[1].1.ends_with("/me/mailFolders/F/messages"));
    }

    #[test]
    fn sends_a_large_file_through_an_upload_session() {
        let size = super::UPLOAD_CHUNK + 10;
        let fake = FakeTransport::new([
            (
                200,
                r#"{"uploadUrl":"https://outlook.office.com/upload/1"}"#.into(),
            ),
            (200, "{}".into()),
            (201, "{}".into()),
        ]);
        let client = GraphClient::new(&fake, "t".into());
        client
            .attach_file("M1", "v.mp4", "video/mp4", None, &vec![7u8; size])
            .unwrap();
        let requests = fake.requests.borrow();
        let range = |i: usize| {
            requests[i]
                .3
                .iter()
                .find(|(name, _)| name == "Content-Range")
                .map(|(_, value)| value.clone())
        };
        assert!(
            requests[0]
                .1
                .ends_with("/me/messages/M1/attachments/createUploadSession")
        );
        assert!(requests[0].2.contains(&format!(r#""size":{size}"#)));
        assert_eq!(requests[1].0, "PUT");
        assert_eq!(requests[1].1, "https://outlook.office.com/upload/1");
        assert!(
            !requests[1]
                .3
                .iter()
                .any(|(name, _)| name == "Authorization")
        );
        assert_eq!(range(1), Some(format!("bytes 0-3276799/{size}")));
        assert_eq!(range(2), Some(format!("bytes 3276800-{}/{size}", size - 1)));
    }

    #[test]
    fn retries_a_file_while_the_previous_one_is_being_stored() {
        let size = super::INLINE_FILE_LIMIT + 10;
        let conflict = r#"{"error":{"code":"ErrorIrresolvableConflict","message":"The change key passed in the request does not match the current change key for the item."}}"#;
        let fake = FakeTransport::new([
            (
                200,
                r#"{"uploadUrl":"https://outlook.office.com/upload/1"}"#.into(),
            ),
            (412, conflict.into()),
            (
                200,
                r#"{"uploadUrl":"https://outlook.office.com/upload/2"}"#.into(),
            ),
            (201, "{}".into()),
        ]);
        GraphClient::new(&fake, "t".into())
            .with_sleep(|_| {})
            .attach_file("M1", "a.mp3", "audio/mpeg", None, &vec![1u8; size])
            .unwrap();
        let requests = fake.requests.borrow();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[3].1, "https://outlook.office.com/upload/2");
    }

    #[test]
    fn attaches_a_small_inline_file_in_one_request() {
        let fake = FakeTransport::new([(201, r#"{"id":"A1"}"#.into())]);
        GraphClient::new(&fake, "t".into())
            .attach_file("M1", "logo.png", "image/png", Some("logo@x"), b"png")
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&fake.requests.borrow()[0].2).unwrap();
        assert_eq!(body["contentBytes"], "cG5n");
        assert_eq!(body["isInline"], true);
        assert_eq!(body["contentId"], "logo@x");
    }

    #[test]
    fn creates_containers_once_and_items_in_them() {
        let fake = FakeTransport::new([
            (200, r#"{"value":[{"id":"C0","name":"Calendar"}]}"#.into()),
            (201, r#"{"id":"C1","name":"ecm-test"}"#.into()),
            (
                200,
                r#"{"value":[{"id":"F1","displayName":"ecm-test"}]}"#.into(),
            ),
            (200, r#"{"value":[]}"#.into()),
            (201, r#"{"id":"L1"}"#.into()),
            (201, r#"{"id":"E1"}"#.into()),
            (201, r#"{"id":"P1"}"#.into()),
            (200, String::new()),
            (201, r#"{"id":"T1"}"#.into()),
        ]);
        let client = GraphClient::new(&fake, "t".into());
        assert_eq!(client.ensure_calendar("ecm-test").unwrap(), "C1");
        assert_eq!(client.ensure_contact_folder("ecm-test").unwrap(), "F1");
        assert_eq!(client.ensure_todo_list("ecm-test").unwrap(), "L1");
        assert_eq!(
            client
                .create_event(Some("C1"), &json!({"subject": "x"}))
                .unwrap(),
            "E1"
        );
        assert_eq!(
            client
                .create_contact(Some("F1"), &json!({"displayName": "Ann"}))
                .unwrap(),
            "P1"
        );
        client
            .set_contact_photo(Some("F1"), "P1", b"\xff\xd8jpeg")
            .unwrap();
        assert_eq!(
            client.create_task("L1", &json!({"title": "t"})).unwrap(),
            "T1"
        );
        let requests = fake.requests.borrow();
        assert_eq!(requests[1].0, "POST");
        assert!(requests[1].1.ends_with("/me/calendars"));
        assert_eq!(requests[1].2, r#"{"name":"ecm-test"}"#);
        assert!(requests[2].1.contains("/me/contactFolders?"));
        assert_eq!(requests[4].2, r#"{"displayName":"ecm-test"}"#);
        assert!(requests[5].1.ends_with("/me/calendars/C1/events"));
        assert!(requests[6].1.ends_with("/me/contactFolders/F1/contacts"));
        assert_eq!(requests[7].0, "PUT");
        assert!(
            requests[7]
                .1
                .ends_with("/me/contactFolders/F1/contacts/P1/photo/$value")
        );
        assert!(
            requests[7]
                .3
                .iter()
                .any(|(n, v)| n == "Content-Type" && v == "image/jpeg")
        );
        assert!(requests[8].1.ends_with("/me/todo/lists/L1/tasks"));
    }

    #[test]
    fn creates_rules_and_categories() {
        let fake = FakeTransport::new([
            (201, r#"{"id":"R1"}"#.into()),
            (201, r#"{"id":"K1"}"#.into()),
        ]);
        let client = GraphClient::new(&fake, "t".into());
        let rule = json!({"displayName": "ecm-test x", "isEnabled": false});
        assert_eq!(client.create_inbox_rule(&rule).unwrap(), "R1");
        assert_eq!(
            client
                .create_category("ecm-test Famille", "preset9")
                .unwrap(),
            "K1"
        );
        let requests = fake.requests.borrow();
        assert!(
            requests[0]
                .1
                .ends_with("/me/mailFolders/inbox/messageRules")
        );
        assert!(requests[1].1.ends_with("/me/outlook/masterCategories"));
        assert_eq!(
            requests[1].2,
            r#"{"color":"preset9","displayName":"ecm-test Famille"}"#
        );
    }
}
