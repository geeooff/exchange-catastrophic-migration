//! Folder paths: folder names joined with `/`.
//!
//! Folder names may contain `/` ("Commandes / Factures"), so inside a name `\` is
//! written `\\` and `/` is written `\/`. A path is therefore unambiguous, and
//! [`split`] gives back the original names. The top folder's path is `""`.

/// The path of folder `name` under `parent` (`""` for the top folder).
pub fn join(parent: &str, name: &str) -> String {
    let name = name.replace('\\', "\\\\").replace('/', "\\/");
    if parent.is_empty() {
        name
    } else {
        format!("{parent}/{name}")
    }
}

/// The folder names of a path, unescaped. `""` gives no names.
pub fn split(path: &str) -> Vec<String> {
    let mut names = Vec::new();
    if path.is_empty() {
        return names;
    }
    let mut current = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => current.extend(chars.next()),
            '/' => names.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    names.push(current);
    names
}

/// The path of the parent folder, `None` for the top folder.
pub fn parent(path: &str) -> Option<String> {
    let mut names = split(path);
    names.pop()?;
    Some(
        names
            .iter()
            .fold(String::new(), |parent, name| join(&parent, name)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_with_slashes_survive_a_round_trip() {
        let path = join(&join("Archive", "Commandes / Factures"), "Gandi / OVH");
        assert_eq!(path, "Archive/Commandes \\/ Factures/Gandi \\/ OVH");
        assert_eq!(
            split(&path),
            ["Archive", "Commandes / Factures", "Gandi / OVH"]
        );
        assert_eq!(
            parent(&path).as_deref(),
            Some("Archive/Commandes \\/ Factures")
        );
    }

    #[test]
    fn backslashes_and_the_top_folder() {
        assert_eq!(split(&join("", "a\\b")), ["a\\b"]);
        assert!(split("").is_empty());
        assert_eq!(parent("Inbox").as_deref(), Some(""));
        assert_eq!(parent(""), None);
    }
}
