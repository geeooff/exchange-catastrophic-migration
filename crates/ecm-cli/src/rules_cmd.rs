//! `ecm plan-rules`: plan copying the Outlook.com inbox rules into the target,
//! with their folders mapped to the target's.

use std::{collections::HashMap, fs, path::Path};

use ecm_audit::plan::{Action, counts};
use ecm_ews::xml::escape;
use serde_json::Value;

use crate::{EwsArgs, audit_cmd::GraphArgs, connect, plan_cmd::folder_map};

/// Writes a plan that creates, in the target, the Outlook.com inbox rules it
/// does not have (by name). Only reads both mailboxes.
#[cfg(windows)]
pub fn plan_rules_command(
    ews: &EwsArgs,
    graph: &GraphArgs,
    client_only_important: bool,
    out: &Path,
) -> Result<(), String> {
    let client = connect(ews)?;
    let (folders, _) = folder_map(&client)?;
    let tree = client
        .folder_tree("msgfolderroot")
        .map_err(|e| e.to_string())?;
    let ids: HashMap<String, String> = tree
        .with_paths()
        .into_iter()
        .map(|(path, folder)| (path.to_lowercase(), folder.id.clone()))
        .collect();
    let existing: Vec<String> = client
        .inbox_rule_names()
        .map_err(|e| e.to_string())?
        .iter()
        .map(|name| name.to_lowercase())
        .collect();

    let transport = ecm_graph::WinHttpTransport::default();
    let token = ecm_graph::auth::sign_in_with(
        &transport,
        &graph.client_id,
        &graph.tenant,
        "MailboxSettings.Read",
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
    let graph = ecm_graph::GraphClient::new(transport, token);

    let mut paths: HashMap<String, Option<String>> = HashMap::new();
    let mut actions = Vec::new();
    let mut created: Vec<String> = Vec::new();
    let mut rules_planned = 0;
    let mut left_out = Vec::new();
    let rules = graph.inbox_rules().map_err(|e| e.to_string())?;
    println!("Outlook.com has {} inbox rules:", rules.len());
    for rule in &rules {
        let name = rule["displayName"].as_str().unwrap_or_default().to_owned();
        if existing.contains(&name.to_lowercase()) {
            left_out.push(format!("{name:?}: already in the target"));
            continue;
        }
        let mut needed: Vec<String> = Vec::new();
        let mut folder_id = |graph_id: &str| -> Option<String> {
            let path = paths
                .entry(graph_id.to_owned())
                .or_insert_with(|| graph.folder_path(graph_id).ok())
                .clone()?;
            let target = folders.target(&path);
            if !ids.contains_key(&target.to_lowercase()) {
                needed.push(target.clone());
            }
            Some(target)
        };
        let priority = rules_planned + existing.len() + 1;
        // Rules only Outlook ran on the PC (an alert on screen) come without
        // actions: the server can only mark their messages as important.
        if client_only_important && rule["actions"].is_null() && rule["conditions"].is_null() {
            left_out.push(format!(
                "{name:?}: a rule only Outlook ran, without conditions Graph can read: it would match every message"
            ));
            continue;
        }
        let rule = &if client_only_important && rule["actions"].is_null() {
            let mut server = rule.clone();
            server["actions"] = serde_json::json!({"markImportance": "high"});
            server
        } else {
            rule.clone()
        };
        match rule_xml(rule, priority, &mut folder_id) {
            Ok((xml, summary)) => {
                for path in needed {
                    let mut missing = Vec::new();
                    let mut current = Some(path);
                    while let Some(path) = current.filter(|p| !p.is_empty()) {
                        if !ids.contains_key(&path.to_lowercase()) && !created.contains(&path) {
                            missing.push(path.clone());
                        }
                        current = ecm_model::path::parent(&path);
                    }
                    for path in missing.into_iter().rev() {
                        println!("  (folder to create: {path})");
                        created.push(path.clone());
                        actions.push(Action::CreateFolder { path });
                    }
                }
                println!("  {name}: {summary}");
                actions.push(Action::CreateRule { name, rule: xml });
                rules_planned += 1;
            }
            Err(reason) => {
                left_out.push(format!("{name:?}: {reason} (actions: {})", rule["actions"]))
            }
        }
    }

    let mut text = String::new();
    for action in &actions {
        text.push_str(&serde_json::to_string(action).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!();
    println!("Plan: {} steps into {}", actions.len(), out.display());
    for (kind, n) in counts(&actions) {
        println!("  {kind:<14} {n:>7}");
    }
    if !left_out.is_empty() {
        println!();
        println!("Left out:");
        for line in &left_out {
            println!("  {line}");
        }
    }
    println!();
    println!(
        "Nothing was changed. Review the plan, then run `ecm apply --plan {} --dry-run`.",
        out.display()
    );
    Ok(())
}

#[cfg(not(windows))]
pub fn plan_rules_command(_: &EwsArgs, _: &GraphArgs, _: bool, _: &Path) -> Result<(), String> {
    Err("Microsoft Graph access needs Windows for now (WinHTTP)".into())
}

fn strings(element: &str, value: &Value) -> String {
    let items: String = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("<t:String>{}</t:String>", escape(s)))
        .collect();
    if items.is_empty() {
        String::new()
    } else {
        format!("<t:{element}>{items}</t:{element}>")
    }
}

fn addresses(element: &str, value: &Value) -> String {
    let items: String = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let address = r["emailAddress"]["address"].as_str()?;
            let name = r["emailAddress"]["name"]
                .as_str()
                .filter(|n| !n.trim().is_empty())
                .map(|n| format!("<t:Name>{}</t:Name>", escape(n)))
                .unwrap_or_default();
            Some(format!(
                "<t:Address>{name}<t:EmailAddress>{}</t:EmailAddress></t:Address>",
                escape(address)
            ))
        })
        .collect();
    if items.is_empty() {
        String::new()
    } else {
        format!("<t:{element}>{items}</t:{element}>")
    }
}

fn flag(element: &str, value: &Value) -> String {
    if value.as_bool() == Some(true) {
        format!("<t:{element}>true</t:{element}>")
    } else {
        String::new()
    }
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// EWS rule predicates (conditions or exceptions), in schema order.
fn predicates(value: &Value) -> String {
    if !value.is_object() {
        return String::new();
    }
    let mut xml = String::new();
    xml += &strings("Categories", &value["categories"]);
    xml += &strings("ContainsBodyStrings", &value["bodyContains"]);
    xml += &strings("ContainsHeaderStrings", &value["headerContains"]);
    xml += &strings("ContainsRecipientStrings", &value["recipientContains"]);
    xml += &strings("ContainsSenderStrings", &value["senderContains"]);
    xml += &strings(
        "ContainsSubjectOrBodyStrings",
        &value["bodyOrSubjectContains"],
    );
    xml += &strings("ContainsSubjectStrings", &value["subjectContains"]);
    if let Some(action) = value["messageActionFlag"].as_str() {
        let action = if action == "fyi" {
            "FYI".to_owned()
        } else {
            capitalized(action)
        };
        xml += &format!("<t:FlaggedForAction>{action}</t:FlaggedForAction>");
    }
    xml += &addresses("FromAddresses", &value["fromAddresses"]);
    xml += &flag("HasAttachments", &value["hasAttachments"]);
    if let Some(importance) = value["importance"].as_str() {
        xml += &format!("<t:Importance>{}</t:Importance>", capitalized(importance));
    }
    xml += &flag("IsApprovalRequest", &value["isApprovalRequest"]);
    xml += &flag("IsAutomaticForward", &value["isAutomaticForward"]);
    xml += &flag("IsAutomaticReply", &value["isAutomaticReply"]);
    xml += &flag("IsEncrypted", &value["isEncrypted"]);
    xml += &flag("IsMeetingRequest", &value["isMeetingRequest"]);
    xml += &flag("IsMeetingResponse", &value["isMeetingResponse"]);
    xml += &flag("IsNDR", &value["isNonDeliveryReport"]);
    xml += &flag("IsPermissionControlled", &value["isPermissionControlled"]);
    xml += &flag("IsReadReceipt", &value["isReadReceipt"]);
    xml += &flag("IsSigned", &value["isSigned"]);
    xml += &flag("IsVoicemail", &value["isVoicemail"]);
    xml += &flag("NotSentToMe", &value["notSentToMe"]);
    xml += &flag("SentCcMe", &value["sentCcMe"]);
    xml += &flag("SentOnlyToMe", &value["sentOnlyToMe"]);
    xml += &addresses("SentToAddresses", &value["sentToAddresses"]);
    xml += &flag("SentToMe", &value["sentToMe"]);
    xml += &flag("SentToOrCcMe", &value["sentToOrCcMe"]);
    if let Some(sensitivity) = value["sensitivity"].as_str() {
        xml += &format!(
            "<t:Sensitivity>{}</t:Sensitivity>",
            capitalized(sensitivity)
        );
    }
    let size = &value["withinSizeRange"];
    if size.is_object() {
        let mut range = String::new();
        if let Some(min) = size["minimumSize"].as_i64() {
            range += &format!("<t:MinimumSize>{min}</t:MinimumSize>");
        }
        if let Some(max) = size["maximumSize"].as_i64() {
            range += &format!("<t:MaximumSize>{max}</t:MaximumSize>");
        }
        if !range.is_empty() {
            xml += &format!("<t:WithinSizeRange>{range}</t:WithinSizeRange>");
        }
    }
    xml
}

/// Marks a target folder path in rule XML; `ecm apply` replaces it with the
/// folder's id.
pub const FOLDER_PLACEHOLDER: &str = "ecm-folder:";

/// The EWS `<t:Rule>` for a Graph message rule, and a one-line summary.
/// `folder` maps a Graph folder id to the target folder path, written as a
/// [`FOLDER_PLACEHOLDER`]; a rule whose folder cannot be found is refused.
pub fn rule_xml(
    rule: &Value,
    priority: usize,
    folder: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<(String, String), String> {
    let actions = &rule["actions"];
    let mut summary = Vec::new();
    let mut xml = String::new();
    xml += &strings("AssignCategories", &actions["assignCategories"]);
    let mut folder_action = |element: &str, key: &str, xml: &mut String| -> Result<(), String> {
        if let Some(id) = actions[key].as_str() {
            let path = folder(id).ok_or_else(|| format!("its {key} folder was not found"))?;
            *xml += &format!(
                r#"<t:{element}><t:FolderId Id="{FOLDER_PLACEHOLDER}{}"/></t:{element}>"#,
                escape(&path)
            );
            summary.push(format!("{key} {path}"));
        }
        Ok(())
    };
    folder_action("CopyToFolder", "copyToFolder", &mut xml)?;
    xml += &flag("Delete", &actions["delete"]);
    xml += &addresses(
        "ForwardAsAttachmentToRecipients",
        &actions["forwardAsAttachmentTo"],
    );
    xml += &addresses("ForwardToRecipients", &actions["forwardTo"]);
    if let Some(importance) = actions["markImportance"].as_str() {
        xml += &format!(
            "<t:MarkImportance>{}</t:MarkImportance>",
            capitalized(importance)
        );
    }
    xml += &flag("MarkAsRead", &actions["markAsRead"]);
    folder_action("MoveToFolder", "moveToFolder", &mut xml)?;
    xml += &flag("PermanentDelete", &actions["permanentDelete"]);
    xml += &addresses("RedirectToRecipients", &actions["redirectTo"]);
    xml += &flag("StopProcessingRules", &actions["stopProcessingRules"]);
    if xml.is_empty() {
        return Err("no action the target supports".into());
    }
    for (key, label) in [
        ("delete", "delete"),
        ("permanentDelete", "PERMANENT DELETE"),
        ("markAsRead", "mark as read"),
        ("stopProcessingRules", "stop"),
    ] {
        if actions[key].as_bool() == Some(true) {
            summary.push(label.to_owned());
        }
    }
    if let Some(importance) = actions["markImportance"].as_str() {
        summary.push(format!("mark importance {importance}"));
    }
    for key in ["forwardTo", "forwardAsAttachmentTo", "redirectTo"] {
        if actions[key].as_array().is_some_and(|a| !a.is_empty()) {
            summary.push(format!("{key} (sends mail!)"));
        }
    }

    let conditions = predicates(&rule["conditions"]);
    let exceptions = predicates(&rule["exceptions"]);
    let enabled = rule["isEnabled"].as_bool().unwrap_or(true);
    let mut out = format!(
        "<t:Rule><t:DisplayName>{}</t:DisplayName><t:Priority>{priority}</t:Priority><t:IsEnabled>{enabled}</t:IsEnabled>",
        escape(rule["displayName"].as_str().unwrap_or("Rule"))
    );
    if !conditions.is_empty() {
        out += &format!("<t:Conditions>{conditions}</t:Conditions>");
    }
    if !exceptions.is_empty() {
        out += &format!("<t:Exceptions>{exceptions}</t:Exceptions>");
    }
    out += &format!("<t:Actions>{xml}</t:Actions></t:Rule>");
    if !enabled {
        summary.push("disabled".into());
    }
    Ok((out, summary.join(", ")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn converts_a_rule_and_maps_its_folder() {
        let rule = json!({
            "displayName": "GitHub & co",
            "sequence": 1,
            "isEnabled": true,
            "conditions": {
                "senderContains": ["@github.com"],
                "fromAddresses": [{"emailAddress": {"name": "Bot", "address": "bot@example.test"}}],
                "hasAttachments": false
            },
            "exceptions": {"subjectContains": ["security"]},
            "actions": {"moveToFolder": "G1", "markAsRead": true, "stopProcessingRules": true}
        });
        let (xml, summary) = rule_xml(&rule, 3, &mut |id| {
            (id == "G1").then(|| "Mailings/GitHub".to_owned())
        })
        .unwrap();
        assert_eq!(
            xml,
            concat!(
                "<t:Rule><t:DisplayName>GitHub &amp; co</t:DisplayName><t:Priority>3</t:Priority><t:IsEnabled>true</t:IsEnabled>",
                "<t:Conditions><t:ContainsSenderStrings><t:String>@github.com</t:String></t:ContainsSenderStrings>",
                "<t:FromAddresses><t:Address><t:Name>Bot</t:Name><t:EmailAddress>bot@example.test</t:EmailAddress></t:Address></t:FromAddresses></t:Conditions>",
                "<t:Exceptions><t:ContainsSubjectStrings><t:String>security</t:String></t:ContainsSubjectStrings></t:Exceptions>",
                r#"<t:Actions><t:MarkAsRead>true</t:MarkAsRead><t:MoveToFolder><t:FolderId Id="ecm-folder:Mailings/GitHub"/></t:MoveToFolder><t:StopProcessingRules>true</t:StopProcessingRules></t:Actions></t:Rule>"#
            )
        );
        assert_eq!(summary, "moveToFolder Mailings/GitHub, mark as read, stop");
    }

    #[test]
    fn refuses_a_rule_whose_folder_is_missing() {
        let rule = json!({"displayName": "X", "actions": {"moveToFolder": "G9"}});
        assert!(rule_xml(&rule, 1, &mut |_| None).is_err());
    }

    #[test]
    fn flags_rules_that_send_mail() {
        let rule = json!({
            "displayName": "Fwd",
            "isEnabled": false,
            "conditions": {"importance": "high", "messageActionFlag": "fyi"},
            "actions": {"forwardTo": [{"emailAddress": {"address": "a@example.test"}}]}
        });
        let (xml, summary) = rule_xml(&rule, 1, &mut |_| None).unwrap();
        assert!(xml.contains("<t:IsEnabled>false</t:IsEnabled><t:Conditions><t:FlaggedForAction>FYI</t:FlaggedForAction><t:Importance>High</t:Importance></t:Conditions>"));
        assert!(xml.contains("<t:ForwardToRecipients><t:Address><t:EmailAddress>a@example.test</t:EmailAddress></t:Address></t:ForwardToRecipients>"));
        assert_eq!(summary, "forwardTo (sends mail!), disabled");
    }
}
