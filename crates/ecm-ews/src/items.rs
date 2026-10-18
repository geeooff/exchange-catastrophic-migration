//! Items: listing a folder with `FindItem`, exporting with `ExportItems`.

use base64::Engine as _;

use ecm_model::{ItemRecord, item::parse_utc};

use crate::{
    Error, EwsClient, Result, Transport,
    xml::{Element, escape},
};

/// Items returned per `FindItem` page.
const PAGE_SIZE: usize = 1000;

/// An item as listed by `FindItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRef {
    /// EWS item id.
    pub id: String,
    /// Item class, e.g. `IPM.Note`.
    pub class: String,
    /// Size in bytes.
    pub size: u64,
}

/// The outcome of exporting one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Export {
    /// The item, in Exchange's full-fidelity export format: opaque bytes that
    /// `UploadItems` turns back into the same item.
    Data(Vec<u8>),
    /// The server could not export this item.
    Failed {
        /// EWS response code.
        code: String,
        /// Error message.
        message: String,
    },
}

impl<T: Transport> EwsClient<T> {
    /// Every normal item of a folder, oldest first, page by page. Associated items
    /// (views, rules) are not listed.
    pub fn list_items(&self, folder_id: &str) -> Result<Vec<ItemRef>> {
        let mut items = Vec::new();
        loop {
            let body = format!(
                r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="item:ItemClass"/><t:FieldURI FieldURI="item:Size"/></t:AdditionalProperties></m:ItemShape><m:IndexedPageItemView MaxEntriesReturned="{PAGE_SIZE}" Offset="{}" BasePoint="Beginning"/><m:SortOrder><t:FieldOrder Order="Ascending"><t:FieldURI FieldURI="item:DateTimeCreated"/></t:FieldOrder></m:SortOrder><m:ParentFolderIds><t:FolderId Id="{}"/></m:ParentFolderIds></m:FindItem>"#,
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
            let mut page = Vec::new();
            for item in root
                .child("Items")
                .map(|items| items.children.as_slice())
                .unwrap_or_default()
            {
                let id = item
                    .child("ItemId")
                    .and_then(|id| id.attribute("Id"))
                    .ok_or_else(|| {
                        Error::MalformedResponse(format!("{} without ItemId", item.name))
                    })?;
                page.push(ItemRef {
                    id: id.to_owned(),
                    class: item.child_text("ItemClass").unwrap_or_default().to_owned(),
                    size: item
                        .child_text("Size")
                        .and_then(|size| size.trim().parse().ok())
                        .unwrap_or(0),
                });
            }
            let last = root.attribute("IncludesLastItemInRange") != Some("false");
            let empty = page.is_empty();
            items.extend(page);
            if last || empty {
                return Ok(items);
            }
        }
    }

    /// Every normal item of a folder with the properties an audit needs, oldest
    /// first, page by page. `folder` is the folder's path, copied into each record.
    pub fn inventory_items(&self, folder_id: &str, folder: &str) -> Result<Vec<ItemRecord>> {
        let mut items = Vec::new();
        loop {
            let body = format!(
                r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties>{INVENTORY_PROPERTIES}</t:AdditionalProperties></m:ItemShape><m:IndexedPageItemView MaxEntriesReturned="{PAGE_SIZE}" Offset="{}" BasePoint="Beginning"/><m:SortOrder><t:FieldOrder Order="Ascending"><t:FieldURI FieldURI="item:DateTimeCreated"/></t:FieldOrder></m:SortOrder><m:ParentFolderIds><t:FolderId Id="{}"/></m:ParentFolderIds></m:FindItem>"#,
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
            let page = root
                .child("Items")
                .map(|list| list.children.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|item| inventory_record(item, folder))
                .collect::<Result<Vec<_>>>()?;
            let last = root.attribute("IncludesLastItemInRange") != Some("false");
            let empty = page.is_empty();
            items.extend(page);
            if last || empty {
                return Ok(items);
            }
        }
    }

    /// Exports items with `ExportItems`, one result per id, in the same order.
    /// Exporting does not change the items, not even their read state.
    pub fn export_items(&self, ids: &[&str]) -> Result<Vec<Export>> {
        let mut body = String::from("<m:ExportItems><m:ItemIds>");
        for id in ids {
            body.push_str(&format!(r#"<t:ItemId Id="{}"/>"#, escape(id)));
        }
        body.push_str("</m:ItemIds></m:ExportItems>");
        let messages = self.call(&body)?;
        if messages.len() != ids.len() {
            return Err(Error::MalformedResponse(format!(
                "ExportItems answered {} messages for {} items",
                messages.len(),
                ids.len()
            )));
        }
        messages
            .into_iter()
            .map(|message| {
                if !message.is_success() {
                    return Ok(Export::Failed {
                        code: message.code,
                        message: message.message,
                    });
                }
                let data = message.element.child_text("Data").ok_or_else(|| {
                    Error::MalformedResponse("ExportItems message without Data".into())
                })?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data.trim())
                    .map_err(|error| {
                        Error::MalformedResponse(format!("invalid base64 in Data: {error}"))
                    })?;
                Ok(Export::Data(bytes))
            })
            .collect()
    }
}

/// Properties `inventory_items` asks for. `PR_TRANSPORT_MESSAGE_HEADERS` comes back
/// truncated by `FindItem`, which is enough to tell whether it is there.
const INVENTORY_PROPERTIES: &str = concat!(
    r#"<t:FieldURI FieldURI="item:ItemClass"/>"#,
    r#"<t:FieldURI FieldURI="item:Subject"/>"#,
    r#"<t:FieldURI FieldURI="item:DateTimeReceived"/>"#,
    r#"<t:FieldURI FieldURI="item:DateTimeSent"/>"#,
    r#"<t:FieldURI FieldURI="item:DateTimeCreated"/>"#,
    r#"<t:FieldURI FieldURI="item:Size"/>"#,
    r#"<t:FieldURI FieldURI="item:HasAttachments"/>"#,
    r#"<t:FieldURI FieldURI="item:Preview"/>"#,
    r#"<t:FieldURI FieldURI="message:InternetMessageId"/>"#,
    r#"<t:FieldURI FieldURI="message:From"/>"#,
    r#"<t:FieldURI FieldURI="message:IsRead"/>"#,
    r#"<t:ExtendedFieldURI PropertyTag="0x007D" PropertyType="String"/>"#,
    r#"<t:ExtendedFieldURI PropertyTag="0x1016" PropertyType="Integer"/>"#,
);

fn inventory_record(item: &Element, folder: &str) -> Result<ItemRecord> {
    let key = item
        .child("ItemId")
        .and_then(|id| id.attribute("Id"))
        .ok_or_else(|| Error::MalformedResponse(format!("{} without ItemId", item.name)))?;
    let text = |name| {
        item.child_text(name)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let flag = |name| text(name).map(|value| value.eq_ignore_ascii_case("true"));
    let mut has_headers = false;
    let mut native_body = None;
    for property in item.children_named("ExtendedProperty") {
        let tag = property
            .child("ExtendedFieldURI")
            .and_then(|uri| uri.attribute("PropertyTag"))
            .map(|tag| {
                tag.trim_start_matches("0x")
                    .trim_start_matches("0X")
                    .to_ascii_lowercase()
            });
        let value = property.child_text("Value").unwrap_or_default().trim();
        match tag.as_deref().map(|t| t.trim_start_matches('0')) {
            Some("7d") => has_headers = !value.is_empty(),
            Some("1016") => native_body = value.parse().ok(),
            _ => {}
        }
    }
    let sender = item.path(&["From", "Mailbox"]).and_then(|mailbox| {
        mailbox
            .child_text("EmailAddress")
            .map(str::trim)
            .filter(|address| address.contains('@'))
            .map(str::to_lowercase)
            .or_else(|| {
                mailbox
                    .child_text("Name")
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            })
    });
    let is_message = item.name == "Message" || item.name.starts_with("Meeting");
    Ok(ItemRecord {
        key: key.to_owned(),
        folder: folder.to_owned(),
        class: text("ItemClass").unwrap_or_default(),
        message_id: text("InternetMessageId"),
        subject: text("Subject"),
        sender,
        sent: text("DateTimeSent").and_then(|t| parse_utc(&t)),
        received: text("DateTimeReceived").and_then(|t| parse_utc(&t)),
        created: text("DateTimeCreated").and_then(|t| parse_utc(&t)),
        size: text("Size").and_then(|size| size.parse().ok()),
        has_attachments: flag("HasAttachments"),
        read: flag("IsRead"),
        has_headers: is_message.then_some(has_headers),
        native_body,
        // Exchange leaves `Preview` out when the body has no text.
        body_empty: is_message.then(|| text("Preview").is_none()),
    })
}

#[cfg(test)]
mod tests {
    use super::{Export, ItemRef};
    use crate::{
        EwsClient,
        testing::{FakeTransport, soap},
    };

    #[test]
    fn lists_items_page_by_page() {
        let page = |last: bool, items: &str| {
            soap(&format!(
                r#"<m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder TotalItemsInView="3" IncludesLastItemInRange="{last}"><t:Items>{items}</t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"#
            ))
        };
        let fake = FakeTransport::new([
            (
                200,
                page(
                    false,
                    r#"<t:Message><t:ItemId Id="A" ChangeKey="1"/><t:ItemClass>IPM.Note</t:ItemClass><t:Size>100</t:Size></t:Message><t:CalendarItem><t:ItemId Id="B" ChangeKey="1"/><t:ItemClass>IPM.Appointment</t:ItemClass><t:Size>200</t:Size></t:CalendarItem>"#,
                ),
            ),
            (
                200,
                page(
                    true,
                    r#"<t:Contact><t:ItemId Id="C" ChangeKey="1"/><t:ItemClass>IPM.Contact</t:ItemClass><t:Size>300</t:Size></t:Contact>"#,
                ),
            ),
        ]);
        let items = EwsClient::new(&fake).list_items("F").unwrap();
        assert_eq!(
            items,
            [
                ("A", "IPM.Note", 100),
                ("B", "IPM.Appointment", 200),
                ("C", "IPM.Contact", 300)
            ]
            .map(|(id, class, size)| ItemRef {
                id: id.into(),
                class: class.into(),
                size
            })
        );
        assert!(fake.requests.borrow()[1].contains(r#"Offset="2""#));
    }

    #[test]
    fn inventories_damaged_and_healthy_messages() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder IncludesLastItemInRange="true"><t:Items><t:Message><t:ItemId Id="OK"/><t:ItemClass>IPM.Note</t:ItemClass><t:Subject>Order shipped</t:Subject><t:DateTimeReceived>2024-12-30T03:19:13Z</t:DateTimeReceived><t:Size>236262</t:Size><t:DateTimeSent>2024-12-30T03:19:09Z</t:DateTimeSent><t:DateTimeCreated>2026-09-01T00:07:02Z</t:DateTimeCreated><t:HasAttachments>false</t:HasAttachments><t:Preview>Your order is on its way</t:Preview><t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="0x7d" PropertyType="String"/><t:Value>Received: from mx</t:Value></t:ExtendedProperty><t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="0x1016" PropertyType="Integer"/><t:Value>3</t:Value></t:ExtendedProperty><t:InternetMessageId>&lt;a@example.test&gt;</t:InternetMessageId><t:From><t:Mailbox><t:Name>Shop</t:Name><t:EmailAddress>Orders@Example.TEST</t:EmailAddress></t:Mailbox></t:From><t:IsRead>true</t:IsRead></t:Message><t:Message><t:ItemId Id="BROKEN"/><t:ItemClass>IPM.Note</t:ItemClass><t:Subject>Photos</t:Subject><t:DateTimeReceived>2026-09-01T05:02:38Z</t:DateTimeReceived><t:DateTimeSent>2021-10-23T21:31:00Z</t:DateTimeSent><t:InternetMessageId>&lt;b@example.test&gt;</t:InternetMessageId><t:IsRead>false</t:IsRead></t:Message></t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"#,
            ),
        )]);
        let items = EwsClient::new(&fake).inventory_items("F", "").unwrap();
        let (ok, broken) = (&items[0], &items[1]);
        assert_eq!(ok.message_id.as_deref(), Some("<a@example.test>"));
        assert_eq!(ok.sender.as_deref(), Some("orders@example.test"));
        assert_eq!(
            (ok.has_headers, ok.native_body, ok.read),
            (Some(true), Some(3), Some(true))
        );
        assert_eq!(
            ok.received,
            ecm_model::item::parse_utc("2024-12-30T03:19:13Z")
        );
        assert_eq!(broken.sender, None);
        assert_eq!(broken.has_headers, Some(false));
        assert_eq!(broken.read, Some(false));
        assert_eq!(
            (ok.body_empty, broken.body_empty),
            (Some(false), Some(true))
        );
        assert!(fake.requests.borrow()[0].contains(r#"PropertyTag="0x007D""#));
    }

    #[test]
    fn exports_items_and_keeps_per_item_failures() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:ExportItemsResponse><m:ResponseMessages><m:ExportItemsResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:ItemId Id="A" ChangeKey="1"/><m:Data>AQIDBA==</m:Data></m:ExportItemsResponseMessage><m:ExportItemsResponseMessage ResponseClass="Error"><m:MessageText>The specified object was not found in the store.</m:MessageText><m:ResponseCode>ErrorItemNotFound</m:ResponseCode></m:ExportItemsResponseMessage></m:ResponseMessages></m:ExportItemsResponse>"#,
            ),
        )]);
        let exports = EwsClient::new(&fake).export_items(&["A", "B"]).unwrap();
        assert_eq!(exports[0], Export::Data(vec![1, 2, 3, 4]));
        assert!(matches!(&exports[1], Export::Failed { code, .. } if code == "ErrorItemNotFound"));
        let request = &fake.requests.borrow()[0];
        assert!(request.contains(r#"<m:ExportItems><m:ItemIds><t:ItemId Id="A"/><t:ItemId Id="B"/></m:ItemIds></m:ExportItems>"#));
    }
}
