//! Normalized properties of one item, to compare copies property by property.

use std::collections::BTreeMap;

/// Item properties under stable names, with normalized values (lower-cased
/// addresses, sorted lists, Unix times), so that the same item read through EWS,
/// Graph or a PST gives the same map.
pub type PropertyMap = BTreeMap<String, String>;

/// Property names a comparison reports, in order. Others are kept for display.
pub const COMPARED: &[&str] = &[
    "class",
    "subject",
    "from",
    "to",
    "cc",
    "sent",
    "received",
    "message_id",
    "read",
    "has_attachments",
    "attachments",
    "body_type",
    "body_length",
    "headers",
];

/// One line of a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// Property name.
    pub name: String,
    /// Value in the reference.
    pub expected: Option<String>,
    /// Value in the copy.
    pub actual: Option<String>,
}

/// Compares a copy with its reference on [`COMPARED`] properties. An empty value
/// and a missing one are equal (one API omits what another sends empty). A body
/// length is equal when within 5 % or 64 characters: servers re-encode bodies. A
/// property missing from the reference is not compared.
pub fn compare(reference: &PropertyMap, copy: &PropertyMap) -> Vec<Difference> {
    compare_on(COMPARED, reference, copy)
}

/// Like [`compare`], on the properties `names` (those of calendar items,
/// contacts or tasks).
pub fn compare_on(names: &[&str], reference: &PropertyMap, copy: &PropertyMap) -> Vec<Difference> {
    names
        .iter()
        .filter_map(|&name| {
            let expected = reference.get(name)?;
            let actual = copy.get(name);
            let equal = match (name, actual) {
                ("body_length", Some(actual)) => {
                    match (expected.parse::<f64>(), actual.parse::<f64>()) {
                        (Ok(a), Ok(b)) => (a - b).abs() <= (a.max(b) * 0.05).max(64.0),
                        _ => expected == actual,
                    }
                }
                (_, Some(actual)) => expected == actual,
                (_, None) => expected.is_empty(),
            };
            (!equal).then(|| Difference {
                name: name.to_owned(),
                expected: Some(expected.clone()),
                actual: actual.cloned(),
            })
        })
        .collect()
}

/// Lower-cases, trims and sorts addresses, joined with `;`.
pub fn address_list<'a>(addresses: impl IntoIterator<Item = &'a str>) -> String {
    let mut list: Vec<String> = addresses
        .into_iter()
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    list.sort();
    list.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> PropertyMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn reports_only_compared_differences() {
        let reference = map(&[
            ("subject", "Hi"),
            ("body_type", "html"),
            ("body_length", "1000"),
            ("id", "A"),
        ]);
        let copy = map(&[
            ("subject", "Hi"),
            ("body_type", "text"),
            ("body_length", "1030"),
            ("id", "B"),
        ]);
        let differences = compare(&reference, &copy);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].name, "body_type");
    }

    #[test]
    fn a_missing_property_in_the_copy_is_a_difference() {
        let differences = compare(&map(&[("from", "a@x")]), &map(&[]));
        assert_eq!(differences[0].actual, None);
    }

    #[test]
    fn empty_and_missing_are_equal_and_short_bodies_get_slack() {
        assert!(compare(&map(&[("cc", "")]), &map(&[])).is_empty());
        assert!(
            compare(
                &map(&[("body_length", "386")]),
                &map(&[("body_length", "409")])
            )
            .is_empty()
        );
        assert_eq!(
            compare(
                &map(&[("body_length", "1411")]),
                &map(&[("body_length", "0")])
            )
            .len(),
            1
        );
    }

    #[test]
    fn addresses_are_normalized() {
        assert_eq!(
            address_list(["B@x.test ", "a@x.test", ""]),
            "a@x.test;b@x.test"
        );
    }
}
