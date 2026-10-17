//! Local state: inventories of the source and target, and the files later steps
//! read and write.
//!
//! Inventories are JSON Lines files, one [`ItemRecord`] per line: small enough to
//! load in memory, readable with any tool, and appendable. They hold mailbox
//! metadata (subjects, addresses), so they belong in a private directory such as
//! the gitignored `reports/`.

#![forbid(unsafe_code)]

use std::{
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use ecm_model::ItemRecord;

/// Errors from reading or writing local state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A line of an inventory is not a valid record.
    #[error("{path}, line {line}: {message}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
}

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// File name of the source inventory in a work directory.
pub const SOURCE_INVENTORY: &str = "source.jsonl";
/// File name of the target inventory in a work directory.
pub const TARGET_INVENTORY: &str = "target.jsonl";

/// Writes an inventory, replacing the file.
pub fn write_inventory(path: &Path, items: &[ItemRecord]) -> Result<()> {
    let io_error = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    let mut out = BufWriter::new(File::create(path).map_err(io_error)?);
    for item in items {
        serde_json::to_writer(&mut out, item).map_err(|e| io_error(io::Error::other(e)))?;
        out.write_all(b"\n").map_err(io_error)?;
    }
    out.flush().map_err(io_error)
}

/// Reads an inventory written by [`write_inventory`].
pub fn read_inventory(path: &Path) -> Result<Vec<ItemRecord>> {
    let file = File::open(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut items = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        items.push(serde_json::from_str(&line).map_err(|e| Error::Invalid {
            path: path.to_owned(),
            line: index + 1,
            message: e.to_string(),
        })?);
    }
    Ok(items)
}

/// Writes rows as CSV (RFC 4180), quoting every field that needs it. A UTF-8 BOM
/// comes first so that Excel reads accents correctly.
pub fn write_csv(path: &Path, header: &[&str], rows: &[Vec<String>]) -> Result<()> {
    let io_error = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    let mut out = BufWriter::new(File::create(path).map_err(io_error)?);
    out.write_all("\u{feff}".as_bytes()).map_err(io_error)?;
    let line = |fields: &mut dyn Iterator<Item = &str>| {
        let mut text = fields.map(csv_field).collect::<Vec<_>>().join(",");
        text.push_str("\r\n");
        text
    };
    out.write_all(line(&mut header.iter().copied()).as_bytes())
        .map_err(io_error)?;
    for row in rows {
        out.write_all(line(&mut row.iter().map(String::as_str)).as_bytes())
            .map_err(io_error)?;
    }
    out.flush().map_err(io_error)
}

fn csv_field(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventories_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(SOURCE_INVENTORY);
        let items = vec![
            ItemRecord {
                key: "1".into(),
                folder: "Inbox".into(),
                class: "IPM.Note".into(),
                subject: Some("Héllo, \"world\"".into()),
                sent: Some(1),
                ..Default::default()
            },
            ItemRecord::default(),
        ];
        write_inventory(&path, &items).unwrap();
        assert_eq!(read_inventory(&path).unwrap(), items);
    }

    #[test]
    fn a_bad_line_names_its_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        std::fs::write(
            &path,
            "{\"key\":\"1\",\"folder\":\"\",\"class\":\"\"}\nnot json\n",
        )
        .unwrap();
        assert!(matches!(
            read_inventory(&path),
            Err(Error::Invalid { line: 2, .. })
        ));
    }

    #[test]
    fn csv_quotes_only_what_needs_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        write_csv(
            &path,
            &["a", "b"],
            &[vec!["x, y".into(), "say \"hi\"".into()]],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "\u{feff}a,b\r\n\"x, y\",\"say \"\"hi\"\"\"\r\n");
    }
}
