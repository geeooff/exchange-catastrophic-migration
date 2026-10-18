//! Read-only PST source.
//!
//! Built on Microsoft's `outlook-pst` crate. A PST file is always opened with a
//! read-only file handle, and its checksum is verified before and after a run.
//!
//! `outlook_pst::open_store` is deliberately not used: it also opens the file for
//! writing whenever it can. This crate opens the file itself, read-only, and hands
//! the handle to `outlook-pst`, which then has no way to write. On Windows the
//! handle also denies write access to other processes while it is open.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fmt,
    fs::{File, OpenOptions},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    rc::Rc,
};

use ecm_model::{FolderSummary, ItemRecord, item::filetime_to_unix};
use outlook_pst::{
    AnsiPstFile, UnicodePstFile,
    ltp::{
        prop_context::PropertyValue,
        table_context::{TableContext, TableRowData},
    },
    messaging::{
        folder::Folder,
        store::{AnsiStore, EntryId, Store, UnicodeStore},
    },
    ndb::node_id::NodeId,
};
use sha2::{Digest, Sha256};

/// `PR_CONTAINER_CLASS`.
const PR_CONTAINER_CLASS: u16 = 0x3613;
/// `PR_ASSOC_CONTENT_COUNT`.
const PR_ASSOC_CONTENT_COUNT: u16 = 0x3617;
/// `PR_MESSAGE_SIZE_EXTENDED`.
const PR_MESSAGE_SIZE_EXTENDED: u16 = 0x0E08;

/// Errors from reading a PST file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be opened or read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The PST file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is not a PST file `outlook-pst` can read, or a structure in it is
    /// damaged.
    #[error("{path} is not a readable PST file: {source}")]
    Format {
        /// The PST file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file changed between two checksums of the same run.
    #[error("{path} changed during the run (SHA-256 {before} before, {after} after)")]
    Modified {
        /// The PST file.
        path: PathBuf,
        /// Checksum taken before the run.
        before: Sha256Digest,
        /// Checksum taken after the run.
        after: Sha256Digest,
    },
}

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A SHA-256 digest, printed in lower-case hexadecimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sha256Digest(pub [u8; 32]);

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// On-disk format of a PST file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PstFormat {
    /// Unicode PST (Outlook 2003 and later), the format Outlook and Microsoft 365
    /// export today.
    Unicode,
    /// ANSI PST (Outlook 97 to 2002), limited to 2 GB.
    Ansi,
}

impl fmt::Display for PstFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PstFormat::Unicode => "Unicode",
            PstFormat::Ansi => "ANSI",
        })
    }
}

/// Opens `path` for reading only.
///
/// On Windows, other processes may still read the file but not write to it, rename
/// it or delete it while the handle is open.
pub fn open_read_only(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        /// `FILE_SHARE_READ`: share for reading, deny writing and deleting.
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path)
}

/// Computes the SHA-256 of a file, reading it through [`open_read_only`].
pub fn sha256_file(path: &Path) -> Result<Sha256Digest> {
    let io_error = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    let mut reader = BufReader::with_capacity(1 << 20, open_read_only(path).map_err(io_error)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = reader.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Sha256Digest(hasher.finalize().into()))
}

/// An open PST file.
pub struct PstSource {
    path: PathBuf,
    format: PstFormat,
    store: Rc<dyn Store>,
}

impl fmt::Debug for PstSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PstSource")
            .field("path", &self.path)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl PstSource {
    /// Opens a PST file, read-only.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let io_error = |source| Error::Io {
            path: path.clone(),
            source,
        };
        let format_error = |source| Error::Format {
            path: path.clone(),
            source,
        };

        // Both formats start with the same magic; the version in the header tells
        // them apart, and `read_from` refuses the one it does not handle. A fresh
        // handle for each attempt, since a failed attempt consumes its reader.
        let unicode = UnicodePstFile::read_from(Box::new(open_read_only(&path).map_err(io_error)?));
        let (format, store): (PstFormat, Rc<dyn Store>) = match unicode {
            Ok(pst) => (
                PstFormat::Unicode,
                UnicodeStore::read(Rc::new(pst)).map_err(format_error)?,
            ),
            Err(unicode_error) => {
                let ansi =
                    AnsiPstFile::read_from(Box::new(open_read_only(&path).map_err(io_error)?))
                        .map_err(|_| format_error(unicode_error))?;
                (
                    PstFormat::Ansi,
                    AnsiStore::read(Rc::new(ansi)).map_err(format_error)?,
                )
            }
        };

        Ok(Self {
            path,
            format,
            store,
        })
    }

    /// The path the file was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// On-disk format of the file.
    pub fn format(&self) -> PstFormat {
        self.format
    }

    /// Display name of the store (`PR_DISPLAY_NAME`), if it has one.
    pub fn display_name(&self) -> Option<String> {
        self.store.properties().display_name().ok()
    }

    /// The folders a mail client shows: the IPM subtree, with every sub-folder.
    pub fn ipm_folders(&self) -> Result<FolderSummary> {
        let entry_id = self
            .store
            .properties()
            .ipm_sub_tree_entry_id()
            .map_err(|source| self.format_error(source))?;
        self.folder_tree(&entry_id)
    }

    /// Every folder of the store, starting at its root: the IPM subtree, search
    /// folders and the other folders a mail client does not show.
    pub fn all_folders(&self) -> Result<FolderSummary> {
        let root = self
            .store
            .properties()
            .make_entry_id(NodeId::from(NID_ROOT_FOLDER))
            .map_err(|source| self.format_error(source))?;
        self.folder_tree(&root)
    }

    fn folder_tree(&self, entry_id: &EntryId) -> Result<FolderSummary> {
        let folder = self
            .store
            .open_folder(entry_id)
            .map_err(|source| self.format_error(source))?;
        self.summarize(folder.as_ref())
    }

    fn summarize(&self, folder: &dyn Folder) -> Result<FolderSummary> {
        let properties = folder.properties();
        let count = |value: io::Result<i32>| value.map_or(0, |n| u64::try_from(n).unwrap_or(0));

        let mut children = Vec::new();
        if let Some(hierarchy) = folder.hierarchy_table() {
            for row in hierarchy.rows_matrix() {
                let node = NodeId::from(u32::from(row.id()));
                let entry_id = self
                    .store
                    .properties()
                    .make_entry_id(node)
                    .map_err(|source| self.format_error(source))?;
                children.push(self.folder_tree(&entry_id)?);
            }
        }
        children.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(FolderSummary {
            name: properties.display_name().unwrap_or_default(),
            class: match properties.get(PR_CONTAINER_CLASS) {
                Some(PropertyValue::Unicode(value)) => Some(value.to_string()),
                Some(PropertyValue::String8(value)) => Some(value.to_string()),
                _ => None,
            },
            item_count: count(properties.content_count()),
            unread_count: count(properties.unread_count()),
            associated_count: match properties.get(PR_ASSOC_CONTENT_COUNT) {
                Some(PropertyValue::Integer32(n)) => u64::try_from(*n).unwrap_or(0),
                _ => 0,
            },
            size: match properties.get(PR_MESSAGE_SIZE_EXTENDED) {
                Some(PropertyValue::Integer64(n)) => u64::try_from(*n).ok(),
                Some(PropertyValue::Integer32(n)) => u64::try_from(*n).ok(),
                _ => None,
            },
            children,
        })
    }

    fn format_error(&self, source: io::Error) -> Error {
        Error::Format {
            path: self.path.clone(),
            source,
        }
    }

    /// Every item of the IPM subtree, folder by folder, with the properties an
    /// audit needs. Folder paths start below the IPM subtree (`""` is the subtree
    /// itself). A message that cannot be read is reported in the second list and
    /// skipped; `on_folder` is called with each folder's path and item count.
    pub fn items(
        &self,
        on_folder: &mut dyn FnMut(&str, usize),
    ) -> Result<(Vec<ItemRecord>, Vec<String>)> {
        let entry_id = self
            .store
            .properties()
            .ipm_sub_tree_entry_id()
            .map_err(|source| self.format_error(source))?;
        let mut items = Vec::new();
        let mut errors = Vec::new();
        self.collect_items(&entry_id, String::new(), &mut items, &mut errors, on_folder)?;
        Ok((items, errors))
    }

    fn collect_items(
        &self,
        entry_id: &EntryId,
        path: String,
        items: &mut Vec<ItemRecord>,
        errors: &mut Vec<String>,
        on_folder: &mut dyn FnMut(&str, usize),
    ) -> Result<()> {
        let folder = self
            .store
            .open_folder(entry_id)
            .map_err(|source| self.format_error(source))?;

        if let Some(contents) = folder.contents_table() {
            let before = items.len();
            for row in contents.rows_matrix() {
                let node = NodeId::from(u32::from(row.id()));
                let key = format!("{:08x}", u32::from(node));
                let message = self
                    .store
                    .properties()
                    .make_entry_id(node)
                    .and_then(|id| self.store.open_message(&id, Some(&ITEM_PROPERTIES)));
                let properties = match message {
                    Ok(message) => Ok(message
                        .properties()
                        .iter()
                        .map(|(id, value)| (*id, value.clone()))
                        .collect()),
                    // outlook-pst refuses messages without a sub-node tree, which
                    // MS-PST allows (no recipient or attachment sub-nodes): many
                    // contacts and appointments. The folder's contents table holds
                    // their main properties, so the record is built from it.
                    Err(error) => row_properties(contents.as_ref(), row).map_err(|_| error),
                };
                match properties {
                    Ok(properties) => items.push(item_record(key, &path, &properties)),
                    Err(error) => errors.push(format!("{path}: message {key}: {error}")),
                }
            }
            on_folder(&path, items.len() - before);
        }

        let mut children = Vec::new();
        if let Some(hierarchy) = folder.hierarchy_table() {
            for row in hierarchy.rows_matrix() {
                let node = NodeId::from(u32::from(row.id()));
                let entry_id = self
                    .store
                    .properties()
                    .make_entry_id(node)
                    .map_err(|source| self.format_error(source))?;
                let name = self
                    .store
                    .open_folder(&entry_id)
                    .map_err(|source| self.format_error(source))?
                    .properties()
                    .display_name()
                    .unwrap_or_default();
                children.push((name, entry_id));
            }
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entry_id) in children {
            let child_path = ecm_model::path::join(&path, &name);
            self.collect_items(&entry_id, child_path, items, errors, on_folder)?;
        }
        Ok(())
    }
}

const PR_MESSAGE_CLASS: u16 = 0x001A;
const PR_SUBJECT: u16 = 0x0037;
const PR_CLIENT_SUBMIT_TIME: u16 = 0x0039;
const PR_TRANSPORT_MESSAGE_HEADERS: u16 = 0x007D;
const PR_SENDER_NAME: u16 = 0x0C1A;
const PR_SENDER_EMAIL_ADDRESS: u16 = 0x0C1F;
const PR_MESSAGE_DELIVERY_TIME: u16 = 0x0E06;
const PR_MESSAGE_FLAGS: u16 = 0x0E07;
const PR_MESSAGE_SIZE: u16 = 0x0E08;
const PR_HASATTACH: u16 = 0x0E1B;
const PR_INTERNET_MESSAGE_ID: u16 = 0x1035;
const PR_NATIVE_BODY_INFO: u16 = 0x1016;
const PR_BODY: u16 = 0x1000;
const PR_RTF_COMPRESSED: u16 = 0x1009;
const PR_HTML: u16 = 0x1013;
const PR_CREATION_TIME: u16 = 0x3007;
const PR_SENDER_SMTP_ADDRESS: u16 = 0x5D01;
/// `MSGFLAG_READ`.
const MSGFLAG_READ: i32 = 0x1;

/// The properties [`PstSource::items`] loads from each message, and no others.
const ITEM_PROPERTIES: [u16; 17] = [
    PR_MESSAGE_CLASS,
    PR_SUBJECT,
    PR_CLIENT_SUBMIT_TIME,
    PR_TRANSPORT_MESSAGE_HEADERS,
    PR_SENDER_NAME,
    PR_SENDER_EMAIL_ADDRESS,
    PR_MESSAGE_DELIVERY_TIME,
    PR_MESSAGE_FLAGS,
    PR_MESSAGE_SIZE,
    PR_HASATTACH,
    PR_INTERNET_MESSAGE_ID,
    PR_NATIVE_BODY_INFO,
    PR_BODY,
    PR_RTF_COMPRESSED,
    PR_HTML,
    PR_CREATION_TIME,
    PR_SENDER_SMTP_ADDRESS,
];

/// Every column of a contents table row, as properties.
fn row_properties(
    table: &dyn TableContext,
    row: &TableRowData,
) -> io::Result<BTreeMap<u16, PropertyValue>> {
    let context = table.context();
    let mut properties = BTreeMap::new();
    for (column, value) in context.columns().iter().zip(row.columns(context)?) {
        if let Some(value) = value {
            properties.insert(
                column.prop_id(),
                table.read_column(&value, column.prop_type())?,
            );
        }
    }
    Ok(properties)
}

fn item_record(key: String, folder: &str, properties: &BTreeMap<u16, PropertyValue>) -> ItemRecord {
    let text = |id| {
        match properties.get(&id) {
            Some(PropertyValue::Unicode(value)) => Some(value.to_string()),
            Some(PropertyValue::String8(value)) => Some(value.to_string()),
            _ => None,
        }
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    };
    let time = |id| match properties.get(&id) {
        Some(PropertyValue::Time(value)) => Some(filetime_to_unix(*value)),
        _ => None,
    };
    let integer = |id| match properties.get(&id) {
        Some(PropertyValue::Integer32(value)) => Some(i64::from(*value)),
        Some(PropertyValue::Integer64(value)) => Some(*value),
        _ => None,
    };
    let sender = text(PR_SENDER_SMTP_ADDRESS)
        .or_else(|| text(PR_SENDER_EMAIL_ADDRESS).filter(|address| address.contains('@')))
        .map(|address| address.to_lowercase())
        .or_else(|| text(PR_SENDER_NAME));

    ItemRecord {
        key,
        folder: folder.to_owned(),
        class: text(PR_MESSAGE_CLASS).unwrap_or_default(),
        message_id: text(PR_INTERNET_MESSAGE_ID),
        subject: text(PR_SUBJECT),
        sender,
        sent: time(PR_CLIENT_SUBMIT_TIME),
        received: time(PR_MESSAGE_DELIVERY_TIME),
        created: time(PR_CREATION_TIME),
        size: integer(PR_MESSAGE_SIZE).and_then(|n| u64::try_from(n).ok()),
        has_attachments: match properties.get(&PR_HASATTACH) {
            Some(PropertyValue::Boolean(value)) => Some(*value),
            _ => None,
        },
        read: integer(PR_MESSAGE_FLAGS).map(|flags| flags & i64::from(MSGFLAG_READ) != 0),
        has_headers: Some(text(PR_TRANSPORT_MESSAGE_HEADERS).is_some()),
        // PSTs rarely store PR_NATIVE_BODY_INFO: the richest body present tells.
        native_body: integer(PR_NATIVE_BODY_INFO)
            .and_then(|n| u8::try_from(n).ok())
            .or_else(|| {
                if properties.contains_key(&PR_HTML) {
                    Some(3)
                } else if properties.contains_key(&PR_RTF_COMPRESSED) {
                    Some(2)
                } else if properties.contains_key(&PR_BODY) {
                    Some(1)
                } else {
                    None
                }
            }),
    }
}

/// `NID_ROOT_FOLDER`, the root of every store's folder hierarchy.
const NID_ROOT_FOLDER: u32 = 0x122;

/// Result of [`check`]: what the PST contains, and proof it was not modified.
#[derive(Debug)]
pub struct CheckReport {
    /// The PST file.
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// Its on-disk format.
    pub format: PstFormat,
    /// Display name of the store.
    pub display_name: Option<String>,
    /// SHA-256 of the file, the same before and after the run.
    pub sha256: Sha256Digest,
    /// The folder tree.
    pub folders: FolderSummary,
}

/// Opens a PST file, reads its folder tree, and verifies the file is unchanged:
/// its SHA-256 is computed before opening it and again after closing it.
///
/// With `all_folders`, the tree starts at the store's root instead of the IPM
/// subtree.
pub fn check(path: impl AsRef<Path>, all_folders: bool) -> Result<CheckReport> {
    let path = path.as_ref();
    let before = sha256_file(path)?;

    let (format, display_name, folders) = {
        let source = PstSource::open(path)?;
        let folders = if all_folders {
            source.all_folders()?
        } else {
            source.ipm_folders()?
        };
        (source.format(), source.display_name(), folders)
    };

    let after = sha256_file(path)?;
    if before != after {
        return Err(Error::Modified {
            path: path.to_owned(),
            before,
            after,
        });
    }

    let size = std::fs::metadata(path)
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?
        .len();

    Ok(CheckReport {
        path: path.to_owned(),
        size,
        format,
        display_name,
        sha256: after,
        folders,
    })
}
