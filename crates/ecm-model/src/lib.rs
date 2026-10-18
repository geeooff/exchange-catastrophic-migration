//! Common model shared by every crate: folders, items, property bags (MAPI property
//! tags and named properties) and the fingerprints used to match items between a
//! source and a target.

#![forbid(unsafe_code)]

pub mod compare;
pub mod item;

pub use item::ItemRecord;

/// A folder and its sub-folders, with the counts a source or a target reports.
///
/// The same shape is produced from a PST file and from an Exchange mailbox, so the
/// two trees can be printed and compared alike.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderSummary {
    /// Display name (`PR_DISPLAY_NAME`).
    pub name: String,
    /// Folder class (`PR_CONTAINER_CLASS`), e.g. `IPF.Note`. `None` when the folder
    /// has none, which some clients treat as a mail folder and others hide.
    pub class: Option<String>,
    /// Number of normal items (`PR_CONTENT_COUNT`).
    pub item_count: u64,
    /// Number of unread items (`PR_CONTENT_UNREAD`).
    pub unread_count: u64,
    /// Number of hidden, associated items such as views and rules
    /// (`PR_ASSOC_CONTENT_COUNT`).
    pub associated_count: u64,
    /// Total size of the folder's items in bytes (`PR_MESSAGE_SIZE_EXTENDED`), when
    /// the source reports it.
    pub size: Option<u64>,
    /// Sub-folders, sorted by name.
    pub children: Vec<FolderSummary>,
}

impl FolderSummary {
    /// Number of folders in this tree, this one included.
    pub fn folder_count(&self) -> usize {
        1 + self
            .children
            .iter()
            .map(FolderSummary::folder_count)
            .sum::<usize>()
    }

    /// Number of normal items in this tree, this folder included.
    pub fn total_item_count(&self) -> u64 {
        self.item_count
            + self
                .children
                .iter()
                .map(FolderSummary::total_item_count)
                .sum::<u64>()
    }

    /// Total size of the items in this tree, counting only folders that report one.
    pub fn total_size(&self) -> u64 {
        self.size.unwrap_or(0)
            + self
                .children
                .iter()
                .map(FolderSummary::total_size)
                .sum::<u64>()
    }

    /// Visits this folder and every sub-folder, depth first, with its depth
    /// (0 for this folder).
    pub fn walk<'a>(&'a self, visit: &mut impl FnMut(&'a FolderSummary, usize)) {
        self.walk_at(0, visit);
    }

    fn walk_at<'a>(&'a self, depth: usize, visit: &mut impl FnMut(&'a FolderSummary, usize)) {
        visit(self, depth);
        for child in &self.children {
            child.walk_at(depth + 1, visit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FolderSummary;

    fn folder(name: &str, items: u64, children: Vec<FolderSummary>) -> FolderSummary {
        FolderSummary {
            name: name.to_owned(),
            item_count: items,
            children,
            ..Default::default()
        }
    }

    #[test]
    fn totals_cover_the_whole_tree() {
        let tree = folder(
            "root",
            1,
            vec![
                folder("Inbox", 10, vec![folder("Receipts", 5, vec![])]),
                folder("Sent", 3, vec![]),
            ],
        );
        assert_eq!(tree.folder_count(), 4);
        assert_eq!(tree.total_item_count(), 19);
    }

    #[test]
    fn walk_is_depth_first_with_depths() {
        let tree = folder(
            "root",
            0,
            vec![
                folder("a", 0, vec![folder("a1", 0, vec![])]),
                folder("b", 0, vec![]),
            ],
        );
        let mut seen = Vec::new();
        tree.walk(&mut |f, depth| seen.push((f.name.clone(), depth)));
        assert_eq!(
            seen,
            [("root", 0), ("a", 1), ("a1", 2), ("b", 1)].map(|(n, d)| (n.to_owned(), d))
        );
    }
}
