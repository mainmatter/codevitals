//! Roles that a repository declares in its `.gitattributes` files, as GitHub's
//! Linguist reads them: `linguist-vendored`, `linguist-generated` and
//! `linguist-documentation`, plus `codevitals=<role>` for any role (`test`, `code`, ...).
//! An attribute turned off (`-linguist-vendored`, `linguist-generated=false`) says
//! the path is first-party code whatever its path conventions suggest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gix::attrs::search::{MetadataCollection, Outcome};
use gix::attrs::{Search, StateRef};
use gix::glob::pattern::Case;

/// The roles a `codevitals` attribute may name: those of `path_roles` in metrics.sql.
pub const ROLES: &[&str] = &["vendored", "generated", "documentation", "test", "code"];

/// The `.gitattributes` files of one commit's tree, and a matcher over them.
#[derive(Default)]
pub struct Attributes {
    files: BTreeMap<String, Vec<u8>>, // path of each .gitattributes -> content
    search: Search,
    collection: MetadataCollection,
}

impl Attributes {
    /// Whether `path` is a `.gitattributes` file.
    pub fn is_file(path: &str) -> bool {
        path == ".gitattributes" || path.ends_with("/.gitattributes")
    }

    /// Replaces or removes (`None`) the `.gitattributes` file at `path`.
    pub fn set(&mut self, path: &str, content: Option<Vec<u8>>) {
        match content {
            Some(content) => self.files.insert(path.to_owned(), content),
            None => self.files.remove(path),
        };
        // Shallow files first: patterns in deeper files take precedence, as in git.
        let mut files: Vec<_> = self.files.iter().collect();
        files.sort_by_key(|(path, _)| (path.matches('/').count(), path.as_str()));
        self.search = Search::default();
        self.collection = MetadataCollection::default();
        for (path, content) in files {
            self.search
                .add_patterns_buffer(
                    content,
                    PathBuf::from(path),
                    Some(Path::new("")),
                    &mut self.collection,
                    true,
                )
                .expect("parsing attributes from memory does no I/O");
        }
    }

    /// The role the attributes give `path`, if any.
    pub fn role(&self, path: &str) -> Result<Option<&'static str>> {
        if self.files.is_empty() {
            return Ok(None);
        }
        let mut out = Outcome::default();
        out.initialize(&self.collection);
        self.search.pattern_matching_relative_path(
            path.into(),
            Case::Sensitive,
            Some(false),
            &mut out,
        );
        // `codevitals` wins over Linguist's attributes; among those, the first listed role does.
        let mut role = None;
        for m in out.iter() {
            let declared = match (m.assignment.name.as_str(), m.assignment.state) {
                ("codevitals", StateRef::Value(value)) => {
                    let value = value.as_bstr().to_string();
                    let Some(declared) = ROLES.iter().find(|r| **r == value) else {
                        bail!(
                            "{path}: the .gitattributes value codevitals={value} is not a role; use one of {}",
                            ROLES.join(", ")
                        );
                    };
                    return Ok(Some(declared));
                }
                ("linguist-vendored", StateRef::Set) => "vendored",
                ("linguist-generated", StateRef::Set) => "generated",
                ("linguist-documentation", StateRef::Set) => "documentation",
                (
                    "linguist-vendored" | "linguist-generated" | "linguist-documentation",
                    StateRef::Unset,
                ) => "code",
                _ => continue,
            };
            role.get_or_insert(declared);
        }
        Ok(role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_files_override_and_codevitals_wins() {
        let mut a = Attributes::default();
        a.set(
            ".gitattributes",
            Some(
                b"third/** linguist-vendored\n*.pb.go linguist-generated\nflow/** codevitals=test\n"
                    .to_vec(),
            ),
        );
        a.set(
            "third/ours/.gitattributes",
            Some(b"** -linguist-vendored\n".to_vec()),
        );
        let role = |a: &Attributes, p: &str| a.role(p).unwrap();
        assert_eq!(role(&a, "third/lib/x.c"), Some("vendored"));
        assert_eq!(role(&a, "third/ours/y.c"), Some("code"));
        assert_eq!(role(&a, "api/x.pb.go"), Some("generated"));
        assert_eq!(role(&a, "flow/t.py"), Some("test"));
        assert_eq!(role(&a, "src/main.rs"), None);
        a.set("third/ours/.gitattributes", None);
        assert_eq!(role(&a, "third/ours/y.c"), Some("vendored"));
    }
}
