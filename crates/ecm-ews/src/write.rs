//! Operations that change a mailbox: creating folders, creating items and moving
//! items.
//!
//! Nothing here updates or deletes an existing item. These operations are used by
//! `ecm fidelity-test`, inside a dedicated test folder, and by `ecm apply`, which
//! only runs reviewed plans.

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
    /// The mailbox's inbox rules with what matters to check them. Only reads.
    pub fn inbox_rules(&self) -> Result<Vec<RuleBrief>> {
        let message = self
            .call("<m:GetInboxRules/>")?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("GetInboxRules returned no message".into()))?
            .into_result()?;
        let flag = |rule: &crate::xml::Element, name: &str| {
            rule.child_text(name)
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        };
        let folder = |rule: &crate::xml::Element, action: &str| {
            rule.path(&["Actions", action, "FolderId"])
                .and_then(|id| id.attribute("Id"))
                .map(str::to_owned)
        };
        Ok(message
            .element
            .child("InboxRules")
            .map(|rules| {
                rules
                    .children
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
                    .collect()
            })
            .unwrap_or_default())
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
        let body = format!(
            r#"<m:CreateFolder><m:ParentFolderId><t:FolderId Id="{}"/></m:ParentFolderId><m:Folders><t:Folder><t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>{}</t:DisplayName></t:Folder></m:Folders></m:CreateFolder>"#,
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
        let message = self
            .call("<m:GetInboxRules/>")?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("GetInboxRules returned no message".into()))?
            .into_result()?;
        Ok(message
            .element
            .child("InboxRules")
            .map(|rules| {
                rules
                    .children
                    .iter()
                    .filter_map(|rule| rule.child_text("DisplayName"))
                    .map(|name| name.trim().to_owned())
                    .collect()
            })
            .unwrap_or_default())
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

#[cfg(test)]
mod tests {
    use super::ExtendedValue;
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
}
