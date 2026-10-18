//! Calendar events, contacts and To Do tasks, read-only, as Graph JSON.

use serde_json::Value;

use crate::{GraphClient, Result, Transport, encode};

/// The To Do list that mirrors flagged messages: not tasks of their own.
pub const FLAGGED_EMAILS_LIST: &str = "flaggedEmails";

impl<T: Transport> GraphClient<T> {
    /// Events of the default calendar: single events and series masters (their
    /// occurrences are not listed), times in UTC.
    pub fn calendar_events(&self) -> Result<Vec<Value>> {
        self.get_all("/me/calendar/events?$top=100")
    }

    /// One event, its times written in `time_zone`.
    pub fn event_in(&self, id: &str, time_zone: &str) -> Result<Value> {
        self.get_in(&format!("/me/events/{}", encode(id)), Some(time_zone))
    }

    /// Contacts of the default contact folder.
    pub fn contacts(&self) -> Result<Vec<Value>> {
        self.get_all("/me/contacts?$top=100")
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
