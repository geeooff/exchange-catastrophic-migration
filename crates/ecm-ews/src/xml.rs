//! A small XML tree for SOAP responses, and escaping for requests.
//!
//! EWS responses are small enough to hold in memory, except exports, which are
//! requested in batches sized to stay reasonable. Namespaces are dropped: element
//! and attribute names are their local names, which are unambiguous in EWS.

use std::borrow::Cow;

use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};

use crate::{Error, Result};

/// An XML element: local name, attributes, child elements and text content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    /// Local name, without namespace prefix.
    pub name: String,
    /// Attributes, by local name.
    pub attributes: Vec<(String, String)>,
    /// Child elements, in document order.
    pub children: Vec<Element>,
    /// Concatenated text content of this element (not of its children).
    pub text: String,
}

impl Element {
    /// The first child with this local name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|child| child.name == name)
    }

    /// Every child with this local name.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// Follows a path of child names from this element.
    pub fn path(&self, names: &[&str]) -> Option<&Element> {
        names
            .iter()
            .try_fold(self, |element, name| element.child(name))
    }

    /// Text of the first child with this local name.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|child| child.text.as_str())
    }

    /// Value of an attribute.
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every descendant with this local name, depth first.
    pub fn descendants_named<'a>(&'a self, name: &'a str, found: &mut Vec<&'a Element>) {
        for child in &self.children {
            if child.name == name {
                found.push(child);
            }
            child.descendants_named(name, found);
        }
    }
}

/// Parses a document into its root element.
pub fn parse(document: &[u8]) -> Result<Element> {
    let malformed = |reason: String| Error::MalformedResponse(reason);
    let mut reader = Reader::from_reader(document);
    let mut stack: Vec<Element> = Vec::new();
    let mut root = None;
    let mut buffer = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| malformed(error.to_string()))?;
        match event {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => {
                let element = element(&start)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(element),
                    None => root = Some(element),
                }
            }
            Event::End(_) => {
                let element = stack
                    .pop()
                    .ok_or_else(|| malformed("unbalanced end tag".into()))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(element),
                    None => root = Some(element),
                }
            }
            Event::Text(text) => {
                if let Some(current) = stack.last_mut() {
                    current.text.push_str(&text.xml10_content());
                }
            }
            Event::CData(data) => {
                if let Some(current) = stack.last_mut() {
                    current.text.push_str(&data.xml10_content());
                }
            }
            Event::GeneralRef(reference) => {
                let resolved = if reference.is_char_ref() {
                    reference
                        .resolve_char_ref()
                        .map_err(|error| malformed(error.to_string()))?
                } else {
                    match reference.xml10_content().as_ref() {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        other => return Err(malformed(format!("unknown entity &{other};"))),
                    }
                };
                if let (Some(c), Some(current)) = (resolved, stack.last_mut()) {
                    current.text.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !stack.is_empty() {
        return Err(malformed("the document ends inside an element".into()));
    }
    root.ok_or_else(|| malformed("empty document".into()))
}

fn element(start: &BytesStart<'_>) -> Result<Element> {
    let mut attributes = Vec::new();
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|error| Error::MalformedResponse(error.to_string()))?;
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|error| Error::MalformedResponse(error.to_string()))?;
        attributes.push((
            attribute.key.local_name().as_ref().to_owned(),
            value.into_owned(),
        ));
    }
    Ok(Element {
        name: start.local_name().as_ref().to_owned(),
        attributes,
        ..Default::default()
    })
}

/// Escapes text for an element or attribute value.
pub fn escape(text: &str) -> Cow<'_, str> {
    quick_xml::escape::escape(text)
}

#[cfg(test)]
mod tests {
    use super::{escape, parse};

    #[test]
    fn parses_names_attributes_text_and_entities() {
        let root = parse(
            br#"<?xml version="1.0"?>
            <s:Envelope xmlns:s="urn:s" xmlns:t="urn:t">
              <t:Folder t:Kind="mail"><t:DisplayName>Factures &amp; re&#231;us</t:DisplayName><t:Empty/></t:Folder>
            </s:Envelope>"#,
        )
        .unwrap();
        assert_eq!(root.name, "Envelope");
        let folder = root.child("Folder").unwrap();
        assert_eq!(folder.attribute("Kind"), Some("mail"));
        assert_eq!(folder.child_text("DisplayName"), Some("Factures & reçus"));
        assert!(folder.child("Empty").is_some());
    }

    #[test]
    fn refuses_truncated_documents() {
        assert!(parse(b"<a><b>").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(escape("a<b & \"c\""), "a&lt;b &amp; &quot;c&quot;");
    }
}
