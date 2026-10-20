//! Plans for an Outlook.com mailbox through Microsoft Graph: the folder map they
//! are written with, and `ecm apply-graph`, which runs them, journaled and
//! resumable like `ecm apply`.

use std::collections::{BTreeSet, HashMap};

use ecm_audit::plan::{Action, FolderMap, WELL_KNOWN, counts};

use crate::{
    audit_cmd::GraphArgs,
    plan_cmd::{journal_path, read_journal, read_plan},
};

/// The folders of a Graph mailbox.
pub struct GraphFolders {
    /// Where well-known source folders go.
    pub map: FolderMap,
    /// Every folder path.
    pub existing: BTreeSet<String>,
    /// Each path's folder id.
    pub ids: HashMap<String, String>,
}

/// The folder map of a Graph mailbox, its folder paths, and each path's id.
pub fn graph_folder_map<T: ecm_graph::Transport>(
    client: &ecm_graph::GraphClient<T>,
) -> Result<GraphFolders, String> {
    let tree = client.mail_folders().map_err(|e| e.to_string())?;
    let ids: HashMap<String, String> = tree
        .flatten()
        .into_iter()
        .filter(|folder| !folder.path.is_empty())
        .map(|folder| (folder.path.clone(), folder.id.clone()))
        .collect();
    let existing = ids.keys().cloned().collect();
    let mut folders = FolderMap::default();
    for (name, _) in WELL_KNOWN {
        let Some(id) = client.well_known_folder(name).map_err(|e| e.to_string())? else {
            continue;
        };
        if let Some((path, _)) = ids.iter().find(|(_, folder)| **folder == id) {
            folders.well_known.insert((*name).to_owned(), path.clone());
        }
    }
    Ok(GraphFolders {
        map: folders,
        existing,
        ids,
    })
}

/// The steps a Graph mailbox can run.
fn for_graph(action: &Action) -> bool {
    matches!(
        action,
        Action::CreateFolder { .. }
            | Action::CreateMessage { .. }
            | Action::Attach { .. }
            | Action::Move { .. }
            | Action::CreateGraph { .. }
            | Action::SetPhoto { .. }
    )
}

/// Whether a step writes other item types than messages: the run then asks for
/// their permissions.
fn needs_pim_scopes(action: &Action) -> bool {
    matches!(action, Action::CreateGraph { .. } | Action::SetPhoto { .. })
}

/// One plan of a run: its steps, journal, done and selected steps.
struct PlanRun {
    actions: Vec<Action>,
    journal: std::path::PathBuf,
    done: HashMap<usize, crate::plan_cmd::JournalLine>,
    selected: Vec<usize>,
}

/// Runs plans written for an Outlook.com mailbox, one after the other with one
/// sign-in, or with `dry_run` only says what is left to do. `limit` counts steps
/// over all of them.
pub fn apply_graph_command(
    plan_paths: &[std::path::PathBuf],
    graph: &GraphArgs,
    limit: Option<usize>,
    dry_run: bool,
) -> Result<(), String> {
    let mut budget = limit.unwrap_or(usize::MAX);
    let mut runs = Vec::new();
    for plan_path in plan_paths {
        let actions = read_plan(plan_path)?;
        if let Some((step, action)) = actions.iter().enumerate().find(|(_, a)| !for_graph(a)) {
            return Err(format!(
                "{}: step {step} ({}) is not for Outlook.com: this plan is for `ecm apply`",
                plan_path.display(),
                action.kind()
            ));
        }
        let journal = journal_path(plan_path);
        let done = read_journal(&journal)?;
        let pending: Vec<usize> = (0..actions.len())
            .filter(|i| !done.contains_key(i))
            .collect();
        let selected: Vec<usize> = pending.iter().copied().take(budget).collect();
        budget -= selected.len();
        println!(
            "{}: {} steps, {} done, {} left.",
            plan_path.display(),
            actions.len(),
            done.len(),
            pending.len()
        );
        println!("This run: {} steps", selected.len());
        let chosen: Vec<Action> = selected.iter().map(|&i| actions[i].clone()).collect();
        for (kind, n) in counts(&chosen) {
            println!("  {kind:<14} {n:>7}");
        }
        runs.push(PlanRun {
            actions,
            journal,
            done,
            selected,
        });
    }
    if dry_run {
        println!("Dry run: nothing was changed.");
        return Ok(());
    }
    if runs.iter().all(|run| run.selected.is_empty()) {
        return Ok(());
    }
    let scopes = if runs.iter().any(|run| {
        run.selected
            .iter()
            .any(|&i| needs_pim_scopes(&run.actions[i]))
    }) {
        ecm_graph::auth::PIM_WRITE_SCOPES
    } else {
        ecm_graph::auth::WRITE_SCOPES
    };
    run(runs, graph, scopes)
}

#[cfg(windows)]
fn run(runs: Vec<PlanRun>, graph: &GraphArgs, scopes: &'static str) -> Result<(), String> {
    let mut runner = runner::GraphRunner::new(graph, scopes)?;
    for plan in runs.into_iter().filter(|run| !run.selected.is_empty()) {
        runner.start(&plan.journal, plan.done)?;
        for (n, &step) in plan.selected.iter().enumerate() {
            runner.single(step, &plan.actions[step])?;
            if n % 20 == 0 || n + 1 == plan.selected.len() {
                eprint!(
                    "\r\x1b[K  {}/{} steps, {} failed, {} waiting for a failed step",
                    runner.executed,
                    plan.selected.len(),
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
            plan.journal.display()
        );
        if runner.failed + runner.skipped > 0 {
            println!("Run the same command again to retry them.");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn run(_: Vec<PlanRun>, _: &GraphArgs, _: &'static str) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

#[cfg(windows)]
mod runner {
    use std::{
        collections::HashMap,
        fs::{self, OpenOptions},
        io::Write as _,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use ecm_audit::plan::Action;
    use ecm_graph::{GraphClient, WinHttpTransport, auth};
    use ecm_model::path;
    use serde_json::Value;

    use crate::{audit_cmd::GraphArgs, plan_cmd::JournalLine, rules_cmd::FOLDER_PLACEHOLDER};

    /// Access tokens last about an hour: renewed before that.
    const RENEW_AFTER: Duration = Duration::from_secs(45 * 60);

    /// Executes steps against a Graph mailbox and journals them.
    pub struct GraphRunner<'a> {
        args: &'a GraphArgs,
        scopes: &'static str,
        client: GraphClient<WinHttpTransport>,
        refresh: Option<String>,
        signed_in: Instant,
        folders: HashMap<String, String>,
        /// Calendars, contact folders and To Do lists by kind and name: `None`
        /// for the default calendar and contact folder.
        containers: HashMap<(String, String), Option<String>>,
        journal: Option<fs::File>,
        journal_path: PathBuf,
        done: HashMap<usize, JournalLine>,
        pub executed: usize,
        pub failed: usize,
        pub skipped: usize,
    }

    impl<'a> GraphRunner<'a> {
        pub fn new(args: &'a GraphArgs, scopes: &'static str) -> Result<Self, String> {
            let transport = WinHttpTransport::default();
            let tokens = auth::sign_in_with_scopes(
                &transport,
                &args.client_id,
                &args.tenant,
                scopes,
                &mut |prompt| {
                    eprintln!();
                    eprintln!(
                        "Sign in to Outlook.com (to apply the plan): open {} and enter the code {}",
                        prompt.verification_uri, prompt.user_code
                    );
                    eprintln!("(Waiting for you to sign in…)");
                },
            )
            .map_err(|e| e.to_string())?;
            let client = GraphClient::new(transport, tokens.access);
            let folders = super::graph_folder_map(&client)?.ids;
            Ok(Self {
                args,
                scopes,
                client,
                refresh: tokens.refresh,
                signed_in: Instant::now(),
                folders,
                containers: HashMap::new(),
                journal: None,
                journal_path: PathBuf::new(),
                done: HashMap::new(),
                executed: 0,
                failed: 0,
                skipped: 0,
            })
        }

        /// Starts a plan: its journal and the steps it has done.
        pub fn start(
            &mut self,
            journal_path: &Path,
            done: HashMap<usize, JournalLine>,
        ) -> Result<(), String> {
            self.journal = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(journal_path)
                    .map_err(|e| format!("{}: {e}", journal_path.display()))?,
            );
            self.journal_path = journal_path.to_owned();
            self.done = done;
            self.executed = 0;
            self.failed = 0;
            self.skipped = 0;
            Ok(())
        }

        /// A new access token from the refresh token, without the user.
        fn renew(&mut self) -> Result<(), String> {
            let refresh = self
                .refresh
                .clone()
                .ok_or("the access token expired and there is no refresh token")?;
            let transport = WinHttpTransport::default();
            let tokens = auth::refresh_with(
                &transport,
                &self.args.client_id,
                &self.args.tenant,
                &refresh,
                self.scopes,
            )
            .map_err(|e| e.to_string())?;
            self.client = GraphClient::new(transport, tokens.access);
            self.refresh = tokens.refresh.or(Some(refresh));
            self.signed_in = Instant::now();
            Ok(())
        }

        /// Calls Graph, renewing the token when it is old or refused.
        fn call<R>(
            &mut self,
            f: impl Fn(&GraphClient<WinHttpTransport>) -> ecm_graph::Result<R>,
        ) -> Result<R, String> {
            if self.signed_in.elapsed() > RENEW_AFTER {
                self.renew()?;
            }
            match f(&self.client) {
                Err(ecm_graph::Error::Graph { status: 401, .. }) => {
                    self.renew()?;
                    f(&self.client).map_err(|e| e.to_string())
                }
                other => other.map_err(|e| e.to_string()),
            }
        }

        /// The calendar, contact folder or To Do list `name` (created when
        /// missing), or the default one when `name` is empty: `None` for the
        /// default calendar and contact folder.
        fn container(&mut self, kind: &str, name: &str) -> Result<Option<String>, String> {
            let key = (kind.to_owned(), name.to_owned());
            if let Some(id) = self.containers.get(&key) {
                return Ok(id.clone());
            }
            let id = match (kind, name.is_empty()) {
                ("task", true) => Some(
                    self.call(|client| client.default_todo_list())?
                        .ok_or("the mailbox has no default To Do list")?,
                ),
                (_, true) => None,
                ("event", false) => Some(self.call(|client| client.ensure_calendar(name))?),
                ("contact", false) => Some(self.call(|client| client.ensure_contact_folder(name))?),
                ("task", false) => Some(self.call(|client| client.ensure_todo_list(name))?),
                (other, _) => return Err(format!("{other} items have no container")),
            };
            self.containers.insert(key, id.clone());
            Ok(id)
        }

        /// Creates an item from its Graph JSON.
        fn create(&mut self, kind: &str, to: &str, item: &str) -> Result<String, String> {
            let mut item: Value =
                serde_json::from_str(item).map_err(|e| format!("item JSON: {e}"))?;
            match kind {
                "event" => {
                    let calendar = self.container(kind, to)?;
                    self.call(|client| client.create_event(calendar.as_deref(), &item))
                }
                "contact" => {
                    let folder = self.container(kind, to)?;
                    self.call(|client| client.create_contact(folder.as_deref(), &item))
                }
                "task" => {
                    let list = self.container(kind, to)?.unwrap_or_default();
                    self.call(|client| client.create_task(&list, &item))
                }
                "rule" => {
                    for key in ["moveToFolder", "copyToFolder"] {
                        let Some(path) = item["actions"][key]
                            .as_str()
                            .and_then(|v| v.strip_prefix(FOLDER_PLACEHOLDER))
                        else {
                            continue;
                        };
                        item["actions"][key] = Value::String(self.folder_id(path)?);
                    }
                    self.call(|client| client.create_inbox_rule(&item))
                }
                "category" => {
                    let name = item["displayName"].as_str().unwrap_or_default();
                    let color = item["color"].as_str().unwrap_or("none");
                    self.call(|client| client.create_category(name, color))
                }
                other => Err(format!("unknown item type {other:?}")),
            }
        }

        fn folder_id(&self, path: &str) -> Result<String, String> {
            self.folders.get(path).cloned().ok_or_else(|| {
                format!("folder {path:?} does not exist (its create-folder step has not run)")
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
            let journal = self.journal.as_mut().ok_or("no plan started")?;
            journal
                .write_all(text.as_bytes())
                .and_then(|()| journal.flush())
                .map_err(|e| format!("{}: {e}", self.journal_path.display()))?;
            if line.status == "done" {
                self.done.insert(step, line);
            }
            Ok(())
        }

        pub fn single(&mut self, step: usize, action: &Action) -> Result<(), String> {
            let result = match action {
                Action::CreateFolder { path: folder } => match self.folders.get(folder) {
                    Some(id) => Ok(id.clone()),
                    None => {
                        let parent = path::parent(folder).unwrap_or_default();
                        let name = path::split(folder).pop().unwrap_or_default();
                        let parent_id = if parent.is_empty() {
                            Ok(None)
                        } else {
                            self.folder_id(&parent).map(Some)
                        };
                        match parent_id {
                            Ok(parent_id) => self
                                .call(|client| client.ensure_folder(parent_id.as_deref(), &name))
                                .inspect(|id| {
                                    self.folders.insert(folder.clone(), id.clone());
                                }),
                            Err(error) => Err(error),
                        }
                    }
                },
                Action::CreateMessage { to, message, .. } => {
                    match (self.folder_id(to), serde_json::from_str(message)) {
                        (Ok(folder), Ok(message)) => {
                            self.call(|client| client.create_message(&folder, &message))
                        }
                        (Err(error), _) => Err(error),
                        (_, Err(error)) => Err(format!("message JSON: {error}")),
                    }
                }
                Action::Attach {
                    requires,
                    name,
                    content_type,
                    content_id,
                    data,
                } => match self.done.get(requires).and_then(|line| line.id.clone()) {
                    // Its message was not created: retried with it.
                    None => Err("skipped".into()),
                    Some(item) => {
                        use base64::Engine as _;
                        match base64::engine::general_purpose::STANDARD.decode(data) {
                            Ok(data) => self
                                .call(|client| {
                                    client.attach_file(
                                        &item,
                                        name,
                                        content_type,
                                        content_id.as_deref(),
                                        &data,
                                    )
                                })
                                .map(|()| item),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                },
                Action::Move {
                    item, to, requires, ..
                } => {
                    if requires.is_some_and(|r| !self.done.contains_key(&r)) {
                        Err("skipped".into())
                    } else {
                        match self.folder_id(to) {
                            Ok(folder) => self.call(|client| client.move_message(item, &folder)),
                            Err(error) => Err(error),
                        }
                    }
                }
                Action::CreateGraph { kind, to, item, .. } => self.create(kind, to, item),
                Action::SetPhoto {
                    requires,
                    to,
                    photo,
                    ..
                } => match self.done.get(requires).and_then(|line| line.id.clone()) {
                    // Its contact was not created: retried with it.
                    None => Err("skipped".into()),
                    Some(contact) => {
                        use base64::Engine as _;
                        match base64::engine::general_purpose::STANDARD.decode(photo) {
                            Ok(photo) => self.container("contact", to).and_then(|folder| {
                                self.call(|client| {
                                    client.set_contact_photo(folder.as_deref(), &contact, &photo)
                                })
                                .map(|()| contact.clone())
                            }),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                },
                other => Err(format!("{} is not a step for Outlook.com", other.kind())),
            };
            self.record(step, result)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::apply_graph_command;
    use crate::audit_cmd::GraphArgs;

    #[test]
    fn refuses_a_plan_for_exchange_and_dry_runs_without_signing_in() {
        let dir = std::env::temp_dir().join(format!("ecm-graph-plan-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let graph = GraphArgs {
            client_id: "id".into(),
            tenant: "consumers".into(),
        };
        let exchange = dir.join("exchange.jsonl");
        fs::write(
            &exchange,
            r#"{"action":"recreate-mime","to":"Inbox","subject":null,"read":null,"sent":null,"received":null,"mime":""}"#,
        )
        .unwrap();
        let error = apply_graph_command(&[exchange], &graph, None, true).unwrap_err();
        assert!(error.contains("not for Outlook.com"), "{error}");

        let outlook = dir.join("outlook.jsonl");
        fs::write(
            &outlook,
            concat!(
                r#"{"action":"create-folder","path":"Mailings/Viadeo"}"#,
                "\n",
                r#"{"action":"create-message","to":"Mailings/Viadeo","subject":"x","message":"{}"}"#,
                "\n",
                r#"{"action":"attach","requires":1,"name":"a.txt","content_type":"text/plain","data":"eA=="}"#,
                "\n",
                r#"{"action":"create-graph","kind":"contact","to":"ecm-test","subject":"Ann","item":"{}"}"#,
                "\n",
                r#"{"action":"set-photo","requires":3,"to":"ecm-test","subject":"Ann","photo":"eA=="}"#,
                "\n"
            ),
        )
        .unwrap();
        assert!(apply_graph_command(&[outlook], &graph, None, true).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }
}
