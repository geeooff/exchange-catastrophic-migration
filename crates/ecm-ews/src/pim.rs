//! Brief listings of calendar items and contacts, to avoid creating twice what a
//! folder already holds. Read-only.

use ecm_model::item::parse_utc;

use crate::{
    Error, EwsClient, Result, Transport,
    xml::{Element, escape},
};

/// A calendar item, as far as matching goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarBrief {
    /// Item id.
    pub id: String,
    /// iCalendar UID.
    pub uid: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// Start, Unix seconds.
    pub start: Option<i64>,
}

/// A contact, as far as matching goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactBrief {
    /// Item id.
    pub id: String,
    /// Display name.
    pub display_name: Option<String>,
    /// First e-mail address, lower-cased.
    pub email: Option<String>,
    /// Whether it has a photo.
    pub has_picture: bool,
}

impl<T: Transport> EwsClient<T> {
    /// Calendar items of a folder: single items and recurring masters.
    pub fn calendar_briefs(&self, folder_id: &str) -> Result<Vec<CalendarBrief>> {
        let fields = r#"<t:FieldURI FieldURI="item:Subject"/><t:FieldURI FieldURI="calendar:Start"/><t:FieldURI FieldURI="calendar:UID"/>"#;
        Ok(self
            .find_all(folder_id, fields)?
            .iter()
            .map(|item| CalendarBrief {
                id: item_id(item),
                uid: text(item, "UID"),
                subject: text(item, "Subject"),
                start: text(item, "Start").and_then(|t| parse_utc(&t)),
            })
            .collect())
    }

    /// Contacts of a folder.
    pub fn contact_briefs(&self, folder_id: &str) -> Result<Vec<ContactBrief>> {
        let fields = r#"<t:FieldURI FieldURI="contacts:DisplayName"/><t:FieldURI FieldURI="contacts:HasPicture"/><t:IndexedFieldURI FieldURI="contacts:EmailAddress" FieldIndex="EmailAddress1"/>"#;
        Ok(self
            .find_all(folder_id, fields)?
            .iter()
            .map(|item| ContactBrief {
                id: item_id(item),
                display_name: text(item, "DisplayName"),
                email: item
                    .path(&["EmailAddresses", "Entry"])
                    .map(|entry| entry.text.trim().to_lowercase())
                    .filter(|address| !address.is_empty()),
                has_picture: text(item, "HasPicture")
                    .is_some_and(|v| v.eq_ignore_ascii_case("true")),
            })
            .collect())
    }

    /// Sets a contact's photo, as the `ContactPicture.jpg` attachment Exchange
    /// and Outlook use for it.
    pub fn attach_contact_photo(&self, contact_id: &str, photo: &[u8]) -> Result<()> {
        use base64::Engine as _;
        let body = format!(
            r#"<m:CreateAttachment><m:ParentItemId Id="{}"/><m:Attachments><t:FileAttachment><t:Name>ContactPicture.jpg</t:Name><t:ContentType>image/jpeg</t:ContentType><t:IsContactPhoto>true</t:IsContactPhoto><t:Content>{}</t:Content></t:FileAttachment></m:Attachments></m:CreateAttachment>"#,
            escape(contact_id),
            base64::engine::general_purpose::STANDARD.encode(photo)
        );
        self.call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("CreateAttachment returned no message".into()))?
            .into_result()
            .map(|_| ())
    }

    fn find_all(&self, folder_id: &str, fields: &str) -> Result<Vec<Element>> {
        let mut items = Vec::new();
        loop {
            let body = format!(
                r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties>{fields}</t:AdditionalProperties></m:ItemShape><m:IndexedPageItemView MaxEntriesReturned="500" Offset="{}" BasePoint="Beginning"/><m:ParentFolderIds><t:FolderId Id="{}"/></m:ParentFolderIds></m:FindItem>"#,
                items.len(),
                escape(folder_id)
            );
            let message = self
                .call(&body)?
                .into_iter()
                .next()
                .ok_or_else(|| Error::MalformedResponse("FindItem returned no message".into()))?
                .into_result()?;
            let root = message
                .element
                .child("RootFolder")
                .ok_or_else(|| Error::MalformedResponse("FindItem without RootFolder".into()))?;
            let page: Vec<Element> = root
                .child("Items")
                .map(|i| i.children.clone())
                .unwrap_or_default();
            let last = root.attribute("IncludesLastItemInRange") != Some("false");
            let empty = page.is_empty();
            items.extend(page);
            if last || empty {
                return Ok(items);
            }
        }
    }
}

fn item_id(item: &Element) -> String {
    item.child("ItemId")
        .and_then(|id| id.attribute("Id"))
        .unwrap_or_default()
        .to_owned()
}

fn text(item: &Element, name: &str) -> Option<String> {
    item.child_text(name)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use crate::{
        EwsClient,
        testing::{FakeTransport, soap},
    };

    #[test]
    fn lists_calendar_items_and_contacts() {
        let fake = FakeTransport::new([
            (
                200,
                soap(
                    r#"<m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder IncludesLastItemInRange="true"><t:Items><t:CalendarItem><t:ItemId Id="E1"/><t:Subject>Dentist</t:Subject><t:Start>2026-09-01T08:00:00Z</t:Start><t:UID>040000008200E0</t:UID></t:CalendarItem></t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"#,
                ),
            ),
            (
                200,
                soap(
                    r#"<m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder IncludesLastItemInRange="true"><t:Items><t:Contact><t:ItemId Id="C1"/><t:DisplayName>Ann</t:DisplayName><t:HasPicture>false</t:HasPicture><t:EmailAddresses><t:Entry Key="EmailAddress1">Ann@Example.TEST</t:Entry></t:EmailAddresses></t:Contact></t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"#,
                ),
            ),
        ]);
        let client = EwsClient::new(&fake);
        let events = client.calendar_briefs("CAL").unwrap();
        assert_eq!(events[0].uid.as_deref(), Some("040000008200E0"));
        assert_eq!(
            events[0].start,
            ecm_model::item::parse_utc("2026-09-01T08:00:00Z")
        );
        let contacts = client.contact_briefs("CON").unwrap();
        assert_eq!(contacts[0].email.as_deref(), Some("ann@example.test"));
        assert_eq!(contacts[0].display_name.as_deref(), Some("Ann"));
        assert!(!contacts[0].has_picture);
    }

    #[test]
    fn attaches_a_contact_photo() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:CreateAttachmentResponse><m:ResponseMessages><m:CreateAttachmentResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Attachments><t:FileAttachment><t:AttachmentId Id="A1"/></t:FileAttachment></m:Attachments></m:CreateAttachmentResponseMessage></m:ResponseMessages></m:CreateAttachmentResponse>"#,
            ),
        )]);
        EwsClient::new(&fake)
            .attach_contact_photo("C1", &[0xFF, 0xD8, 0xFF])
            .unwrap();
        assert!(fake.requests.borrow()[0].contains(r#"<m:ParentItemId Id="C1"/><m:Attachments><t:FileAttachment><t:Name>ContactPicture.jpg</t:Name><t:ContentType>image/jpeg</t:ContentType><t:IsContactPhoto>true</t:IsContactPhoto><t:Content>/9j/</t:Content>"#));
    }
}
