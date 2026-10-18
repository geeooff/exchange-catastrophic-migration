//! Detailed properties of items, to compare copies (`GetItem`). Read-only, and
//! does not change the read state.

use ecm_model::{
    compare::{PropertyMap, address_list},
    item::parse_utc,
};

use crate::{Error, EwsClient, Result, Transport, xml::Element, xml::escape};

const DETAIL_PROPERTIES: &str = concat!(
    r#"<t:FieldURI FieldURI="item:ItemClass"/>"#,
    r#"<t:FieldURI FieldURI="item:Subject"/>"#,
    r#"<t:FieldURI FieldURI="item:Body"/>"#,
    r#"<t:FieldURI FieldURI="item:Attachments"/>"#,
    r#"<t:FieldURI FieldURI="item:HasAttachments"/>"#,
    r#"<t:FieldURI FieldURI="item:DateTimeReceived"/>"#,
    r#"<t:FieldURI FieldURI="item:DateTimeSent"/>"#,
    r#"<t:FieldURI FieldURI="item:Size"/>"#,
    r#"<t:FieldURI FieldURI="message:From"/>"#,
    r#"<t:FieldURI FieldURI="message:ToRecipients"/>"#,
    r#"<t:FieldURI FieldURI="message:CcRecipients"/>"#,
    r#"<t:FieldURI FieldURI="message:IsRead"/>"#,
    r#"<t:FieldURI FieldURI="message:InternetMessageId"/>"#,
    r#"<t:ExtendedFieldURI PropertyTag="0x007D" PropertyType="String"/>"#,
    r#"<t:ExtendedFieldURI PropertyTag="0x0E07" PropertyType="Integer"/>"#,
    r#"<t:ExtendedFieldURI PropertyTag="0x1016" PropertyType="Integer"/>"#,
);

impl<T: Transport> EwsClient<T> {
    /// Detailed, normalized properties of items, one result per id.
    pub fn item_properties(&self, ids: &[&str]) -> Result<Vec<Result<PropertyMap>>> {
        let mut body = format!(
            "<m:GetItem><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:BodyType>Best</t:BodyType><t:AdditionalProperties>{DETAIL_PROPERTIES}</t:AdditionalProperties></m:ItemShape><m:ItemIds>"
        );
        for id in ids {
            body.push_str(&format!(r#"<t:ItemId Id="{}"/>"#, escape(id)));
        }
        body.push_str("</m:ItemIds></m:GetItem>");
        Ok(self
            .call(&body)?
            .into_iter()
            .map(|message| {
                let message = message.into_result()?;
                let item = message
                    .element
                    .child("Items")
                    .and_then(|items| items.children.first())
                    .ok_or_else(|| Error::MalformedResponse("GetItem returned no item".into()))?;
                Ok(properties(item))
            })
            .collect())
    }
}

/// A file attached to an item, with its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// File name.
    pub name: String,
    /// MIME content type, as stored.
    pub content_type: String,
    /// The file's bytes.
    pub content: Vec<u8>,
}

impl<T: Transport> EwsClient<T> {
    /// The file attachments of an item, with their content (`GetItem`, then
    /// `GetAttachment`). Only reads.
    pub fn attachments(&self, item_id: &str) -> Result<Vec<Attachment>> {
        let body = format!(
            r#"<m:GetItem><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="item:Attachments"/></t:AdditionalProperties></m:ItemShape><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:GetItem>"#,
            escape(item_id)
        );
        let message = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("GetItem returned no message".into()))?
            .into_result()?;
        let ids: Vec<String> = message
            .element
            .path(&["Items"])
            .and_then(|items| items.children.first())
            .and_then(|item| item.child("Attachments"))
            .map(|attachments| {
                attachments
                    .children
                    .iter()
                    .filter(|a| a.name == "FileAttachment")
                    .filter_map(|a| a.child("AttachmentId")?.attribute("Id"))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut body = String::from("<m:GetAttachment><m:AttachmentIds>");
        for id in &ids {
            body.push_str(&format!(r#"<t:AttachmentId Id="{}"/>"#, escape(id)));
        }
        body.push_str("</m:AttachmentIds></m:GetAttachment>");
        let mut found = Vec::new();
        for message in self.call(&body)? {
            let message = message.into_result()?;
            for attachment in message
                .element
                .child("Attachments")
                .map(|a| a.children.as_slice())
                .unwrap_or_default()
            {
                use base64::Engine as _;
                let content = attachment.child_text("Content").unwrap_or_default();
                found.push(Attachment {
                    name: attachment.child_text("Name").unwrap_or_default().to_owned(),
                    content_type: attachment
                        .child_text("ContentType")
                        .unwrap_or_default()
                        .to_owned(),
                    content: base64::engine::general_purpose::STANDARD
                        .decode(content.trim())
                        .map_err(|e| {
                            Error::MalformedResponse(format!("attachment content: {e}"))
                        })?,
                });
            }
        }
        Ok(found)
    }
}

/// Normalizes an EWS item element into a [`PropertyMap`].
pub fn properties(item: &Element) -> PropertyMap {
    let mut map = PropertyMap::new();
    let mut put = |name: &str, value: Option<String>| {
        if let Some(value) = value {
            map.insert(name.to_owned(), value);
        }
    };
    let text = |name: &str| {
        item.child_text(name)
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    };
    let addresses = |name: &str| {
        item.child(name).map(|list| {
            address_list(
                list.children_named("Mailbox")
                    .filter_map(|mailbox| mailbox.child_text("EmailAddress")),
            )
        })
    };
    put("class", text("ItemClass"));
    put("subject", text("Subject"));
    put(
        "from",
        item.path(&["From", "Mailbox"])
            .and_then(|mailbox| mailbox.child_text("EmailAddress"))
            .map(|a| a.trim().to_lowercase()),
    );
    put("to", addresses("ToRecipients"));
    put("cc", addresses("CcRecipients"));
    put(
        "sent",
        text("DateTimeSent")
            .and_then(|t| parse_utc(&t))
            .map(|t| t.to_string()),
    );
    put(
        "received",
        text("DateTimeReceived")
            .and_then(|t| parse_utc(&t))
            .map(|t| t.to_string()),
    );
    put("message_id", text("InternetMessageId"));
    put("read", text("IsRead").map(|r| r.to_lowercase()));
    put(
        "has_attachments",
        text("HasAttachments").map(|r| r.to_lowercase()),
    );
    put("size", text("Size"));
    if let Some(attachments) = item.child("Attachments") {
        let mut names: Vec<String> = attachments
            .children
            .iter()
            .filter_map(|a| a.child_text("Name"))
            .map(|n| n.trim().to_owned())
            .collect();
        names.sort();
        put("attachments", Some(names.join(";")));
    } else {
        put("attachments", Some(String::new()));
    }
    if let Some(body) = item.child("Body") {
        put(
            "body_type",
            body.attribute("BodyType").map(|t| t.to_lowercase()),
        );
        put("body_length", Some(body.text.chars().count().to_string()));
        put(
            "body_has_markup_in_text",
            Some(
                (body.attribute("BodyType") == Some("Text")
                    && ["<html", "<div", "<table", "<body", "<br"]
                        .iter()
                        .any(|tag| body.text.to_lowercase().contains(tag)))
                .to_string(),
            ),
        );
    }
    for property in item.children_named("ExtendedProperty") {
        let tag = property
            .child("ExtendedFieldURI")
            .and_then(|uri| uri.attribute("PropertyTag"))
            .and_then(|tag| {
                u32::from_str_radix(tag.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
            });
        let value = property
            .child_text("Value")
            .unwrap_or_default()
            .trim()
            .to_owned();
        match tag {
            Some(0x007D) => put("headers", Some((!value.is_empty()).to_string())),
            Some(0x0E07) => put("message_flags", Some(value)),
            Some(0x1016) => put("native_body", Some(value)),
            _ => {}
        }
    }
    map.entry("headers".to_owned())
        .or_insert_with(|| "false".to_owned());
    map
}

#[cfg(test)]
mod tests {
    use crate::{
        EwsClient,
        testing::{FakeTransport, soap},
    };

    #[test]
    fn reads_attachments_with_their_content() {
        let fake = FakeTransport::new([
            (
                200,
                soap(
                    r#"<m:GetItemResponse><m:ResponseMessages><m:GetItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:Message><t:ItemId Id="A"/><t:Attachments><t:FileAttachment><t:AttachmentId Id="AT1"/><t:Name>a.jpg</t:Name></t:FileAttachment><t:ItemAttachment><t:AttachmentId Id="AT2"/></t:ItemAttachment></t:Attachments></t:Message></m:Items></m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"#,
                ),
            ),
            (
                200,
                soap(
                    r#"<m:GetAttachmentResponse><m:ResponseMessages><m:GetAttachmentResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Attachments><t:FileAttachment><t:AttachmentId Id="AT1"/><t:Name>a.jpg</t:Name><t:ContentType>image/jpeg</t:ContentType><t:Content>/9j/</t:Content></t:FileAttachment></m:Attachments></m:GetAttachmentResponseMessage></m:ResponseMessages></m:GetAttachmentResponse>"#,
                ),
            ),
        ]);
        let found = EwsClient::new(&fake).attachments("A").unwrap();
        assert_eq!(
            found,
            [super::Attachment {
                name: "a.jpg".into(),
                content_type: "image/jpeg".into(),
                content: vec![0xFF, 0xD8, 0xFF],
            }]
        );
        assert!(
            fake.requests.borrow()[1]
                .contains(r#"<m:AttachmentIds><t:AttachmentId Id="AT1"/></m:AttachmentIds>"#)
        );
    }

    #[test]
    fn normalizes_item_properties() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:GetItemResponse><m:ResponseMessages><m:GetItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Items><t:Message><t:ItemId Id="A"/><t:ItemClass>IPM.Note</t:ItemClass><t:Subject>Hi</t:Subject><t:Body BodyType="Text">&lt;html&gt;&lt;body&gt;Hi&lt;/body&gt;&lt;/html&gt;</t:Body><t:Attachments><t:FileAttachment><t:Name>b.pdf</t:Name></t:FileAttachment><t:FileAttachment><t:Name>a.jpg</t:Name></t:FileAttachment></t:Attachments><t:DateTimeSent>2019-11-14T10:00:00Z</t:DateTimeSent><t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="0x1016" PropertyType="Integer"/><t:Value>1</t:Value></t:ExtendedProperty><t:ToRecipients><t:Mailbox><t:EmailAddress>B@x.test</t:EmailAddress></t:Mailbox><t:Mailbox><t:EmailAddress>a@x.test</t:EmailAddress></t:Mailbox></t:ToRecipients><t:IsRead>true</t:IsRead></t:Message></m:Items></m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"#,
            ),
        )]);
        let map = EwsClient::new(&fake)
            .item_properties(&["A"])
            .unwrap()
            .remove(0)
            .unwrap();
        assert_eq!(map["body_type"], "text");
        assert_eq!(map["body_has_markup_in_text"], "true");
        assert_eq!(map["attachments"], "a.jpg;b.pdf");
        assert_eq!(map["to"], "a@x.test;b@x.test");
        assert_eq!(map["native_body"], "1");
        assert_eq!(map["headers"], "false");
        assert_eq!(map["read"], "true");
        assert!(fake.requests.borrow()[0].contains("<t:BodyType>Best</t:BodyType>"));
    }
}
