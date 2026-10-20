//! Calendar events, contacts and To Do tasks, read-only, as Graph JSON.

use std::collections::HashSet;

use serde_json::Value;

use crate::{GraphClient, Result, Transport, encode};

/// The To Do list that mirrors flagged messages: not tasks of their own.
pub const FLAGGED_EMAILS_LIST: &str = "flaggedEmails";

impl<T: Transport> GraphClient<T> {
    /// Events of the default calendar: single events and series masters (their
    /// occurrences are not listed), times in UTC.
    pub fn calendar_events(&self) -> Result<Vec<Value>> {
        self.events_in(None)
    }

    /// Every element of a collection. Graph pages by offset, and an element
    /// changed during the read (a contact whose photo the server is still
    /// processing) can slip between two pages: the ids are read again until a
    /// pass misses none, and the elements missed are read one by one, at
    /// `item(id)`.
    fn get_all_complete(
        &self,
        collection: &str,
        item: &dyn Fn(&str) -> String,
    ) -> Result<Vec<Value>> {
        let mut values = self.get_all(&format!("{collection}?$top=100"))?;
        let mut seen: HashSet<String> = values
            .iter()
            .filter_map(|v| v["id"].as_str().map(str::to_owned))
            .collect();
        for _ in 0..3 {
            let missed: Vec<String> = self
                .get_all(&format!("{collection}?$top=1000&$select=id"))?
                .iter()
                .filter_map(|v| v["id"].as_str())
                .filter(|id| !seen.contains(*id))
                .map(str::to_owned)
                .collect();
            if missed.is_empty() {
                break;
            }
            for id in missed {
                values.push(self.get(&item(&id))?);
                seen.insert(id);
            }
        }
        Ok(values)
    }

    /// One event, its times written in `time_zone`.
    pub fn event_in(&self, id: &str, time_zone: &str) -> Result<Value> {
        self.get_in(&format!("/me/events/{}", encode(id)), Some(time_zone))
    }

    /// Contacts of the default contact folder.
    pub fn contacts(&self) -> Result<Vec<Value>> {
        self.contacts_in(None)
    }

    /// The id of the element of `collection` (`/me/calendars`,
    /// `/me/contactFolders`, `/me/todo/lists`) whose `field` is `name`.
    pub fn find_named(&self, collection: &str, field: &str, name: &str) -> Result<Option<String>> {
        Ok(self
            .get_all(&format!("{collection}?$top=100"))?
            .iter()
            .find(|e| e[field].as_str() == Some(name))
            .and_then(|e| e["id"].as_str())
            .map(str::to_owned))
    }

    /// Events of a calendar, the default one when `None`: single events and
    /// series masters, times in UTC.
    pub fn events_in(&self, calendar_id: Option<&str>) -> Result<Vec<Value>> {
        let collection = match calendar_id {
            Some(id) => format!("/me/calendars/{}/events", encode(id)),
            None => "/me/calendar/events".to_owned(),
        };
        self.get_all_complete(&collection, &|id| format!("/me/events/{}", encode(id)))
    }

    /// Contacts of a contact folder, the default one when `None`.
    pub fn contacts_in(&self, folder_id: Option<&str>) -> Result<Vec<Value>> {
        let collection = match folder_id {
            Some(id) => format!("/me/contactFolders/{}/contacts", encode(id)),
            None => "/me/contacts".to_owned(),
        };
        self.get_all_complete(&collection, &|id| format!("{collection}/{}", encode(id)))
    }

    /// The id of the default To Do list.
    pub fn default_todo_list(&self) -> Result<Option<String>> {
        Ok(self
            .todo_lists()?
            .iter()
            .find(|list| list["wellknownListName"].as_str() == Some("defaultList"))
            .and_then(|list| list["id"].as_str())
            .map(str::to_owned))
    }

    /// The photo of a contact of a contact folder (the default one when
    /// `None`), or `None` when it has none.
    pub fn contact_photo_in(&self, folder_id: Option<&str>, id: &str) -> Result<Option<Vec<u8>>> {
        let url = match folder_id {
            Some(folder) => format!(
                "/me/contactFolders/{}/contacts/{}/photo/$value",
                encode(folder),
                encode(id)
            ),
            None => format!("/me/contacts/{}/photo/$value", encode(id)),
        };
        match self.get_raw(&url) {
            Ok(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
            Ok(_) | Err(crate::Error::Graph { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// A contact's photo, or `None` when it has none.
    pub fn contact_photo(&self, id: &str) -> Result<Option<Vec<u8>>> {
        self.contact_photo_in(None, id)
    }

    /// Contact folders under the default one, with how many contacts each holds.
    pub fn contact_folders(&self) -> Result<Vec<(String, usize)>> {
        let mut folders = Vec::new();
        for folder in self.get_all("/me/contactFolders?$top=100")? {
            let id = folder["id"].as_str().unwrap_or_default();
            let count = self
                .get_all(&format!(
                    "/me/contactFolders/{}/contacts?$top=100&$select=id",
                    encode(id)
                ))?
                .len();
            folders.push((
                folder["displayName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                count,
            ));
        }
        Ok(folders)
    }

    /// Inbox rules, in order. Needs the `MailboxSettings.Read` scope.
    pub fn inbox_rules(&self) -> Result<Vec<Value>> {
        let mut rules = self.get_all("/me/mailFolders/inbox/messageRules")?;
        rules.sort_by_key(|rule| rule["sequence"].as_i64().unwrap_or(i64::MAX));
        Ok(rules)
    }

    /// The master category list. Needs the `MailboxSettings.Read` scope.
    pub fn master_categories(&self) -> Result<Vec<Value>> {
        self.get_all("/me/outlook/masterCategories")
    }

    /// The path of a mail folder from the top of the mailbox (`/`-separated,
    /// with `/` in names escaped as in [`ecm_model::path`]).
    pub fn folder_path(&self, id: &str) -> Result<String> {
        let top = self.get("/me/mailFolders/msgfolderroot?$select=id")?;
        let top = top["id"].as_str().unwrap_or_default().to_owned();
        let mut names = Vec::new();
        let mut current = id.to_owned();
        for _ in 0..32 {
            let folder = self.get(&format!(
                "/me/mailFolders/{}?$select=displayName,parentFolderId",
                encode(&current)
            ))?;
            names.push(
                folder["displayName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            match folder["parentFolderId"].as_str() {
                Some(parent) if parent != top && parent != current => current = parent.to_owned(),
                _ => break,
            }
        }
        Ok(names.iter().rev().fold(String::new(), |path, name| {
            ecm_model::path::join(&path, name)
        }))
    }

    /// To Do lists.
    pub fn todo_lists(&self) -> Result<Vec<Value>> {
        self.get_all("/me/todo/lists?$top=100")
    }

    /// Tasks of a To Do list.
    pub fn todo_tasks(&self, list_id: &str) -> Result<Vec<Value>> {
        self.get_all(&format!(
            "/me/todo/lists/{}/tasks?$top=100",
            encode(list_id)
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::{GraphClient, testing::FakeTransport};

    #[test]
    fn reads_again_the_contacts_that_slipped_between_two_pages() {
        let fake = FakeTransport::new([
            (200, r#"{"value":[{"id":"C1","displayName":"Ann"}]}"#.into()),
            (200, r#"{"value":[{"id":"C1"},{"id":"C2"}]}"#.into()),
            (200, r#"{"id":"C2","displayName":"Bob"}"#.into()),
            (200, r#"{"value":[{"id":"C1"},{"id":"C2"}]}"#.into()),
        ]);
        let client = GraphClient::new(&fake, "T".into()).with_sleep(|_| {});
        let contacts = client.contacts_in(Some("F")).unwrap();
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[1]["displayName"], "Bob");
        let requests = fake.requests.borrow();
        assert!(
            requests[1]
                .1
                .ends_with("/me/contactFolders/F/contacts?$top=1000&$select=id")
        );
        assert!(requests[2].1.ends_with("/me/contactFolders/F/contacts/C2"));
        assert_eq!(requests.len(), 4);
    }

    #[test]
    fn a_contact_without_photo_has_none() {
        let fake = FakeTransport::new([
            (
                404,
                r#"{"error":{"code":"ErrorItemNotFound","message":"The photo wasn't found."}}"#
                    .into(),
            ),
            (200, "JPEG".into()),
        ]);
        let client = GraphClient::new(&fake, "T".into()).with_sleep(|_| {});
        assert_eq!(client.contact_photo("C1").unwrap(), None);
        assert_eq!(client.contact_photo("C2").unwrap(), Some(b"JPEG".to_vec()));
        assert_eq!(
            fake.requests.borrow()[0].1,
            "https://graph.microsoft.com/v1.0/me/contacts/C1/photo/$value"
        );
    }

    #[test]
    fn asks_for_local_times_when_reading_one_event() {
        let fake = FakeTransport::new([(200, r#"{"id":"E"}"#.into())]);
        let event = GraphClient::new(&fake, "T".into())
            .with_sleep(|_| {})
            .event_in("E", "Romance Standard Time")
            .unwrap();
        assert_eq!(event["id"], "E");
        let requests = fake.requests.borrow();
        assert_eq!(
            requests[0].1,
            "https://graph.microsoft.com/v1.0/me/events/E"
        );
        assert!(requests[0].3.iter().any(|(name, value)| name == "Prefer"
            && value.contains("outlook.timezone=\"Romance Standard Time\"")));
    }
}
