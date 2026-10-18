//! `ecm plan` and `ecm apply`.

use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use ecm_audit::plan::{Action, FolderMap, WELL_KNOWN, counts, plan};
use ecm_ews::write::ExtendedValue;
use ecm_model::{
    item::{format_iso, format_utc, parse_utc},
    path,
};
use ecm_store::{SOURCE_INVENTORY, TARGET_INVENTORY, read_inventory};
use serde::{Deserialize, Serialize};

use crate::{
    EwsArgs,
    audit_cmd::{GraphArgs, OUTLOOK_INVENTORY, SourceKind},
    connect,
};

/// Writes a plan from the inventories in `work` and the target's folders.
pub fn plan_command(
    work: &Path,
    kind: SourceKind,
    cutoff: &str,
    ews: &EwsArgs,
    out: &Path,
) -> Result<(), String> {
    let cutoff = parse_utc(&format!("{cutoff}T00:00:00Z"))
        .ok_or_else(|| format!("invalid --cutoff {cutoff:?}: expected YYYY-MM-DD"))?;
    let source_file = match kind {
        SourceKind::Pst => SOURCE_INVENTORY,
        SourceKind::Outlook => OUTLOOK_INVENTORY,
    };
    let source = read_inventory(&work.join(source_file)).map_err(|e| e.to_string())?;
    let target = read_inventory(&work.join(TARGET_INVENTORY)).map_err(|e| e.to_string())?;
    let audit = ecm_audit::audit(&source, &target, cutoff);

    let client = connect(ews)?;
    let tree = client
        .folder_tree("msgfolderroot")
        .map_err(|e| e.to_string())?;
    let paths: HashMap<String, String> = tree
        .with_paths()
        .into_iter()
        .map(|(path, folder)| (folder.id.clone(), path))
        .collect();
    let existing: BTreeSet<String> = paths.values().cloned().collect();
    let mut folders = FolderMap::default();
    for (id, _) in WELL_KNOWN {
        if let Ok(folder) = client.get_distinguished_folder(id)
            && let Some(path) = paths.get(&folder.id)
        {
            folders.well_known.insert((*id).to_owned(), path.clone());
        }
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let run = format_utc(now).replace([' ', ':'], "-");
    let actions = plan(&audit, &source, &target, &folders, &existing, &run);

    let mut text = String::new();
    for action in &actions {
        text.push_str(&serde_json::to_string(action).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;

    println!("Plan {run}: {} steps into {}", actions.len(), out.display());
    for (kind, n) in counts(&actions) {
        println!("  {kind:<14} {n:>7}");
    }
    println!();
    println!(
        "Nothing was changed. Review the plan, then run `ecm apply --plan {} --dry-run`.",
        out.display()
    );
    Ok(())
}

/// One line of an apply journal.
#[derive(Debug, Serialize, Deserialize)]
struct JournalLine {
    step: usize,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn journal_path(plan: &Path) -> PathBuf {
    let mut name = plan.file_name().unwrap_or_default().to_os_string();
    name.push(".journal.jsonl");
    plan.with_file_name(name)
}

fn read_plan(plan: &Path) -> Result<Vec<Action>, String> {
    fs::read_to_string(plan)
        .map_err(|e| format!("{}: {e}", plan.display()))?
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| {
            serde_json::from_str(line)
                .map_err(|e| format!("{}, line {}: {e}", plan.display(), n + 1))
        })
        .collect()
}

/// Steps already done, according to the journal.
fn read_journal(path: &Path) -> Result<HashMap<usize, JournalLine>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut done = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let entry: JournalLine =
            serde_json::from_str(line).map_err(|e| format!("{}: {e}", path.display()))?;
        if entry.status == "done" {
            done.insert(entry.step, entry);
        } else {
            done.remove(&entry.step);
        }
    }
    Ok(done)
}

/// Runs a plan, or with `dry_run` only says what is left to do.
pub fn apply_command(
    plan_path: &Path,
    ews: &EwsArgs,
    graph: Option<&GraphArgs>,
    limit: Option<usize>,
    dry_run: bool,
) -> Result<(), String> {
    let actions = read_plan(plan_path)?;
    let journal = journal_path(plan_path);
    let done = read_journal(&journal)?;
    let pending: Vec<usize> = (0..actions.len())
        .filter(|i| !done.contains_key(i))
        .collect();
    let selected: Vec<usize> = pending
        .iter()
        .copied()
        .take(limit.unwrap_or(usize::MAX))
        .collect();

    let summary = |indices: &[usize]| {
        let chosen: Vec<Action> = indices.iter().map(|&i| actions[i].clone()).collect();
        counts(&chosen)
    };
    println!(
        "Plan: {} steps, {} done, {} left.",
        actions.len(),
        done.len(),
        pending.len()
    );
    println!("This run: {} steps", selected.len());
    for (kind, n) in summary(&selected) {
        println!("  {kind:<14} {n:>7}");
    }
    if dry_run {
        println!("Dry run: nothing was changed.");
        return Ok(());
    }
    if selected.is_empty() {
        return Ok(());
    }

    let client = connect(ews)?;
    let mut runner = Runner::new(&client, graph, &journal, done)?;
    let mut index = 0;
    while index < selected.len() {
        let step = selected[index];
        match &actions[step] {
            Action::Move { to, .. } => {
                // Consecutive moves to the same folder go in one request.
                let mut batch = vec![step];
                while batch.len() < 50
                    && index + batch.len() < selected.len()
                    && matches!(&actions[selected[index + batch.len()]], Action::Move { to: next, .. } if next == to)
                {
                    batch.push(selected[index + batch.len()]);
                }
                index += batch.len();
                runner.moves(&actions, &batch)?;
            }
            action => {
                index += 1;
                runner.single(step, action)?;
            }
        }
        if runner.executed % 200 < 50 || index == selected.len() {
            eprint!(
                "\r\x1b[K  {}/{} steps, {} failed, {} waiting for a failed step",
                runner.executed,
                selected.len(),
                runner.failed,
                runner.skipped
            );
        }
    }
    eprintln!();
    println!(
        "{} steps done, {} failed, {} skipped because a step they need failed. Journal: {}",
        runner.executed - runner.failed - runner.skipped,
        runner.failed,
        runner.skipped,
        journal.display()
    );
    if runner.failed + runner.skipped > 0 {
        println!("Run the same command again to retry them.");
    }
    Ok(())
}

/// Executes steps and journals them.
struct Runner<'a, T: ecm_ews::Transport> {
    client: &'a ecm_ews::EwsClient<T>,
    graph_args: Option<&'a GraphArgs>,
    #[cfg(windows)]
    graph: Option<ecm_graph::GraphClient<ecm_graph::WinHttpTransport>>,
    journal: fs::File,
    journal_path: PathBuf,
    done: HashMap<usize, JournalLine>,
    folders: HashMap<String, String>,
    executed: usize,
    failed: usize,
    skipped: usize,
}

impl<'a, T: ecm_ews::Transport> Runner<'a, T> {
    fn new(
        client: &'a ecm_ews::EwsClient<T>,
        graph_args: Option<&'a GraphArgs>,
        journal_path: &Path,
        done: HashMap<usize, JournalLine>,
    ) -> Result<Self, String> {
        let tree = client
            .folder_tree("msgfolderroot")
            .map_err(|e| e.to_string())?;
        let mut folders: HashMap<String, String> = tree
            .with_paths()
            .into_iter()
            .map(|(path, folder)| (path, folder.id.clone()))
            .collect();
        folders.insert(String::new(), tree.root.id.clone());
        let journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal_path)
            .map_err(|e| format!("{}: {e}", journal_path.display()))?;
        Ok(Self {
            client,
            graph_args,
            #[cfg(windows)]
            graph: None,
            journal,
            journal_path: journal_path.to_owned(),
            done,
            folders,
            executed: 0,
            failed: 0,
            skipped: 0,
        })
    }

    fn record(&mut self, step: usize, result: Result<String, String>) -> Result<(), String> {
        self.executed += 1;
        let line = match result {
            Ok(id) => JournalLine {
                step,
                status: "done".into(),
                id: Some(id),
                error: None,
            },
            Err(error) if error == "skipped" => {
                self.skipped += 1;
                JournalLine {
                    step,
                    status: "skipped".into(),
                    id: None,
                    error: None,
                }
            }
            Err(error) => {
                self.failed += 1;
                JournalLine {
                    step,
                    status: "failed".into(),
                    id: None,
                    error: Some(error),
                }
            }
        };
        let mut text = serde_json::to_string(&line).map_err(|e| e.to_string())?;
        text.push('\n');
        self.journal
            .write_all(text.as_bytes())
            .and_then(|()| self.journal.flush())
            .map_err(|e| format!("{}: {e}", self.journal_path.display()))?;
        if line.status == "done" {
            self.done.insert(step, line);
        }
        Ok(())
    }

    fn folder_id(&self, path: &str) -> Result<String, String> {
        self.folders.get(path).cloned().ok_or_else(|| {
            format!("folder {path:?} does not exist (its create-folder step has not run)")
        })
    }

    fn single(&mut self, step: usize, action: &Action) -> Result<(), String> {
        let result = match action {
            Action::CreateFolder { path: folder } => {
                if let Some(id) = self.folders.get(folder) {
                    Ok(id.clone())
                } else {
                    let parent = path::parent(folder).unwrap_or_default();
                    let name = path::split(folder).pop().unwrap_or_default();
                    self.folder_id(&parent)
                        .and_then(|parent_id| {
                            self.client
                                .create_folder(&parent_id, &name)
                                .map_err(|e| e.to_string())
                        })
                        .inspect(|id| {
                            self.folders.insert(folder.clone(), id.clone());
                        })
                }
            }
            Action::Recreate {
                source,
                to,
                read,
                sent,
                received,
                ..
            } => self
                .folder_id(to)
                .and_then(|folder| self.recreate(&folder, source, *read, *sent, *received)),
            Action::Move { .. } => unreachable!("moves are batched"),
        };
        self.record(step, result)
    }

    fn moves(&mut self, actions: &[Action], batch: &[usize]) -> Result<(), String> {
        let Action::Move { to, .. } = &actions[batch[0]] else {
            unreachable!()
        };
        let mut ready = Vec::new();
        for &step in batch {
            let Action::Move { requires, .. } = &actions[step] else {
                unreachable!()
            };
            if requires.is_some_and(|r| !self.done.contains_key(&r)) {
                self.record(step, Err("skipped".into()))?;
            } else {
                ready.push(step);
            }
        }
        if ready.is_empty() {
            return Ok(());
        }
        let folder = match self.folder_id(to) {
            Ok(folder) => folder,
            Err(error) => {
                for step in ready {
                    self.record(step, Err(error.clone()))?;
                }
                return Ok(());
            }
        };
        let ids: Vec<&str> = ready
            .iter()
            .map(|&step| match &actions[step] {
                Action::Move { item, .. } => item.as_str(),
                _ => unreachable!(),
            })
            .collect();
        match self.client.move_items(&ids, &folder) {
            Ok(results) => {
                for (step, result) in ready.into_iter().zip(results) {
                    self.record(step, result.map_err(|e| e.to_string()))?;
                }
            }
            Err(error) => {
                for step in ready {
                    self.record(step, Err(error.to_string()))?;
                }
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    fn recreate(
        &mut self,
        folder: &str,
        source: &str,
        read: Option<bool>,
        sent: Option<i64>,
        received: Option<i64>,
    ) -> Result<String, String> {
        let mime = match self.graph()?.mime(source) {
            Err(ecm_graph::Error::Graph { status: 401, .. }) => {
                // The token expired during a long run: sign in again once.
                self.graph = None;
                self.graph()?.mime(source)
            }
            other => other,
        }
        .map_err(|e| e.to_string())?;
        let mut extended = vec![ExtendedValue {
            tag: 0x0E07,
            kind: "Integer",
            value: if read == Some(true) { "1" } else { "0" }.into(),
        }];
        if let Some(received) = received {
            extended.push(ExtendedValue {
                tag: 0x0E06,
                kind: "SystemTime",
                value: format_iso(received),
            });
        }
        if let Some(sent) = sent {
            extended.push(ExtendedValue {
                tag: 0x0039,
                kind: "SystemTime",
                value: format_iso(sent),
            });
        }
        self.client
            .create_from_mime(folder, &mime, &extended)
            .map_err(|e| e.to_string())
    }

    #[cfg(windows)]
    fn graph(&mut self) -> Result<&ecm_graph::GraphClient<ecm_graph::WinHttpTransport>, String> {
        if self.graph.is_none() {
            let args = self.graph_args.ok_or(
                "this plan re-creates items from Outlook.com: add --outlook-com --client-id <id>",
            )?;
            let transport = ecm_graph::WinHttpTransport::default();
            let token = ecm_graph::auth::sign_in(
                &transport,
                &args.client_id,
                &args.tenant,
                &mut |prompt| {
                    eprintln!();
                    eprintln!(
                        "Sign in to Outlook.com: open {} and enter the code {}",
                        prompt.verification_uri, prompt.user_code
                    );
                    eprintln!("(Waiting for you to sign in…)");
                },
            )
            .map_err(|e| e.to_string())?;
            self.graph = Some(ecm_graph::GraphClient::new(transport, token));
        }
        Ok(self.graph.as_ref().expect("just set"))
    }

    #[cfg(not(windows))]
    fn recreate(
        &mut self,
        _: &str,
        _: &str,
        _: Option<bool>,
        _: Option<i64>,
        _: Option<i64>,
    ) -> Result<String, String> {
        let _ = self.graph_args;
        Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{journal_path, read_journal};

    #[test]
    fn the_journal_sits_next_to_the_plan() {
        assert_eq!(
            journal_path(Path::new("reports/plan.jsonl")),
            Path::new("reports/plan.jsonl.journal.jsonl")
        );
    }

    #[test]
    fn done_steps_stay_done_and_failed_ones_are_retried() {
        let dir = std::env::temp_dir().join(format!("ecm-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("j.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"step":0,"status":"done","id":"F"}"#,
                "\n",
                r#"{"step":1,"status":"failed","error":"busy"}"#,
                "\n",
                r#"{"step":2,"status":"failed","error":"busy"}"#,
                "\n",
                r#"{"step":2,"status":"done","id":"X"}"#,
                "\n",
                r#"{"step":3,"status":"skipped"}"#,
                "\n",
            ),
        )
        .unwrap();
        let done = read_journal(&path).unwrap();
        let mut steps: Vec<_> = done.keys().copied().collect();
        steps.sort();
        assert_eq!(steps, [0, 2]);
        assert!(read_journal(&dir.join("missing.jsonl")).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
