//! Items: listing a folder with `FindItem`, exporting with `ExportItems`.

use base64::Engine as _;

use crate::{Error, EwsClient, Result, Transport, xml::escape};

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
