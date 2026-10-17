use std::fmt;

use thiserror::Error;

/// Turns a name into a DNS label: lowercase, every character outside `[a-z0-9-]`
/// becomes `-`, repeated dashes collapse and leading or trailing dashes are dropped.
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for c in name.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push(c);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    Project,
    App,
}

impl fmt::Display for NameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NameKind::Project => "project name",
            NameKind::App => "app name",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NameError {
    #[error("{kind} {name:?} is too short: it needs at least 3 characters")]
    TooShort { kind: NameKind, name: String },
    #[error("{kind} {name:?} is too long: a DNS label holds at most 63 characters")]
    TooLong { kind: NameKind, name: String },
    #[error("{kind} {name:?} contains {found:?}: only a-z, 0-9 and - are allowed")]
    InvalidChar {
        kind: NameKind,
        name: String,
        found: char,
    },
    #[error("{kind} {name:?} contains \"--\", which carte uses to join public hostnames")]
    DoubleDash { kind: NameKind, name: String },
    #[error("{kind} {name:?} can't start or end with a dash")]
    EdgeDash { kind: NameKind, name: String },
}

pub fn validate_project_name(name: &str) -> Result<(), NameError> {
    validate_label(NameKind::Project, name)
}

/// Validates an app's hostname label (its name after prefix stripping, or its override).
pub fn validate_app_short_name(name: &str) -> Result<(), NameError> {
    validate_label(NameKind::App, name)
}

fn validate_label(kind: NameKind, name: &str) -> Result<(), NameError> {
    let name_owned = || name.to_string();
    if let Some(found) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Err(NameError::InvalidChar {
            kind,
            name: name_owned(),
            found,
        });
    }
    if name.len() < 3 {
        return Err(NameError::TooShort {
            kind,
            name: name_owned(),
        });
    }
    if name.len() > 63 {
        return Err(NameError::TooLong {
            kind,
            name: name_owned(),
        });
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(NameError::EdgeDash {
            kind,
            name: name_owned(),
        });
    }
    if name.contains("--") {
        return Err(NameError::DoubleDash {
            kind,
            name: name_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_names() {
        assert_eq!(slugify("feature-x"), "feature-x");
        assert_eq!(slugify("claude/fix-thing-123"), "claude-fix-thing-123");
        assert_eq!(slugify("Feature/JIRA-42_Thing"), "feature-jira-42-thing");
        assert_eq!(slugify("a//b--c"), "a-b-c");
        assert_eq!(slugify("/-leading/and/trailing-/"), "leading-and-trailing");
        assert_eq!(slugify("v1.2.3"), "v1-2-3");
        assert_eq!(slugify("blåbær"), "bl-b-r");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn slugs_never_contain_double_dashes() {
        for branch in ["x--y", "x/-/y", "x_-_y", "x - y", "x.-.y"] {
            assert!(!slugify(branch).contains("--"), "{branch}");
        }
    }

    #[test]
    fn accepts_valid_names() {
        assert_eq!(validate_project_name("nova"), Ok(()));
        assert_eq!(validate_project_name("eventportal"), Ok(()));
        assert_eq!(validate_app_short_name("postcard-frontend"), Ok(()));
        assert_eq!(validate_app_short_name("api"), Ok(()));
        assert_eq!(validate_app_short_name("s3-proxy"), Ok(()));
        assert_eq!(validate_app_short_name(&"a".repeat(63)), Ok(()));
    }

    #[test]
    fn rejects_short_names() {
        assert!(matches!(
            validate_project_name("mk"),
            Err(NameError::TooShort {
                kind: NameKind::Project,
                ..
            })
        ));
        assert!(matches!(
            validate_app_short_name(""),
            Err(NameError::TooShort {
                kind: NameKind::App,
                ..
            })
        ));
    }

    #[test]
    fn rejects_long_names() {
        assert!(matches!(
            validate_app_short_name(&"a".repeat(64)),
            Err(NameError::TooLong { .. })
        ));
    }

    #[test]
    fn rejects_invalid_characters() {
        assert_eq!(
            validate_app_short_name("News_mail"),
            Err(NameError::InvalidChar {
                kind: NameKind::App,
                name: "News_mail".into(),
                found: 'N'
            })
        );
        assert!(matches!(
            validate_project_name("nova.no"),
            Err(NameError::InvalidChar { found: '.', .. })
        ));
    }

    #[test]
    fn rejects_dashes_in_the_wrong_places() {
        assert!(matches!(
            validate_app_short_name("xn--frontend"),
            Err(NameError::DoubleDash { .. })
        ));
        assert!(matches!(
            validate_app_short_name("-frontend"),
            Err(NameError::EdgeDash { .. })
        ));
        assert!(matches!(
            validate_project_name("nova-"),
            Err(NameError::EdgeDash { .. })
        ));
    }

    #[test]
    fn errors_describe_the_problem() {
        let err = validate_app_short_name("ab").unwrap_err();
        assert_eq!(
            err.to_string(),
            "app name \"ab\" is too short: it needs at least 3 characters"
        );
        let err = validate_project_name("a--b").unwrap_err();
        assert_eq!(
            err.to_string(),
            "project name \"a--b\" contains \"--\", which carte uses to join public hostnames"
        );
    }
}
