//! Full-fidelity backup of a mailbox with `ExportItems`.
//!
//! Layout of a backup directory:
//!
//! - `folders.json`: the folder tree, with EWS ids and paths, rewritten each run.
//! - `items/0000001.bin`…: one file per item, the bytes `ExportItems` returned.
//! - `manifest.jsonl`: one JSON line per saved item, appended once its file is
//!   written. An item is in the backup if and only if it has a manifest line.
//! - `errors.jsonl`: items the server could not export, appended per run. They have
//!   no manifest line, so the next run tries them again.
//!
//! Running the backup again into the same directory resumes it: items already in
//! the manifest are skipped. Each `.bin` file can be restored with `UploadItems`.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Error, EwsClient, Result, Transport,
    folders::{EwsFolder, FolderKind},
    items::{Export, ItemRef},
};

/// What to back up and how.
#[derive(Debug, Clone)]
pub struct BackupOptions {
    /// Backup directory, created if needed.
    pub out: PathBuf,
    /// Distinguished folder the backup starts at, `msgfolderroot` by default:
    /// every folder a client shows, and the items stored at that root.
    pub root: String,
    /// Most items per `ExportItems` request.
    pub max_batch_items: usize,
    /// Most bytes (by item size) per `ExportItems` request. A larger item goes
    /// alone.
    pub max_batch_bytes: u64,
}

impl BackupOptions {
    /// Default options for a backup into `out`.
    pub fn new(out: impl Into<PathBuf>) -> Self {
        Self {
            out: out.into(),
            root: "msgfolderroot".into(),
            max_batch_items: 50,
            max_batch_bytes: 20_000_000,
        }
    }
}

/// Progress of a backup, reported as it goes.
#[derive(Debug, Clone)]
pub enum BackupEvent<'a> {
    /// The folder tree is read: this many folders, this many items in all.
    Started {
        /// Folders to back up.
        folders: usize,
        /// Items they hold, as the server counts them.
        items: u64,
        /// Bytes they hold, as the server counts them.
        bytes: u64,
        /// Items already in the backup from earlier runs.
        already_saved: usize,
    },
    /// A folder is being backed up.
    Folder {
        /// Its path.
        path: &'a str,
        /// Items listed in it.
        items: usize,
        /// Of which, already in the backup.
        already_saved: usize,
    },
    /// A batch of items is saved.
    Saved {
        /// Items saved by this run so far.
        items: usize,
        /// Bytes saved by this run so far.
        bytes: u64,
    },
    /// An item could not be exported.
    Failed {
        /// Folder path.
        path: &'a str,
        /// EWS response code.
        code: &'a str,
    },
}

/// Result of a backup run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupReport {
    /// Folders backed up.
    pub folders: usize,
    /// Items listed in them.
    pub listed: usize,
    /// Items already in the backup before this run.
    pub already_saved: usize,
    /// Items saved by this run.
    pub saved: usize,
    /// Bytes saved by this run.
    pub saved_bytes: u64,
    /// Items the server could not export.
    pub failed: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestLine {
    file: String,
    folder: String,
    folder_id: String,
    item_id: String,
    class: String,
    size: u64,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct ErrorLine<'a> {
    folder: &'a str,
    folder_id: &'a str,
    item_id: &'a str,
    class: &'a str,
    code: &'a str,
    message: &'a str,
}

#[derive(Debug, Serialize)]
struct FolderLine<'a> {
    path: &'a str,
    id: &'a str,
    parent_id: Option<&'a str>,
    name: &'a str,
    class: Option<&'a str>,
    total_count: u64,
    hidden: bool,
}

const README: &str = "\
This directory is a backup made by ecm (exchange-catastrophic-migration).

folders.json    the folder tree, with Exchange Web Services (EWS) ids and paths
items/*.bin     one file per item, exactly as EWS ExportItems returned it
manifest.jsonl  one line per item: its file, folder, EWS id, class, size, SHA-256
errors.jsonl    items the server could not export

An item is in the backup when manifest.jsonl lists it. Each .bin file can be
restored, with every property, by EWS UploadItems into a folder of a mailbox.
";

impl<T: Transport> EwsClient<T> {
    /// Backs up every folder from `options.root` down, resuming any earlier run in
    /// the same directory. Only reads the mailbox.
    pub fn backup(
        &self,
        options: &BackupOptions,
        on_event: &mut dyn FnMut(BackupEvent<'_>),
    ) -> Result<BackupReport> {
        let out = &options.out;
        let io_error = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::Io { path, source }
        };
        fs::create_dir_all(out.join("items")).map_err(io_error(&out.join("items")))?;
        fs::write(out.join("README.txt"), README).map_err(io_error(&out.join("README.txt")))?;

        let tree = self.folder_tree(&options.root)?;
        let folders: Vec<(String, &EwsFolder)> = tree
            .with_paths()
            .into_iter()
            .filter(|(_, folder)| folder.kind != FolderKind::Search)
            .collect();
        write_folders(&out.join("folders.json"), &folders)?;

        let manifest_path = out.join("manifest.jsonl");
        let (saved_ids, mut next_sequence) = read_manifest(&manifest_path)?;
        let mut manifest = append(&manifest_path)?;
        let errors_path = out.join("errors.jsonl");
        let mut errors = append(&errors_path)?;

        let mut report = BackupReport {
            folders: folders.len(),
            already_saved: 0,
            ..Default::default()
        };
        on_event(BackupEvent::Started {
            folders: folders.len(),
            items: folders.iter().map(|(_, f)| f.total_count).sum(),
            bytes: folders.iter().filter_map(|(_, f)| f.size).sum(),
            already_saved: saved_ids.len(),
        });

        for (path, folder) in &folders {
            if folder.total_count == 0 {
                continue;
            }
            let items = self.list_items(&folder.id)?;
            let pending: Vec<&ItemRef> = items
                .iter()
                .filter(|item| !saved_ids.contains(item.id.as_str()))
                .collect();
            report.listed += items.len();
            report.already_saved += items.len() - pending.len();
            on_event(BackupEvent::Folder {
                path,
                items: items.len(),
                already_saved: items.len() - pending.len(),
            });

            for batch in batches(&pending, options.max_batch_items, options.max_batch_bytes) {
                let ids: Vec<&str> = batch.iter().map(|item| item.id.as_str()).collect();
                let exports = self.export_items(&ids)?;
                for (item, export) in batch.iter().zip(exports) {
                    match export {
                        Export::Data(data) => {
                            let file = format!("items/{next_sequence:07}.bin");
                            let file_path = out.join(&file);
                            fs::write(&file_path, &data).map_err(io_error(&file_path))?;
                            let line = ManifestLine {
                                file,
                                folder: path.clone(),
                                folder_id: folder.id.clone(),
                                item_id: item.id.clone(),
                                class: item.class.clone(),
                                size: item.size,
                                bytes: data.len() as u64,
                                sha256: hex(&Sha256::digest(&data)),
                            };
                            write_line(&mut manifest, &manifest_path, &line)?;
                            next_sequence += 1;
                            report.saved += 1;
                            report.saved_bytes += data.len() as u64;
                        }
                        Export::Failed { code, message } => {
                            let line = ErrorLine {
                                folder: path,
                                folder_id: &folder.id,
                                item_id: &item.id,
                                class: &item.class,
                                code: &code,
                                message: &message,
                            };
                            write_line(&mut errors, &errors_path, &line)?;
                            report.failed += 1;
                            on_event(BackupEvent::Failed { path, code: &code });
                        }
                    }
                }
                manifest.flush().map_err(io_error(&manifest_path))?;
                on_event(BackupEvent::Saved {
                    items: report.saved,
                    bytes: report.saved_bytes,
                });
            }
        }
        Ok(report)
    }
}

/// Splits items into export batches of at most `max_items` items and, unless a
/// single item is larger, `max_bytes` bytes.
fn batches<'a>(items: &[&'a ItemRef], max_items: usize, max_bytes: u64) -> Vec<Vec<&'a ItemRef>> {
    let mut batches = Vec::new();
    let mut current: Vec<&ItemRef> = Vec::new();
    let mut current_bytes = 0;
    for &item in items {
        if !current.is_empty()
            && (current.len() >= max_items.max(1) || current_bytes + item.size > max_bytes)
        {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes += item.size;
        current.push(item);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Item ids already saved, and the next file sequence number.
fn read_manifest(path: &Path) -> Result<(HashSet<String>, u64)> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((HashSet::new(), 1));
        }
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut ids = HashSet::new();
    let mut next = 1;
    for (number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: ManifestLine = serde_json::from_str(&line).map_err(|error| Error::Io {
            path: path.to_owned(),
            source: std::io::Error::other(format!("line {}: {error}", number + 1)),
        })?;
        let sequence = entry
            .file
            .strip_prefix("items/")
            .and_then(|name| name.strip_suffix(".bin"))
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(0);
        next = next.max(sequence + 1);
        ids.insert(entry.item_id);
    }
    Ok((ids, next))
}

fn append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })
}

fn write_line(file: &mut File, path: &Path, line: &impl Serialize) -> Result<()> {
    let mut text = serde_json::to_string(line).map_err(|error| Error::Io {
        path: path.to_owned(),
        source: std::io::Error::other(error),
    })?;
    text.push('\n');
    file.write_all(text.as_bytes()).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

fn write_folders(path: &Path, folders: &[(String, &EwsFolder)]) -> Result<()> {
    let lines: Vec<FolderLine<'_>> = folders
        .iter()
        .map(|(path, folder)| FolderLine {
            path,
            id: &folder.id,
            parent_id: folder.parent_id.as_deref(),
            name: &folder.name,
            class: folder.class.as_deref(),
            total_count: folder.total_count,
            hidden: folder.hidden,
        })
        .collect();
    let json = serde_json::to_string_pretty(&lines).map_err(|error| Error::Io {
        path: path.to_owned(),
        source: std::io::Error::other(error),
    })?;
    fs::write(path, json).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{BackupOptions, batches};
    use crate::{
        EwsClient,
        items::ItemRef,
        testing::{FakeTransport, soap},
    };

    fn item(id: &str, size: u64) -> ItemRef {
        ItemRef {
            id: id.into(),
            class: "IPM.Note".into(),
            size,
        }
    }

    #[test]
    fn batches_respect_item_and_byte_limits() {
        let items = [
            item("a", 10),
            item("b", 10),
            item("c", 10),
            item("big", 100),
            item("d", 1),
        ];
        let refs: Vec<_> = items.iter().collect();
        let ids = |batches: Vec<Vec<&ItemRef>>| -> Vec<Vec<String>> {
            batches
                .into_iter()
                .map(|b| b.iter().map(|i| i.id.clone()).collect())
                .collect()
        };
        assert_eq!(
            ids(batches(&refs, 2, 1000)),
            [vec!["a", "b"], vec!["c", "big"], vec!["d"]]
        );
        assert_eq!(
            ids(batches(&refs, 10, 25)),
            [vec!["a", "b"], vec!["c"], vec!["big"], vec!["d"]]
        );
    }

    const GET_ROOT: &str = r#"<m:GetFolderResponse><m:ResponseMessages><m:GetFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:Folders><t:Folder><t:FolderId Id="ROOT"/><t:DisplayName>Top</t:DisplayName><t:TotalCount>2</t:TotalCount></t:Folder></m:Folders></m:GetFolderResponseMessage></m:ResponseMessages></m:GetFolderResponse>"#;
    const FIND_FOLDERS: &str = r#"<m:FindFolderResponse><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder IncludesLastItemInRange="true"><t:Folders><t:SearchFolder><t:FolderId Id="SEARCH"/><t:ParentFolderId Id="ROOT"/><t:DisplayName>Saved search</t:DisplayName><t:TotalCount>9</t:TotalCount></t:SearchFolder></t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"#;
    const FIND_ITEMS: &str = r#"<m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:RootFolder IncludesLastItemInRange="true"><t:Items><t:Message><t:ItemId Id="A"/><t:ItemClass>IPM.Note</t:ItemClass><t:Size>4</t:Size></t:Message><t:Message><t:ItemId Id="B"/><t:ItemClass>IPM.Note</t:ItemClass><t:Size>4</t:Size></t:Message></t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"#;

    fn export(messages: &str) -> String {
        soap(&format!(
            "<m:ExportItemsResponse><m:ResponseMessages>{messages}</m:ResponseMessages></m:ExportItemsResponse>"
        ))
    }
    const EXPORTED_A: &str = r#"<m:ExportItemsResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:ItemId Id="A"/><m:Data>AQIDBA==</m:Data></m:ExportItemsResponseMessage>"#;
    const EXPORTED_B: &str = r#"<m:ExportItemsResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:ItemId Id="B"/><m:Data>BQYHCA==</m:Data></m:ExportItemsResponseMessage>"#;
    const FAILED_B: &str = r#"<m:ExportItemsResponseMessage ResponseClass="Error"><m:MessageText>Internal error.</m:MessageText><m:ResponseCode>ErrorInternalServerError</m:ResponseCode></m:ExportItemsResponseMessage>"#;

    #[test]
    fn a_second_run_resumes_and_retries_failed_items() {
        let dir = tempfile::tempdir().unwrap();
        let options = BackupOptions::new(dir.path());

        // First run: A is saved, B fails. The search folder is skipped.
        let first = FakeTransport::new([
            (200, soap(GET_ROOT)),
            (200, soap(FIND_FOLDERS)),
            (200, soap(FIND_ITEMS)),
            (200, export(&format!("{EXPORTED_A}{FAILED_B}"))),
        ]);
        let report = EwsClient::new(&first)
            .backup(&options, &mut |_| {})
            .unwrap();
        assert_eq!((report.folders, report.saved, report.failed), (1, 1, 1));
        assert_eq!(
            fs::read(dir.path().join("items/0000001.bin")).unwrap(),
            [1, 2, 3, 4]
        );

        // Second run: only B is exported, into the next file.
        let second = FakeTransport::new([
            (200, soap(GET_ROOT)),
            (200, soap(FIND_FOLDERS)),
            (200, soap(FIND_ITEMS)),
            (200, export(EXPORTED_B)),
        ]);
        let report = EwsClient::new(&second)
            .backup(&options, &mut |_| {})
            .unwrap();
        assert_eq!(
            (report.already_saved, report.saved, report.failed),
            (1, 1, 0)
        );
        assert!(second.requests.borrow()[3].contains(r#"<t:ItemId Id="B"/></m:ItemIds>"#));
        assert!(!second.requests.borrow()[3].contains(r#"Id="A""#));
        assert_eq!(
            fs::read(dir.path().join("items/0000002.bin")).unwrap(),
            [5, 6, 7, 8]
        );

        let manifest = fs::read_to_string(dir.path().join("manifest.jsonl")).unwrap();
        assert_eq!(manifest.lines().count(), 2);
        assert!(manifest.contains(r#""item_id":"B""#));
        assert!(
            fs::read_to_string(dir.path().join("errors.jsonl"))
                .unwrap()
                .contains("ErrorInternalServerError")
        );
        assert!(
            fs::read_to_string(dir.path().join("folders.json"))
                .unwrap()
                .contains(r#""id": "ROOT""#)
        );
    }
}
