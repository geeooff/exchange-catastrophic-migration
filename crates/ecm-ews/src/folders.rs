//! Folder hierarchy: `GetFolder` and `FindFolder`.

use std::collections::HashMap;

use ecm_model::FolderSummary;

use crate::{Error, EwsClient, Result, Transport, xml::Element};

/// `PR_ASSOC_CONTENT_COUNT`.
const PR_ASSOC_CONTENT_COUNT: u32 = 0x3617;
/// `PR_MESSAGE_SIZE_EXTENDED`: total size of the folder's items, in bytes.
const PR_MESSAGE_SIZE_EXTENDED: u32 = 0x0E08;
/// `PR_ATTR_HIDDEN`.
const PR_ATTR_HIDDEN: u32 = 0x10F4;

/// Folders returned per `FindFolder` page.
const PAGE_SIZE: usize = 500;

const FOLDER_SHAPE: &str = r#"<m:FolderShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="folder:ParentFolderId"/><t:FieldURI FieldURI="folder:DisplayName"/><t:FieldURI FieldURI="folder:FolderClass"/><t:FieldURI FieldURI="folder:TotalCount"/><t:FieldURI FieldURI="folder:ChildFolderCount"/><t:FieldURI FieldURI="folder:UnreadCount"/><t:ExtendedFieldURI PropertyTag="0x3617" PropertyType="Integer"/><t:ExtendedFieldURI PropertyTag="0x0E08" PropertyType="Long"/><t:ExtendedFieldURI PropertyTag="0x10F4" PropertyType="Boolean"/></t:AdditionalProperties></m:FolderShape>"#;

/// The kind of folder element EWS returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    /// `t:Folder`: mail and anything else.
    Generic,
    /// `t:CalendarFolder`.
    Calendar,
    /// `t:ContactsFolder`.
    Contacts,
    /// `t:TasksFolder`.
    Tasks,
    /// `t:SearchFolder`: a saved search, whose items live elsewhere.
    Search,
}

/// A folder as EWS describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EwsFolder {
    /// EWS folder id.
    pub id: String,
    /// EWS id of the parent folder.
    pub parent_id: Option<String>,
    /// Element kind.
    pub kind: FolderKind,
    /// Display name.
    pub name: String,
    /// Folder class, e.g. `IPF.Note`.
    pub class: Option<String>,
    /// Number of normal items.
    pub total_count: u64,
    /// Number of unread items.
    pub unread_count: u64,
    /// Number of sub-folders.
    pub child_folder_count: u64,
    /// Number of associated items (views, rules…).
    pub associated_count: u64,
    /// Total size of the items, in bytes, when the server reports it.
    pub size: Option<u64>,
    /// Whether the folder is hidden (`PR_ATTR_HIDDEN`).
    pub hidden: bool,
}

impl EwsFolder {
    fn from_element(element: &Element) -> Result<Self> {
        let kind = match element.name.as_str() {
            "Folder" => FolderKind::Generic,
            "CalendarFolder" => FolderKind::Calendar,
            "ContactsFolder" => FolderKind::Contacts,
            "TasksFolder" => FolderKind::Tasks,
            "SearchFolder" => FolderKind::Search,
            other => {
                return Err(Error::MalformedResponse(format!(
                    "unknown folder element {other}"
                )));
            }
        };
        let id = element
            .child("FolderId")
            .and_then(|id| id.attribute("Id"))
            .ok_or_else(|| Error::MalformedResponse("folder without FolderId".into()))?;
        let number = |name| {
            element
                .child_text(name)
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0)
        };
        let mut folder = Self {
            id: id.to_owned(),
            parent_id: element
                .child("ParentFolderId")
                .and_then(|id| id.attribute("Id"))
                .map(str::to_owned),
            kind,
            name: element
                .child_text("DisplayName")
                .unwrap_or_default()
                .to_owned(),
            class: element
                .child_text("FolderClass")
                .filter(|class| !class.is_empty())
                .map(str::to_owned),
            total_count: number("TotalCount"),
            unread_count: number("UnreadCount"),
            child_folder_count: number("ChildFolderCount"),
            associated_count: 0,
            size: None,
            hidden: false,
        };
        for property in element.children_named("ExtendedProperty") {
            let tag = property
                .child("ExtendedFieldURI")
                .and_then(|uri| uri.attribute("PropertyTag"))
                .and_then(parse_tag);
            let value = property.child_text("Value").unwrap_or_default().trim();
            match tag {
                Some(PR_ASSOC_CONTENT_COUNT) => {
                    folder.associated_count = value.parse().unwrap_or(0)
                }
                Some(PR_MESSAGE_SIZE_EXTENDED) => folder.size = value.parse().ok(),
                Some(PR_ATTR_HIDDEN) => folder.hidden = value.eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
        Ok(folder)
    }
}

/// Parses a property tag as EWS writes it back: `0x3617`, `0xe08`, or decimal.
fn parse_tag(tag: &str) -> Option<u32> {
    match tag.strip_prefix("0x").or_else(|| tag.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => tag.parse().ok(),
    }
}

/// A folder and every folder below it.
#[derive(Debug, Clone)]
pub struct FolderTree {
    /// The folder the tree starts at.
    pub root: EwsFolder,
    /// Every folder below it, in the order the server listed them.
    pub descendants: Vec<EwsFolder>,
}

impl FolderTree {
    /// The tree as a [`FolderSummary`], children sorted by name.
    pub fn summary(&self) -> FolderSummary {
        let children = self.children_by_parent();
        build_summary(&self.root, &children)
    }

    /// Every folder of the tree, the root first, with its path from the root
    /// (`""` for the root, `"Inbox/Receipts"` below it), depth first by name.
    pub fn with_paths(&self) -> Vec<(String, &EwsFolder)> {
        let children = self.children_by_parent();
        let mut out = Vec::new();
        collect_paths(&self.root, String::new(), &children, &mut out);
        out
    }

    fn children_by_parent(&self) -> HashMap<&str, Vec<&EwsFolder>> {
        let mut children: HashMap<&str, Vec<&EwsFolder>> = HashMap::new();
        for folder in &self.descendants {
            if let Some(parent) = &folder.parent_id {
                children.entry(parent.as_str()).or_default().push(folder);
            }
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| a.name.cmp(&b.name));
        }
        children
    }
}

fn build_summary(folder: &EwsFolder, children: &HashMap<&str, Vec<&EwsFolder>>) -> FolderSummary {
    FolderSummary {
        name: folder.name.clone(),
        class: folder.class.clone(),
        item_count: folder.total_count,
        unread_count: folder.unread_count,
        associated_count: folder.associated_count,
        size: folder.size,
        children: children
            .get(folder.id.as_str())
            .map(|list| {
                list.iter()
                    .map(|child| build_summary(child, children))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn collect_paths<'a>(
    folder: &'a EwsFolder,
    path: String,
    children: &HashMap<&str, Vec<&'a EwsFolder>>,
    out: &mut Vec<(String, &'a EwsFolder)>,
) {
    out.push((path.clone(), folder));
    for child in children.get(folder.id.as_str()).into_iter().flatten() {
        let child_path = if path.is_empty() {
            child.name.clone()
        } else {
            format!("{path}/{}", child.name)
        };
        collect_paths(child, child_path, children, out);
    }
}

impl<T: Transport> EwsClient<T> {
    /// Reads one folder by its distinguished name, e.g. `msgfolderroot`.
    pub fn get_distinguished_folder(&self, distinguished: &str) -> Result<EwsFolder> {
        let body = format!(
            r#"<m:GetFolder>{FOLDER_SHAPE}<m:FolderIds><t:DistinguishedFolderId Id="{}"/></m:FolderIds></m:GetFolder>"#,
            crate::xml::escape(distinguished)
        );
        let message = self
            .call(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::MalformedResponse("GetFolder returned no message".into()))?
            .into_result()?;
        let folder = message
            .element
            .child("Folders")
            .and_then(|folders| folders.children.first())
            .ok_or_else(|| Error::MalformedResponse("GetFolder returned no folder".into()))?;
        EwsFolder::from_element(folder)
    }

    /// Every folder below `parent_id`, at any depth, page by page.
    pub fn find_folders_deep(&self, parent_id: &str) -> Result<Vec<EwsFolder>> {
        let mut folders = Vec::new();
        loop {
            let body = format!(
                r#"<m:FindFolder Traversal="Deep">{FOLDER_SHAPE}<m:IndexedPageFolderView MaxEntriesReturned="{PAGE_SIZE}" Offset="{}" BasePoint="Beginning"/><m:ParentFolderIds><t:FolderId Id="{}"/></m:ParentFolderIds></m:FindFolder>"#,
                folders.len(),
                crate::xml::escape(parent_id)
            );
            let message = self
                .call(&body)?
                .into_iter()
                .next()
                .ok_or_else(|| Error::MalformedResponse("FindFolder returned no message".into()))?
                .into_result()?;
            let root = message
                .element
                .child("RootFolder")
                .ok_or_else(|| Error::MalformedResponse("FindFolder without RootFolder".into()))?;
            let page: Vec<_> = root
                .child("Folders")
                .map(|list| list.children.iter().map(EwsFolder::from_element).collect())
                .transpose()?
                .unwrap_or_default();
            let last = root.attribute("IncludesLastItemInRange") != Some("false");
            let empty = page.is_empty();
            folders.extend(page);
            if last || empty {
                return Ok(folders);
            }
        }
    }

    /// A folder, by distinguished name, and every folder below it.
    pub fn folder_tree(&self, distinguished: &str) -> Result<FolderTree> {
        let root = self.get_distinguished_folder(distinguished)?;
        let descendants = self.find_folders_deep(&root.id)?;
        Ok(FolderTree { root, descendants })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        EwsClient,
        testing::{FakeTransport, soap},
    };

    fn folder(kind: &str, id: &str, parent: &str, name: &str, class: &str, total: u32) -> String {
        format!(
            r#"<t:{kind}><t:FolderId Id="{id}" ChangeKey="ck"/><t:ParentFolderId Id="{parent}" ChangeKey="ck"/><t:FolderClass>{class}</t:FolderClass><t:DisplayName>{name}</t:DisplayName><t:TotalCount>{total}</t:TotalCount><t:ChildFolderCount>0</t:ChildFolderCount><t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="0xe08" PropertyType="Long"/><t:Value>{}</t:Value></t:ExtendedProperty><t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="0x3617" PropertyType="Integer"/><t:Value>2</t:Value></t:ExtendedProperty><t:UnreadCount>1</t:UnreadCount></t:{kind}>"#,
            total * 1000
        )
    }

    #[test]
    fn builds_the_tree_from_get_folder_and_paged_find_folder() {
        let get = soap(&format!(
            r#"<m:GetFolderResponse><m:ResponseMessages><m:GetFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Folders>{}</m:Folders></m:GetFolderResponseMessage></m:ResponseMessages></m:GetFolderResponse>"#,
            folder(
                "Folder",
                "ROOT",
                "TOP",
                "Top of Information Store",
                "",
                1200
            )
        ));
        let page = |offset: u32, last: bool, folders: String| {
            soap(&format!(
                r#"<m:FindFolderResponse><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder IndexedPagingOffset="{offset}" TotalItemsInView="3" IncludesLastItemInRange="{last}"><t:Folders>{folders}</t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"#
            ))
        };
        let fake = FakeTransport::new([
            (200, get),
            (
                200,
                page(
                    2,
                    false,
                    folder(
                        "Folder",
                        "INBOX",
                        "ROOT",
                        "Boîte de réception",
                        "IPF.Note",
                        40,
                    ) + &folder(
                        "CalendarFolder",
                        "CAL",
                        "ROOT",
                        "Calendrier",
                        "IPF.Appointment",
                        7,
                    ),
                ),
            ),
            (
                200,
                page(
                    3,
                    true,
                    folder(
                        "Folder",
                        "SUB",
                        "INBOX",
                        "Factures &amp; reçus",
                        "IPF.Note",
                        5,
                    ),
                ),
            ),
        ]);

        let tree = EwsClient::new(&fake).folder_tree("msgfolderroot").unwrap();

        let requests = fake.requests.borrow();
        assert!(requests[0].contains(r#"<t:DistinguishedFolderId Id="msgfolderroot"/>"#));
        assert!(
            requests[1].contains(r#"Offset="0""#)
                && requests[1].contains(r#"<t:FolderId Id="ROOT"/>"#)
        );
        assert!(requests[2].contains(r#"Offset="2""#));

        let summary = tree.summary();
        assert_eq!(summary.item_count, 1200);
        assert_eq!(summary.size, Some(1_200_000));
        let names: Vec<_> = summary.children.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Boîte de réception", "Calendrier"]);
        assert_eq!(summary.children[0].children[0].name, "Factures & reçus");
        assert_eq!(summary.children[0].associated_count, 2);
        assert_eq!(summary.total_item_count(), 1200 + 40 + 7 + 5);

        let paths: Vec<_> = tree
            .with_paths()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(
            paths,
            [
                "",
                "Boîte de réception",
                "Boîte de réception/Factures & reçus",
                "Calendrier"
            ]
        );
    }

    #[test]
    fn a_failed_message_is_an_error() {
        let fake = FakeTransport::new([(
            200,
            soap(
                r#"<m:GetFolderResponse><m:ResponseMessages><m:GetFolderResponseMessage ResponseClass="Error"><m:MessageText>The specified folder could not be found in the store.</m:MessageText><m:ResponseCode>ErrorFolderNotFound</m:ResponseCode></m:GetFolderResponseMessage></m:ResponseMessages></m:GetFolderResponse>"#,
            ),
        )]);
        let error = EwsClient::new(&fake)
            .get_distinguished_folder("archivemsgfolderroot")
            .unwrap_err();
        assert!(error.to_string().contains("ErrorFolderNotFound"));
    }
}
