//! `.env`-style files: `KEY=VALUE` lines.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFile {
    pub path: PathBuf,
    pub entries: Vec<EnvFileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileEntry {
    pub key: String,
    pub value: String,
    /// 1-based.
    pub line: usize,
}

#[derive(Debug, Error)]
pub enum EnvFileError {
    #[error("{}:{line}: {message}", path.display())]
    Parse {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("could not read {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
}

/// Reads `names` relative to `checkout`, in order. Missing files are skipped.
pub fn read_env_files(checkout: &Path, names: &[String]) -> Result<Vec<EnvFile>, EnvFileError> {
    let mut files = Vec::new();
    for name in names {
        let path = checkout.join(name);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(EnvFileError::Read { path, source }),
        };
        let entries = parse_env_file(&path, &content)?;
        files.push(EnvFile { path, entries });
    }
    Ok(files)
}

/// Parses `KEY=VALUE` lines with `#` comments, an optional `export ` prefix
/// and single- or double-quoted values; a double-quoted value can span lines.
/// `path` is only used in errors.
pub fn parse_env_file(path: &Path, content: &str) -> Result<Vec<EnvFileEntry>, EnvFileError> {
    let mut entries = Vec::new();
    let mut lines = content.lines().enumerate();
    while let Some((index, raw)) = lines.next() {
        let line = index + 1;
        let err = |message: &str| EnvFileError::Parse {
            path: path.to_path_buf(),
            line,
            message: message.to_string(),
        };
        let text = raw.trim_start();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let text = text
            .strip_prefix("export")
            .filter(|rest| rest.starts_with([' ', '\t']))
            .map_or(text, str::trim_start);
        let Some((key, value)) = text.split_once('=') else {
            return Err(err("expected KEY=VALUE"));
        };
        let key = key.trim_end();
        if !is_valid_key(key) {
            return Err(err(&format!(
                "invalid variable name {key:?}: use letters, digits and underscores"
            )));
        }
        let mut input = value.trim_start().to_string();
        let value = loop {
            match parse_value(&input) {
                Err(m) if m == UNTERMINATED_DOUBLE => match lines.next() {
                    Some((_, more)) => {
                        input.push('\n');
                        input.push_str(more);
                    }
                    None => break Err(m),
                },
                other => break other,
            }
        }
        .map_err(|m| err(&m))?;
        entries.push(EnvFileEntry {
            key: key.to_string(),
            value,
            line,
        });
    }
    Ok(entries)
}

const UNTERMINATED_DOUBLE: &str = "unterminated double-quoted value";

pub(crate) fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn parse_value(input: &str) -> Result<String, String> {
    let (value, rest) = match input.chars().next() {
        Some('\'') => {
            let end = input[1..]
                .find('\'')
                .ok_or("unterminated single-quoted value")?;
            (input[1..1 + end].to_string(), &input[end + 2..])
        }
        Some('"') => {
            let mut value = String::new();
            let mut chars = input[1..].char_indices();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        end = Some(i + 2);
                        break;
                    }
                    '\\' => match chars.next().map(|(_, c)| c) {
                        Some('n') => value.push('\n'),
                        Some('t') => value.push('\t'),
                        Some('r') => value.push('\r'),
                        Some(c @ ('"' | '\\' | '$')) => value.push(c),
                        Some(c) => {
                            value.push('\\');
                            value.push(c);
                        }
                        None => value.push('\\'),
                    },
                    c => value.push(c),
                }
            }
            let end = end.ok_or(UNTERMINATED_DOUBLE)?;
            (value, &input[end..])
        }
        _ => {
            let value = match input.find(" #").or_else(|| input.find("\t#")) {
                Some(comment) => &input[..comment],
                None => input,
            };
            return Ok(value.trim_end().to_string());
        }
    };
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(value)
    } else {
        Err(format!("unexpected text after quoted value: {rest:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Vec<(String, String, usize)> {
        parse_env_file(Path::new(".env"), content)
            .unwrap()
            .into_iter()
            .map(|e| (e.key, e.value, e.line))
            .collect()
    }

    fn one(content: &str) -> String {
        parse(content).remove(0).1
    }

    fn error(content: &str) -> String {
        parse_env_file(Path::new("/w/nova-billing/.env"), content)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn parses_plain_lines_comments_and_blanks() {
        let entries =
            parse("# comment\n\nRAILS_ENV=development\n  # indented\nSECRET_KEY_BASE=abc123\n");
        assert_eq!(
            entries,
            vec![
                ("RAILS_ENV".into(), "development".into(), 3),
                ("SECRET_KEY_BASE".into(), "abc123".into(), 5),
            ]
        );
    }

    #[test]
    fn accepts_export_prefix_and_spaces_around_equals() {
        assert_eq!(parse("export FOO=bar")[0].0, "FOO");
        assert_eq!(
            parse("export\tFOO = bar")[0],
            ("FOO".into(), "bar".into(), 1)
        );
        assert_eq!(parse("exported=1")[0].0, "exported");
    }

    #[test]
    fn unquoted_values_keep_inner_text_and_drop_trailing_comments() {
        assert_eq!(
            one("URL=postgres://u:p@localhost/db?a=b"),
            "postgres://u:p@localhost/db?a=b"
        );
        assert_eq!(one("COLOR=#fff"), "#fff");
        assert_eq!(one("A=one two # comment"), "one two");
        assert_eq!(one("EMPTY="), "");
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(
            one(r"A='a \n $B # not a comment'"),
            r"a \n $B # not a comment"
        );
        assert_eq!(one("A='x' # trailing"), "x");
    }

    #[test]
    fn double_quotes_support_escapes() {
        assert_eq!(one(r#"A="line1\nline2""#), "line1\nline2");
        assert_eq!(one(r#"A="say \"hi\"""#), "say \"hi\"");
        assert_eq!(one(r#"A="back\\slash \d""#), r"back\slash \d");
        assert_eq!(one(r#"A="""#), "");
        assert_eq!(one(r#"A="x y" # comment"#), "x y");
    }

    #[test]
    fn double_quoted_values_can_span_lines() {
        let entries = parse(
            "PRIVATE_KEY=\"-----BEGIN KEY-----\n  abc\\\"def  \n-----END KEY-----\" # pem\nNEXT=1\n",
        );
        assert_eq!(
            entries,
            vec![
                (
                    "PRIVATE_KEY".into(),
                    "-----BEGIN KEY-----\n  abc\"def  \n-----END KEY-----".into(),
                    1
                ),
                ("NEXT".into(), "1".into(), 4),
            ]
        );
        assert_eq!(
            error("A=1\nB=\"open\nstill open\n"),
            "/w/nova-billing/.env:2: unterminated double-quoted value"
        );
    }

    #[test]
    fn reports_errors_with_file_and_line() {
        assert_eq!(
            error("A=1\nnot a pair\n"),
            "/w/nova-billing/.env:2: expected KEY=VALUE"
        );
        assert_eq!(
            error("\n\nA=\"open"),
            "/w/nova-billing/.env:3: unterminated double-quoted value"
        );
        assert!(error("A='open").ends_with(":1: unterminated single-quoted value"));
        assert!(error("A=\"x\" y").contains("unexpected text after quoted value"));
        assert!(error("1A=x").contains("invalid variable name \"1A\""));
        assert!(error("=x").contains("invalid variable name \"\""));
    }

    #[test]
    fn reads_files_in_order_and_skips_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=1\nB=2\n").unwrap();
        std::fs::write(dir.path().join(".powenv"), "export B=3\n").unwrap();
        let files = read_env_files(
            dir.path(),
            &[".env".into(), ".env.local".into(), ".powenv".into()],
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, dir.path().join(".env"));
        assert_eq!(files[1].entries[0].value, "3");
    }

    #[test]
    fn read_errors_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=1\nB\n").unwrap();
        let err = read_env_files(dir.path(), &[".env".into()]).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "{}:2: expected KEY=VALUE",
                dir.path().join(".env").display()
            )
        );
    }
}
