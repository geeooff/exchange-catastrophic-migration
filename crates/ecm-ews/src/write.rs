//! Every operation that changes a mailbox: creating folders, items, inbox rules
//! and contact photos, moving items, and moving a folder into Deleted Items.
//!
//! Nothing here deletes anything, and the only change to an existing item is a
//! contact's photo. These operations are used by `ecm fidelity-test`, inside a
//! dedicated test folder, and by `ecm apply`, which only runs reviewed plans.

use base64::Engine as _;

use crate::{Error, EwsClient, Result, Transport, xml::escape};

/// A property to set on an item at creation time, as an EWS extended property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedValue {
    /// Property tag, e.g. `0x0E06`.
    pub tag: u16,
    /// EWS property type, e.g. `SystemTime`, `Integer`, `String`.
    pub kind: &'static str,
    /// Value, as EWS writes it (ISO 8601 for times).
    pub value: String,
}

/// An inbox rule as the server keeps it, for checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleBrief {
    /// Display name.
    pub name: String,
    /// Order of evaluation.
    pub priority: Option<u32>,
    /// Whether it runs.
    pub enabled: bool,
    /// Whether the server reports it in error.
    pub in_error: bool,
    /// Whether the server cannot run it (client-only parts).
    pub not_supported: bool,
    /// Folder id of a move action.
    pub move_to: Option<String>,
    /// Folder id of a copy action.
    pub copy_to: Option<String>,
    /// Whether it stops the rules after it.
    pub stops: bool,
}

impl<T: Transport> EwsClient<T> {
    /// The mailbox's inbox rules, whole (`<t:Rule>` elements), in order. Only
    /// reads.
    pub fn inbox_rule_elements(&self) -> Result<Vec<crate::xml::Element>> {
        let message = self
            .call("<m:GetInboxRules/>")?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("GetInboxRules returned no message".into()))?
            .into_result()?;
        Ok(message
            .element
            .child("InboxRules")
            .map(|rules| rules.children.clone())
            .unwrap_or_default())
    }

    /// The mailbox's inbox rules with what matters to check them. Only reads.
    pub fn inbox_rules(&self) -> Result<Vec<RuleBrief>> {
        let flag = |rule: &crate::xml::Element, name: &str| {
            rule.child_text(name)
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        };
        let folder = |rule: &crate::xml::Element, action: &str| {
            rule.path(&["Actions", action, "FolderId"])
                .and_then(|id| id.attribute("Id"))
                .map(str::to_owned)
        };
        Ok(self
            .inbox_rule_elements()?
            .iter()
            .map(|rule| RuleBrief {
                name: rule
                    .child_text("DisplayName")
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                priority: rule
                    .child_text("Priority")
                    .and_then(|p| p.trim().parse().ok()),
                enabled: flag(rule, "IsEnabled"),
                in_error: flag(rule, "IsInError"),
                not_supported: flag(rule, "IsNotSupported"),
                move_to: folder(rule, "MoveToFolder"),
                copy_to: folder(rule, "CopyToFolder"),
                stops: rule
                    .path(&["Actions", "StopProcessingRules"])
                    .is_some_and(|v| v.text.trim().eq_ignore_ascii_case("true")),
            })
            .collect())
    }

    /// Returns the id of the mail folder `name` directly under the top of the
    /// mailbox, creating it if needed.
    pub fn ensure_top_folder(&self, name: &str) -> Result<String> {
        let tree = self.folder_tree("msgfolderroot")?;
        if let Some(existing) = tree
            .descendants
            .iter()
            .find(|f| f.name == name && f.parent_id.as_deref() == Some(tree.root.id.as_str()))
        {
            return Ok(existing.id.clone());
        }
        let body = format!(
            r#"<m:CreateFolder><m:ParentFolderId><t:DistinguishedFolderId Id="msgfolderroot"/></m:ParentFolderId><m:Folders><t:Folder><t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>{}</t:DisplayName></t:Folder></m:Folders></m:CreateFolder>"#,
            escape(name)
        );
        let message = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("CreateFolder returned no message".into()))?
            .into_result()?;
        message
            .element
            .child("Folders")
            .and_then(|folders| folders.children.first())
            .and_then(|folder| folder.child("FolderId"))
            .and_then(|id| id.attribute("Id"))
            .map(str::to_owned)
            .ok_or_else(|| Error::MalformedResponse("CreateFolder returned no folder id".into()))
    }

    /// Creates a mail folder `name` under the folder `parent_id` and returns its id.
    pub fn create_folder(&self, parent_id: &str, name: &str) -> Result<String> {
        self.create_folder_of_class(parent_id, name, "IPF.Note")
    }

    /// Creates a folder `name` of the class `class` (`IPF.Appointment` for a
    /// calendar, `IPF.Contact`, `IPF.Task`) under the folder `parent_id` and
    /// returns its id.
    pub fn create_folder_of_class(
        &self,
        parent_id: &str,
        name: &str,
        class: &str,
    ) -> Result<String> {
        // Typed folders are `CalendarFolder`, `ContactsFolder`, `TasksFolder`,
        // whose class Exchange sets itself: naming it is refused
        // (`ErrorNoFolderClassOverride`).
        let (element, class) = match class {
            "IPF.Appointment" => ("CalendarFolder", String::new()),
            "IPF.Contact" => ("ContactsFolder", String::new()),
            "IPF.Task" => ("TasksFolder", String::new()),
            other => (
                "Folder",
                format!("<t:FolderClass>{}</t:FolderClass>", escape(other)),
            ),
        };
        let body = format!(
            r#"<m:CreateFolder><m:ParentFolderId><t:FolderId Id="{}"/></m:ParentFolderId><m:Folders><t:{element}>{class}<t:DisplayName>{}</t:DisplayName></t:{element}></m:Folders></m:CreateFolder>"#,
            escape(parent_id),
            escape(name)
        );
        let message = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("CreateFolder returned no message".into()))?
            .into_result()?;
        message
            .element
            .child("Folders")
            .and_then(|folders| folders.children.first())
            .and_then(|folder| folder.child("FolderId"))
            .and_then(|id| id.attribute("Id"))
            .map(str::to_owned)
            .ok_or_else(|| Error::MalformedResponse("CreateFolder returned no folder id".into()))
    }

    /// Renames a folder. `class` is its class (`IPF.Contact`…): a calendar,
    /// contact or task folder is changed as the folder type it is.
    pub fn rename_folder(&self, folder_id: &str, class: Option<&str>, name: &str) -> Result<()> {
        let element = match class {
            Some("IPF.Appointment") => "CalendarFolder",
            Some("IPF.Contact") => "ContactsFolder",
            Some("IPF.Task") => "TasksFolder",
            _ => "Folder",
        };
        let body = format!(
            r#"<m:UpdateFolder><m:FolderChanges><t:FolderChange><t:FolderId Id="{}"/><t:Updates><t:SetFolderField><t:FieldURI FieldURI="folder:DisplayName"/><t:{element}><t:DisplayName>{}</t:DisplayName></t:{element}></t:SetFolderField></t:Updates></t:FolderChange></m:FolderChanges></m:UpdateFolder>"#,
            escape(folder_id),
            escape(name)
        );
        self.call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("UpdateFolder returned no message".into()))?
            .into_result()
            .map(|_| ())
    }

    /// Moves a folder, with everything in it, into the Deleted Items folder. The
    /// user empties it, or not: nothing is deleted here.
    pub fn move_folder_to_deleted_items(&self, folder_id: &str) -> Result<()> {
        let body = format!(
            r#"<m:MoveFolder><m:ToFolderId><t:DistinguishedFolderId Id="deleteditems"/></m:ToFolderId><m:FolderIds><t:FolderId Id="{}"/></m:FolderIds></m:MoveFolder>"#,
            escape(folder_id)
        );
        self.call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("MoveFolder returned no message".into()))?
            .into_result()
            .map(|_| ())
    }

    /// Sets a contact's photo, as the `ContactPicture.jpg` attachment Exchange
    /// and Outlook use for it.
    pub fn attach_contact_photo(&self, contact_id: &str, photo: &[u8]) -> Result<()> {
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

    /// Attaches a file to an item, for a message too large to create in one
    /// request with its files. A file with a `Content-ID` is inline: its HTML
    /// body shows it.
    pub fn attach_file(
        &self,
        item_id: &str,
        name: &str,
        content_type: &str,
        content_id: Option<&str>,
        data: &[u8],
    ) -> Result<()> {
        let inline = content_id.map_or(String::new(), |id| {
            format!(
                "<t:ContentId>{}</t:ContentId><t:IsInline>true</t:IsInline>",
                escape(id)
            )
        });
        let body = format!(
            r#"<m:CreateAttachment><m:ParentItemId Id="{}"/><m:Attachments><t:FileAttachment><t:Name>{}</t:Name><t:ContentType>{}</t:ContentType>{inline}<t:Content>{}</t:Content></t:FileAttachment></m:Attachments></m:CreateAttachment>"#,
            escape(item_id),
            escape(name),
            escape(content_type),
            base64::engine::general_purpose::STANDARD.encode(data)
        );
        // Right after a file, Exchange may still be storing it: the item's change
        // key conflict it answers then passes after a pause.
        let mut wait = std::time::Duration::from_secs(5);
        loop {
            let result = self
                .call(&body)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::MalformedResponse("CreateAttachment returned no message".into())
                })?
                .into_result()
                .map(|_| ());
            match result {
                Err(Error::Response { code, .. })
                    if code == "ErrorIrresolvableConflict"
                        && wait <= std::time::Duration::from_secs(40) =>
                {
                    (self.sleep)(wait);
                    wait *= 2;
                }
                other => return other,
            }
        }
    }

    /// Moves items to a folder. Nothing is deleted. Returns, per item, its new id
    /// (a moved item gets a new id) or the error.
    pub fn move_items(&self, ids: &[&str], folder_id: &str) -> Result<Vec<Result<String>>> {
        let mut body = format!(
            r#"<m:MoveItem><m:ToFolderId><t:FolderId Id="{}"/></m:ToFolderId><m:ItemIds>"#,
            escape(folder_id)
        );
        for id in ids {
            body.push_str(&format!(r#"<t:ItemId Id="{}"/>"#, escape(id)));
        }
        body.push_str("</m:ItemIds><m:ReturnNewItemIds>true</m:ReturnNewItemIds></m:MoveItem>");
        let messages = self.call(&body)?;
        if messages.len() != ids.len() {
            return Err(Error::MalformedResponse(format!(
                "MoveItem answered {} messages for {} items",
                messages.len(),
                ids.len()
            )));
        }
        Ok(messages
            .into_iter()
            .map(|message| {
                let message = message.into_result()?;
                Ok(message
                    .element
                    .child("Items")
                    .and_then(|items| items.children.first())
                    .and_then(|item| item.child("ItemId"))
                    .and_then(|id| id.attribute("Id"))
                    .unwrap_or_default()
                    .to_owned())
            })
            .collect())
    }

    /// Restores exported items (`ExportItems` data) as new items in a folder, with
    /// `UploadItems`. Returns the new item ids, one result per item.
    pub fn upload_items(&self, folder_id: &str, items: &[Vec<u8>]) -> Result<Vec<Result<String>>> {
        let mut body = String::from("<m:UploadItems><m:Items>");
        for data in items {
            body.push_str(&format!(
                r#"<t:Item CreateAction="CreateNew"><t:ParentFolderId Id="{}"/><t:Data>{}</t:Data></t:Item>"#,
                escape(folder_id),
                base64::engine::general_purpose::STANDARD.encode(data)
            ));
        }
        body.push_str("</m:Items></m:UploadItems>");
        let messages = self.call(&body)?;
        if messages.len() != items.len() {
            return Err(Error::MalformedResponse(format!(
                "UploadItems answered {} messages for {} items",
                messages.len(),
                items.len()
            )));
        }
        Ok(messages
            .into_iter()
            .map(|message| {
                let message = message.into_result()?;
                message
                    .element
                    .child("ItemId")
                    .and_then(|id| id.attribute("Id"))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        Error::MalformedResponse("UploadItems returned no item id".into())
                    })
            })
            .collect())
    }

    /// Creates a message from its MIME content in a folder, setting `extended`
    /// properties in the same request (some, such as `PR_MESSAGE_FLAGS`, can only
    /// be set at creation). Returns the new item id.
    ///
    /// Exchange refuses MIME it would turn into another type, such as a meeting
    /// request (`ErrorObjectTypeChanged`); a generic `t:Item` does not help
    /// (`ErrorUnsupportedMimeConversion`).
    pub fn create_from_mime(
        &self,
        folder_id: &str,
        mime: &[u8],
        extended: &[ExtendedValue],
    ) -> Result<String> {
        self.create_from_mime_as("Message", folder_id, mime, extended)
    }

    /// Creates an item from its EWS XML (`<t:CalendarItem>`, `<t:Contact>`,
    /// `<t:Task>`…) in a folder, without sending any meeting invitation. Returns
    /// the new item id.
    pub fn create_item(&self, folder_id: &str, item_xml: &str) -> Result<String> {
        let body = format!(
            r#"<m:CreateItem SendMeetingInvitations="SendToNone"><m:SavedItemFolderId><t:FolderId Id="{}"/></m:SavedItemFolderId><m:Items>{item_xml}</m:Items></m:CreateItem>"#,
            escape(folder_id)
        );
        let message = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("CreateItem returned no message".into()))?
            .into_result()?;
        message
            .element
            .child("Items")
            .and_then(|items| items.children.first())
            .and_then(|item| item.child("ItemId"))
            .and_then(|id| id.attribute("Id"))
            .map(str::to_owned)
            .ok_or_else(|| Error::MalformedResponse("CreateItem returned no item id".into()))
    }

    /// Display names of the mailbox's inbox rules (`GetInboxRules`). Only reads.
    pub fn inbox_rule_names(&self) -> Result<Vec<String>> {
        Ok(self
            .inbox_rule_elements()?
            .iter()
            .filter_map(|rule| rule.child_text("DisplayName"))
            .map(|name| name.trim().to_owned())
            .collect())
    }

    /// Creates an inbox rule from its EWS XML (`<t:Rule>`), with
    /// `UpdateInboxRules`.
    pub fn create_inbox_rule(&self, rule_xml: &str) -> Result<()> {
        let body = format!(
            "<m:UpdateInboxRules><m:RemoveOutlookRuleBlob>false</m:RemoveOutlookRuleBlob><m:Operations><t:CreateRuleOperation>{rule_xml}</t:CreateRuleOperation></m:Operations></m:UpdateInboxRules>"
        );
        self.call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("UpdateInboxRules returned no message".into()))?
            .into_result()
            .map(|_| ())
    }

    fn create_from_mime_as(
        &self,
        element: &str,
        folder_id: &str,
        mime: &[u8],
        extended: &[ExtendedValue],
    ) -> Result<String> {
        let mut properties = String::new();
        for property in extended {
            properties.push_str(&format!(
                r#"<t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="0x{:04X}" PropertyType="{}"/><t:Value>{}</t:Value></t:ExtendedProperty>"#,
                property.tag,
                property.kind,
                escape(&property.value)
            ));
        }
        let body = format!(
            r#"<m:CreateItem MessageDisposition="SaveOnly"><m:SavedItemFolderId><t:FolderId Id="{}"/></m:SavedItemFolderId><m:Items><t:{element}><t:MimeContent CharacterSet="UTF-8">{}</t:MimeContent>{properties}</t:{element}></m:Items></m:CreateItem>"#,
            escape(folder_id),
            base64::engine::general_purpose::STANDARD.encode(mime)
        );
        let message = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("CreateItem returned no message".into()))?
            .into_result()?;
        message
            .element
            .child("Items")
            .and_then(|items| items.children.first())
            .and_then(|item| item.child("ItemId"))
            .and_then(|id| id.attribute("Id"))
            .map(str::to_owned)
            .ok_or_else(|| Error::MalformedResponse("CreateItem returned no item id".into()))
    }
}

/// What a modified occurrence of a series has of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OccurrenceChange<'a> {
    /// Its start, UTC (`2024-12-30T03:19:13Z`).
    pub start: &'a str,
    /// Its end, UTC.
    pub end: &'a str,
    /// Its subject, when not the series'.
    pub subject: Option<&'a str>,
    /// Its location, when not the series'.
    pub location: Option<&'a str>,
}

/// Half the width of the window an occurrence is looked for in: series recur
/// by the day at the least, so the window holds one unchanged occurrence.
const OCCURRENCE_WINDOW: i64 = 12 * 3600;

impl<T: Transport> EwsClient<T> {
    /// The id of the occurrence of a series that its pattern places at
    /// `original_start` (UTC): the occurrence of the series' `UID` in the
    /// calendar view of `folder_id` around that time, one not yet changed
    /// rather than one moved there. Only reads.
    pub fn occurrence_of(
        &self,
        folder_id: &str,
        series_id: &str,
        original_start: &str,
    ) -> Result<String> {
        let at = ecm_model::item::parse_utc(original_start).ok_or_else(|| {
            Error::MalformedResponse(format!("{original_start:?} is not a UTC time"))
        })?;
        let body = format!(
            r#"<m:GetItem><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="calendar:UID"/></t:AdditionalProperties></m:ItemShape><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:GetItem>"#,
            escape(series_id)
        );
        let series = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("GetItem returned no message".into()))?
            .into_result()?;
        let uid = series
            .element
            .child("Items")
            .and_then(|items| items.children.first())
            .and_then(|item| item.child_text("UID"))
            .map(str::to_owned)
            .ok_or_else(|| Error::MalformedResponse("the series has no UID".into()))?;

        let body = format!(
            r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="calendar:Start"/><t:FieldURI FieldURI="calendar:UID"/><t:FieldURI FieldURI="calendar:CalendarItemType"/></t:AdditionalProperties></m:ItemShape><m:CalendarView StartDate="{}" EndDate="{}"/><m:ParentFolderIds><t:FolderId Id="{}"/></m:ParentFolderIds></m:FindItem>"#,
            ecm_model::item::format_iso(at - OCCURRENCE_WINDOW + 1),
            ecm_model::item::format_iso(at + OCCURRENCE_WINDOW - 1),
            escape(folder_id)
        );
        let view = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("FindItem returned no message".into()))?
            .into_result()?;
        let mut found = Vec::new();
        view.element.descendants_named("CalendarItem", &mut found);
        // (starts at the original time, not yet changed, id)
        let mut candidates: Vec<(bool, bool, &str)> = found
            .into_iter()
            .filter(|item| item.child_text("UID") == Some(uid.as_str()))
            .filter_map(|item| {
                let id = item.child("ItemId")?.attribute("Id")?;
                let start = item
                    .child_text("Start")
                    .and_then(ecm_model::item::parse_utc)?;
                let unchanged = item.child_text("CalendarItemType") == Some("Occurrence");
                Some((start == at, unchanged, id))
            })
            .collect();
        // The one at the original time first, an unchanged one before a moved
        // one.
        candidates.sort_by_key(|(on_time, unchanged, _)| (!on_time, !unchanged));
        match candidates.as_slice() {
            [(true, _, id), ..] | [(false, true, id)] => Ok((*id).to_owned()),
            _ => Err(Error::MalformedResponse(format!(
                "no occurrence of the series at {original_start}"
            ))),
        }
    }

    /// Removes one occurrence of a series, by its own id, into the Deleted
    /// Items folder: the series keeps it as a deleted occurrence. No
    /// cancellation is sent.
    pub fn remove_occurrence(&self, occurrence_id: &str) -> Result<()> {
        let body = format!(
            r#"<m:DeleteItem DeleteType="MoveToDeletedItems" SendMeetingCancellations="SendToNone"><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:DeleteItem>"#,
            escape(occurrence_id)
        );
        self.call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("DeleteItem returned no message".into()))?
            .into_result()
            .map(|_| ())
    }

    /// Gives one occurrence of a series, by its own id, a start, an end and
    /// maybe a subject and a location of its own. No invitation is sent.
    pub fn change_occurrence(&self, occurrence_id: &str, change: &OccurrenceChange) -> Result<()> {
        let set = |uri: &str, name: &str, value: &str| {
            format!(
                r#"<t:SetItemField><t:FieldURI FieldURI="{uri}"/><t:CalendarItem><t:{name}>{}</t:{name}></t:CalendarItem></t:SetItemField>"#,
                escape(value)
            )
        };
        let mut updates = String::new();
        if let Some(subject) = change.subject {
            updates += &set("item:Subject", "Subject", subject);
        }
        updates += &set("calendar:Start", "Start", change.start);
        updates += &set("calendar:End", "End", change.end);
        if let Some(location) = change.location {
            updates += &set("calendar:Location", "Location", location);
        }
        let body = format!(
            r#"<m:UpdateItem ConflictResolution="AlwaysOverwrite" SendMeetingInvitationsOrCancellations="SendToNone"><m:ItemChanges><t:ItemChange><t:ItemId Id="{}"/><t:Updates>{updates}</t:Updates></t:ItemChange></m:ItemChanges></m:UpdateItem>"#,
            escape(occurrence_id)
        );
        self.call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("UpdateItem returned no message".into()))?
            .into_result()
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtendedValue, OccurrenceChange};
    use crate::{
        EwsClient,
        testing::{FakeTransport, soap},
    };

    #[test]
    fn creates_a_message_from_mime_with_creation_time_properties() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:CreateItemResponse><m:ResponseMessages><m:CreateItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:Message><t:ItemId Id="NEW" ChangeKey="1"/></t:Message></m:Items></m:CreateItemResponseMessage></m:ResponseMessages></m:CreateItemResponse>"#,
            ),
        )]);
        let id = EwsClient::new(&fake)
            .create_from_mime(
                "F",
                b"Subject: x\r\n\r\nbody",
                &[ExtendedValue {
                    tag: 0x0E07,
                    kind: "Integer",
                    value: "1".into(),
                }],
            )
            .unwrap();
        assert_eq!(id, "NEW");
        let request = &fake.requests.borrow()[0];
        assert!(request.contains(r#"<m:CreateItem MessageDisposition="SaveOnly"><m:SavedItemFolderId><t:FolderId Id="F"/>"#));
        assert!(request.contains(
            "<t:MimeContent CharacterSet=\"UTF-8\">U3ViamVjdDogeA0KDQpib2R5</t:MimeContent>"
        ));
        assert!(request.contains(r#"<t:ExtendedFieldURI PropertyTag="0x0E07" PropertyType="Integer"/><t:Value>1</t:Value>"#));
    }

    #[test]
    fn creates_typed_folders_without_naming_their_class() {
        let created = |kind: &str| {
            (
                200,
                soap(&format!(
                    r#"<m:CreateFolderResponse><m:ResponseMessages><m:CreateFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Folders><t:{kind}><t:FolderId Id="F1" ChangeKey="1"/></t:{kind}></m:Folders></m:CreateFolderResponseMessage></m:ResponseMessages></m:CreateFolderResponse>"#
                )),
            )
        };
        let fake = FakeTransport::new([created("CalendarFolder"), created("Folder")]);
        let client = EwsClient::new(&fake);
        assert_eq!(
            client
                .create_folder_of_class("CAL", "ecm-test", "IPF.Appointment")
                .unwrap(),
            "F1"
        );
        assert_eq!(client.create_folder("TOP", "Archive").unwrap(), "F1");
        let requests = fake.requests.borrow();
        assert!(requests[0].contains(
            "<m:Folders><t:CalendarFolder><t:DisplayName>ecm-test</t:DisplayName></t:CalendarFolder></m:Folders>"
        ));
        assert!(requests[1].contains(
            "<t:Folder><t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Archive</t:DisplayName></t:Folder>"
        ));
    }

    #[test]
    fn creates_typed_items_without_invitations() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:CreateItemResponse><m:ResponseMessages><m:CreateItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:CalendarItem><t:ItemId Id="C1" ChangeKey="1"/></t:CalendarItem></m:Items></m:CreateItemResponseMessage></m:ResponseMessages></m:CreateItemResponse>"#,
            ),
        )]);
        let id = EwsClient::new(&fake)
            .create_item(
                "CAL",
                "<t:CalendarItem><t:Subject>x</t:Subject></t:CalendarItem>",
            )
            .unwrap();
        assert_eq!(id, "C1");
        assert!(fake.requests.borrow()[0].contains(r#"<m:CreateItem SendMeetingInvitations="SendToNone"><m:SavedItemFolderId><t:FolderId Id="CAL"/></m:SavedItemFolderId><m:Items><t:CalendarItem>"#));
    }

    #[test]
    fn reads_rule_names_and_creates_a_rule() {
        let fake = FakeTransport::new([
            (
                200,
                soap(
                    r#"<m:GetInboxRulesResponse ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:OutlookRuleBlobExists>false</m:OutlookRuleBlobExists><m:InboxRules><t:Rule><t:RuleId>R1</t:RuleId><t:DisplayName>GitHub</t:DisplayName></t:Rule></m:InboxRules></m:GetInboxRulesResponse>"#,
                ),
            ),
            (
                200,
                soap(
                    r#"<m:UpdateInboxRulesResponse ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode></m:UpdateInboxRulesResponse>"#,
                ),
            ),
        ]);
        let client = EwsClient::new(&fake);
        assert_eq!(client.inbox_rule_names().unwrap(), ["GitHub"]);
        client
            .create_inbox_rule("<t:Rule><t:DisplayName>x</t:DisplayName></t:Rule>")
            .unwrap();
        assert!(fake.requests.borrow()[1].contains("<t:CreateRuleOperation><t:Rule><t:DisplayName>x</t:DisplayName></t:Rule></t:CreateRuleOperation>"));
    }

    #[test]
    fn renames_a_typed_folder_as_its_type() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:UpdateFolderResponse><m:ResponseMessages><m:UpdateFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Folders><t:ContactsFolder><t:FolderId Id="F1"/></t:ContactsFolder></m:Folders></m:UpdateFolderResponseMessage></m:ResponseMessages></m:UpdateFolderResponse>"#,
            ),
        )]);
        EwsClient::new(&fake)
            .rename_folder("F1", Some("IPF.Contact"), "ecm-test (Contacts)")
            .unwrap();
        assert!(fake.requests.borrow()[0].contains(r#"<t:FolderId Id="F1"/><t:Updates><t:SetFolderField><t:FieldURI FieldURI="folder:DisplayName"/><t:ContactsFolder><t:DisplayName>ecm-test (Contacts)</t:DisplayName></t:ContactsFolder></t:SetFolderField></t:Updates>"#));
    }

    #[test]
    fn moves_a_folder_to_deleted_items() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:MoveFolderResponse><m:ResponseMessages><m:MoveFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Folders><t:Folder><t:FolderId Id="Q2"/></t:Folder></m:Folders></m:MoveFolderResponseMessage></m:ResponseMessages></m:MoveFolderResponse>"#,
            ),
        )]);
        EwsClient::new(&fake)
            .move_folder_to_deleted_items("Q1")
            .unwrap();
        assert!(fake.requests.borrow()[0].contains(r#"<m:MoveFolder><m:ToFolderId><t:DistinguishedFolderId Id="deleteditems"/></m:ToFolderId><m:FolderIds><t:FolderId Id="Q1"/></m:FolderIds></m:MoveFolder>"#));
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

    #[test]
    fn attaches_files_to_a_message() {
        let answer = r#"<m:CreateAttachmentResponse><m:ResponseMessages><m:CreateAttachmentResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Attachments><t:FileAttachment><t:AttachmentId Id="A1"/></t:FileAttachment></m:Attachments></m:CreateAttachmentResponseMessage></m:ResponseMessages></m:CreateAttachmentResponse>"#;
        let fake = FakeTransport::new([(200, soap(answer)), (200, soap(answer))]);
        let client = EwsClient::new(&fake);
        client
            .attach_file("M1", "a&b.mp4", "video/mp4", None, b"abc")
            .unwrap();
        client
            .attach_file("M1", "logo.png", "image/png", Some("logo@x"), b"abc")
            .unwrap();
        let requests = fake.requests.borrow();
        assert!(requests[0].contains(r#"<m:ParentItemId Id="M1"/><m:Attachments><t:FileAttachment><t:Name>a&amp;b.mp4</t:Name><t:ContentType>video/mp4</t:ContentType><t:Content>YWJj</t:Content>"#));
        assert!(requests[1].contains(r#"<t:ContentType>image/png</t:ContentType><t:ContentId>logo@x</t:ContentId><t:IsInline>true</t:IsInline><t:Content>"#));
    }

    #[test]
    fn retries_a_file_while_the_previous_one_is_being_stored() {
        let conflict = r#"<m:CreateAttachmentResponse><m:ResponseMessages><m:CreateAttachmentResponseMessage ResponseClass="Error"><m:MessageText>The send or update operation could not be performed because the change key passed in the request does not match the current change key for the item.</m:MessageText><m:ResponseCode>ErrorIrresolvableConflict</m:ResponseCode></m:CreateAttachmentResponseMessage></m:ResponseMessages></m:CreateAttachmentResponse>"#;
        let done = r#"<m:CreateAttachmentResponse><m:ResponseMessages><m:CreateAttachmentResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Attachments><t:FileAttachment><t:AttachmentId Id="A2"/></t:FileAttachment></m:Attachments></m:CreateAttachmentResponseMessage></m:ResponseMessages></m:CreateAttachmentResponse>"#;
        let fake = FakeTransport::new([(200, soap(conflict)), (200, soap(done))]);
        EwsClient::new(&fake)
            .with_sleep(|_| {})
            .attach_file("M1", "b.mp4", "video/mp4", None, b"abc")
            .unwrap();
        assert_eq!(fake.requests.borrow().len(), 2);

        let conflicts = std::iter::repeat_n((200, soap(conflict)), 5);
        let fake = FakeTransport::new(conflicts);
        let error = EwsClient::new(&fake)
            .with_sleep(|_| {})
            .attach_file("M1", "b.mp4", "video/mp4", None, b"abc")
            .unwrap_err();
        assert!(error.to_string().contains("ErrorIrresolvableConflict"));
        assert_eq!(fake.requests.borrow().len(), 5);
    }

    #[test]
    fn moves_items_and_reports_each_result() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:MoveItemResponse><m:ResponseMessages><m:MoveItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:Message><t:ItemId Id="NEW1" ChangeKey="2"/></t:Message></m:Items></m:MoveItemResponseMessage><m:MoveItemResponseMessage ResponseClass="Error"><m:MessageText>The specified object was not found in the store.</m:MessageText><m:ResponseCode>ErrorItemNotFound</m:ResponseCode></m:MoveItemResponseMessage></m:ResponseMessages></m:MoveItemResponse>"#,
            ),
        )]);
        let results = EwsClient::new(&fake).move_items(&["A", "B"], "Q").unwrap();
        assert_eq!(results[0].as_deref().unwrap(), "NEW1");
        assert!(results[1].is_err());
        let request = &fake.requests.borrow()[0];
        assert!(request.contains(r#"<m:MoveItem><m:ToFolderId><t:FolderId Id="Q"/></m:ToFolderId><m:ItemIds><t:ItemId Id="A"/><t:ItemId Id="B"/></m:ItemIds>"#));
    }

    #[test]
    fn uploads_exported_items_and_reports_each_result() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:UploadItemsResponse><m:ResponseMessages><m:UploadItemsResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:ItemId Id="U1" ChangeKey="1"/></m:UploadItemsResponseMessage><m:UploadItemsResponseMessage ResponseClass="Error"><m:MessageText>Corrupt data.</m:MessageText><m:ResponseCode>ErrorInvalidRequest</m:ResponseCode></m:UploadItemsResponseMessage></m:ResponseMessages></m:UploadItemsResponse>"#,
            ),
        )]);
        let results = EwsClient::new(&fake)
            .upload_items("F", &[vec![1, 2], vec![3]])
            .unwrap();
        assert_eq!(results[0].as_deref().unwrap(), "U1");
        assert!(results[1].is_err());
        assert!(fake.requests.borrow()[0].contains(r#"<t:Item CreateAction="CreateNew"><t:ParentFolderId Id="F"/><t:Data>AQI=</t:Data></t:Item>"#));
    }

    /// The answers to the two requests `occurrence_of` makes: the series' UID,
    /// then the calendar view.
    fn series_and_view(view: &str) -> [(u16, String); 2] {
        [
            (
                200,
                soap(
                    r#"<m:GetItemResponse><m:ResponseMessages><m:GetItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:CalendarItem><t:ItemId Id="S1"/><t:UID>uid-1</t:UID></t:CalendarItem></m:Items></m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"#,
                ),
            ),
            (
                200,
                soap(&format!(
                    r#"<m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder TotalItemsInView="3" IncludesLastItemInRange="true"><t:Items>{view}</t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"#
                )),
            ),
        ]
    }

    fn in_view(id: &str, start: &str, uid: &str, kind: &str) -> String {
        format!(
            r#"<t:CalendarItem><t:ItemId Id="{id}"/><t:Start>{start}</t:Start><t:CalendarItemType>{kind}</t:CalendarItemType><t:UID>{uid}</t:UID></t:CalendarItem>"#
        )
    }

    #[test]
    fn finds_the_occurrence_of_a_series_at_its_original_time() {
        // Another series at the same time, an occurrence of this one moved
        // nearby, and the one looked for.
        let view = [
            in_view("X", "2024-05-06T08:00:00Z", "uid-2", "Occurrence"),
            in_view("M", "2024-05-06T10:00:00Z", "uid-1", "Exception"),
            in_view("O", "2024-05-06T08:00:00Z", "uid-1", "Occurrence"),
        ]
        .concat();
        let fake = FakeTransport::new(series_and_view(&view));
        let id = EwsClient::new(&fake)
            .occurrence_of("CAL", "S1", "2024-05-06T08:00:00Z")
            .unwrap();
        assert_eq!(id, "O");
        let requests = fake.requests.borrow();
        assert!(requests[0].contains(r#"<t:FieldURI FieldURI="calendar:UID"/>"#));
        assert!(requests[0].contains(r#"<t:ItemId Id="S1"/>"#));
        assert!(requests[1].contains(
            r#"<m:CalendarView StartDate="2024-05-05T20:00:01Z" EndDate="2024-05-06T19:59:59Z"/><m:ParentFolderIds><t:FolderId Id="CAL"/>"#
        ));
    }

    #[test]
    fn takes_the_only_unchanged_occurrence_near_and_refuses_a_guess() {
        // An all-day series whose day starts an hour off: the only unchanged
        // occurrence in the window is the one.
        let near = in_view("O", "2024-05-05T23:00:00Z", "uid-1", "Occurrence");
        let fake = FakeTransport::new(series_and_view(&near));
        let id = EwsClient::new(&fake)
            .occurrence_of("CAL", "S1", "2024-05-06T00:00:00Z")
            .unwrap();
        assert_eq!(id, "O");

        // Nothing of the series, or only an occurrence already moved: refused.
        for view in [
            in_view("X", "2024-05-06T00:00:00Z", "uid-2", "Occurrence"),
            in_view("M", "2024-05-06T03:00:00Z", "uid-1", "Exception"),
        ] {
            let fake = FakeTransport::new(series_and_view(&view));
            assert!(
                EwsClient::new(&fake)
                    .occurrence_of("CAL", "S1", "2024-05-06T00:00:00Z")
                    .is_err()
            );
        }
    }

    #[test]
    fn removes_and_changes_an_occurrence_without_telling_anyone() {
        let fake = FakeTransport::new([
            (
                200,
                soap(
                    r#"<m:DeleteItemResponse><m:ResponseMessages><m:DeleteItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode></m:DeleteItemResponseMessage></m:ResponseMessages></m:DeleteItemResponse>"#,
                ),
            ),
            (
                200,
                soap(
                    r#"<m:UpdateItemResponse><m:ResponseMessages><m:UpdateItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:CalendarItem><t:ItemId Id="O2"/></t:CalendarItem></m:Items></m:UpdateItemResponseMessage></m:ResponseMessages></m:UpdateItemResponse>"#,
                ),
            ),
        ]);
        let client = EwsClient::new(&fake);
        client.remove_occurrence("O1").unwrap();
        client
            .change_occurrence(
                "O2",
                &OccurrenceChange {
                    start: "2024-05-13T10:00:00Z",
                    end: "2024-05-13T11:00:00Z",
                    subject: Some("Yoga & co"),
                    location: None,
                },
            )
            .unwrap();
        let requests = fake.requests.borrow();
        assert!(requests[0].contains(r#"<m:DeleteItem DeleteType="MoveToDeletedItems" SendMeetingCancellations="SendToNone"><m:ItemIds><t:ItemId Id="O1"/></m:ItemIds></m:DeleteItem>"#));
        assert!(requests[1].contains(r#"<m:UpdateItem ConflictResolution="AlwaysOverwrite" SendMeetingInvitationsOrCancellations="SendToNone"><m:ItemChanges><t:ItemChange><t:ItemId Id="O2"/><t:Updates><t:SetItemField><t:FieldURI FieldURI="item:Subject"/><t:CalendarItem><t:Subject>Yoga &amp; co</t:Subject></t:CalendarItem></t:SetItemField><t:SetItemField><t:FieldURI FieldURI="calendar:Start"/><t:CalendarItem><t:Start>2024-05-13T10:00:00Z</t:Start></t:CalendarItem></t:SetItemField><t:SetItemField><t:FieldURI FieldURI="calendar:End"/><t:CalendarItem><t:End>2024-05-13T11:00:00Z</t:End></t:CalendarItem></t:SetItemField></t:Updates>"#));
    }
}
