//! Calendar items, contacts and tasks: brief listings, to avoid creating twice
//! what a folder already holds, and whole items, to copy them elsewhere.
//! Read-only.

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

/// A category of the mailbox's master list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    /// Name.
    pub name: String,
    /// Outlook color index: -1 for none, else 0 to 24.
    pub color: i32,
}

impl<T: Transport> EwsClient<T> {
    /// The master category list: the `CategoryList` configuration of the
    /// calendar folder. Only reads.
    pub fn master_categories(&self) -> Result<Vec<Category>> {
        use base64::Engine as _;
        let message = self
            .call(r#"<m:GetUserConfiguration><m:UserConfigurationName Name="CategoryList"><t:DistinguishedFolderId Id="calendar"/></m:UserConfigurationName><m:UserConfigurationProperties>XmlData</m:UserConfigurationProperties></m:GetUserConfiguration>"#)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                Error::MalformedResponse("GetUserConfiguration returned no message".into())
            })?;
        if message.code == "ErrorItemNotFound" {
            return Ok(Vec::new());
        }
        let message = message.into_result()?;
        let data = message
            .element
            .path(&["UserConfiguration", "XmlData"])
            .map(|d| d.text.trim().to_owned())
            .unwrap_or_default();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let xml = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| Error::MalformedResponse(format!("category list: {e}")))?;
        Ok(crate::xml::parse(&xml)?
            .children_named("category")
            .filter_map(|category| {
                Some(Category {
                    name: category.attribute("name")?.to_owned(),
                    color: category
                        .attribute("color")
                        .and_then(|c| c.parse().ok())
                        .unwrap_or(-1),
                })
            })
            .collect())
    }

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

    /// Every item of a folder with all its properties (`AllProperties`, the best
    /// body): calendar items with their recurrence, wall clock times and time
    /// zone ids, contacts with their attachments, tasks.
    pub fn full_items(&self, folder_id: &str) -> Result<Vec<Element>> {
        let ids: Vec<String> = self.find_all(folder_id, "")?.iter().map(item_id).collect();
        let mut items = Vec::with_capacity(ids.len());
        for batch in ids.chunks(50) {
            let mut body = String::from(
                r#"<m:GetItem><m:ItemShape><t:BaseShape>AllProperties</t:BaseShape><t:BodyType>Best</t:BodyType><t:AdditionalProperties><t:FieldURI FieldURI="item:Attachments"/></t:AdditionalProperties></m:ItemShape><m:ItemIds>"#,
            );
            for id in batch {
                body.push_str(&format!(r#"<t:ItemId Id="{}"/>"#, escape(id)));
            }
            body.push_str("</m:ItemIds></m:GetItem>");
            for message in self.call(&body)? {
                let message = message.into_result()?;
                if let Some(item) = message
                    .element
                    .child("Items")
                    .and_then(|items| items.children.first())
                {
                    items.push(item.clone());
                }
            }
        }
        Ok(items)
    }

    /// The photo of a contact read by [`full_items`](Self::full_items), or `None`
    /// when it has none.
    pub fn contact_photo(&self, contact: &Element) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let Some(id) = contact
            .child("Attachments")
            .into_iter()
            .flat_map(|a| a.children_named("FileAttachment"))
            .find(|a| a.child_text("IsContactPhoto") == Some("true"))
            .and_then(|a| a.child("AttachmentId")?.attribute("Id"))
        else {
            return Ok(None);
        };
        let body = format!(
            r#"<m:GetAttachment><m:AttachmentIds><t:AttachmentId Id="{}"/></m:AttachmentIds></m:GetAttachment>"#,
            escape(id)
        );
        let message = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("GetAttachment returned no message".into()))?
            .into_result()?;
        let content = message
            .element
            .path(&["Attachments", "FileAttachment", "Content"])
            .map(|c| c.text.trim().to_owned())
            .unwrap_or_default();
        base64::engine::general_purpose::STANDARD
            .decode(content)
            .map(Some)
            .map_err(|e| Error::MalformedResponse(format!("contact photo: {e}")))
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
    fn reads_the_master_category_list() {
        use base64::Engine as _;
        let list = base64::engine::general_purpose::STANDARD.encode(
            r#"<?xml version="1.0"?><categories default="" xmlns="CategoryList.xsd"><category name="Famille" color="9" renameOnFirstUse="1"/><category name="Sans couleur" color="-1"/></categories>"#,
        );
        let fake = FakeTransport::new([(
            200,
            soap(&format!(
                r#"<m:GetUserConfigurationResponse><m:ResponseMessages><m:GetUserConfigurationResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:UserConfiguration><t:UserConfigurationName Name="CategoryList"/><t:XmlData>{list}</t:XmlData></m:UserConfiguration></m:GetUserConfigurationResponseMessage></m:ResponseMessages></m:GetUserConfigurationResponse>"#
            )),
        )]);
        let categories = EwsClient::new(&fake).master_categories().unwrap();
        assert_eq!(
            categories,
            [
                super::Category {
                    name: "Famille".into(),
                    color: 9
                },
                super::Category {
                    name: "Sans couleur".into(),
                    color: -1
                }
            ]
        );
    }

    #[test]
    fn reads_whole_items_and_a_contact_photo() {
        let fake = FakeTransport::new([
            (
                200,
                soap(
                    r#"<m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder IncludesLastItemInRange="true"><t:Items><t:Contact><t:ItemId Id="C1"/></t:Contact></t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"#,
                ),
            ),
            (
                200,
                soap(
                    r#"<m:GetItemResponse><m:ResponseMessages><m:GetItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:Contact><t:ItemId Id="C1"/><t:DisplayName>Ann</t:DisplayName><t:Attachments><t:FileAttachment><t:AttachmentId Id="A0"/><t:IsContactPhoto>false</t:IsContactPhoto></t:FileAttachment><t:FileAttachment><t:AttachmentId Id="A1"/><t:IsContactPhoto>true</t:IsContactPhoto></t:FileAttachment></t:Attachments></t:Contact></m:Items></m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"#,
                ),
            ),
            (
                200,
                soap(
                    r#"<m:GetAttachmentResponse><m:ResponseMessages><m:GetAttachmentResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Attachments><t:FileAttachment><t:AttachmentId Id="A1"/><t:Content>SlBFRw==</t:Content></t:FileAttachment></m:Attachments></m:GetAttachmentResponseMessage></m:ResponseMessages></m:GetAttachmentResponse>"#,
                ),
            ),
        ]);
        let client = EwsClient::new(&fake);
        let items = client.full_items("CON").unwrap();
        assert_eq!(items[0].child_text("DisplayName"), Some("Ann"));
        assert_eq!(
            client.contact_photo(&items[0]).unwrap(),
            Some(b"JPEG".to_vec())
        );
        let requests = fake.requests.borrow();
        assert!(requests[1].contains("<t:BaseShape>AllProperties</t:BaseShape>"));
        assert!(requests[2].contains(r#"<t:AttachmentId Id="A1"/>"#));
    }
}
