//! Themes people write in `~/.config/carte/themes/<id>.toml`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use carte_core::config::ConfigError;
use ratatui::style::Color;
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use toml::Spanned;

use super::theme::{self, Colors, Theme};

/// The themes on offer, and what was wrong with the files that didn't load.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub themes: Vec<Theme>,
    pub problems: Vec<String>,
    /// The ids of files that don't load, built-in names among them.
    pub broken: Vec<String>,
}

pub fn dir(config_dir: &Path) -> PathBuf {
    config_dir.join("themes")
}

/// A colour as a theme file writes it: `#rrggbb`, an ANSI name like `bright-black`,
/// `reset`, or a 0–255 index.
struct FileColor(Color);

impl<'de> Deserialize<'de> for FileColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ColorVisitor;

        impl Visitor<'_> for ColorVisitor {
            type Value = FileColor;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a colour: \"#rrggbb\", an ANSI name or a 0–255 index")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<FileColor, E> {
                Color::from_str(value).map(FileColor).map_err(|_| {
                    E::custom(format!(
                        "unknown colour {value:?}; use \"#rrggbb\", an ANSI name or a 0–255 index"
                    ))
                })
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<FileColor, E> {
                u8::try_from(value)
                    .map(|index| FileColor(Color::Indexed(index)))
                    .map_err(|_| E::custom(format!("colour index {value} isn't 0–255")))
            }
        }

        deserializer.deserialize_any(ColorVisitor)
    }
}

macro_rules! roles {
    ($($role:ident),* $(,)?) => {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ThemeFile {
            extends: Option<Spanned<String>>,
            label: Option<String>,
            prefixes: Option<Vec<FileColor>>,
            $($role: Option<FileColor>,)*
        }

        impl ThemeFile {
            /// `base` with the roles this file sets.
            fn apply(&self, mut base: Theme) -> Theme {
                $(if let Some(color) = &self.$role {
                    base.$role = color.0;
                })*
                if let Some(prefixes) = &self.prefixes {
                    base.prefixes = prefixes.iter().map(|c| c.0).collect();
                }
                base
            }
        }
    };
}

roles!(
    fg,
    bg,
    muted,
    accent,
    border,
    border_focus,
    ok,
    warn,
    error,
    shared,
    paused,
    selection_bg,
    selection_fg,
    match_fg,
    match_bg,
    match_current_fg,
    match_current_bg,
);

struct File {
    path: PathBuf,
    text: String,
    theme: ThemeFile,
}

impl File {
    /// `path:line: message`, at where `extends` is written.
    fn at_extends(&self, message: &str) -> String {
        let line = self.theme.extends.as_ref().map_or(1, |extends| {
            self.text[..extends.span().start.min(self.text.len())]
                .matches('\n')
                .count()
                + 1
        });
        format!("{}:{line}: {message}", self.path.display())
    }
}

/// The built-ins, with any a file replaces, followed by the other files' themes by
/// id. A file that doesn't load leaves its theme out and adds a problem.
pub fn load(dir: &Path, colors: Colors) -> Loaded {
    let mut problems = Vec::new();
    let mut files = BTreeMap::new();
    let mut broken = Vec::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "toml")
                && !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
        })
        .collect();
    paths.sort();
    for path in paths {
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                problems.push(format!("{}: {err}", path.display()));
                broken.push(id);
                continue;
            }
        };
        match toml::from_str::<ThemeFile>(&text) {
            Ok(theme) => {
                files.insert(id, File { path, text, theme });
            }
            Err(err) => {
                problems.push(ConfigError::parse(&path, &text, err).to_string());
                broken.push(id);
            }
        }
    }

    let mut resolver = Resolver {
        files: &files,
        broken: &broken,
        builtins: theme::builtins(colors),
        done: BTreeMap::new(),
        problems: Vec::new(),
    };
    for id in files.keys() {
        resolver.resolve(id, &mut Vec::new());
    }
    let Resolver {
        builtins,
        done,
        problems: chain_problems,
        ..
    } = resolver;
    problems.extend(chain_problems);
    let mut resolved = BTreeMap::new();
    for (id, theme) in done {
        match theme {
            Some(theme) => {
                resolved.insert(id, theme);
            }
            None => broken.push(id),
        }
    }
    let mut themes: Vec<Theme> = builtins
        .into_iter()
        .map(|builtin| resolved.remove(&builtin.id).unwrap_or(builtin))
        .collect();
    themes.extend(resolved.into_values());
    broken.sort();
    problems.sort();
    Loaded {
        themes,
        problems,
        broken,
    }
}

/// Resolves each file once, so a problem is reported by the file it's in.
struct Resolver<'a> {
    files: &'a BTreeMap<String, File>,
    broken: &'a [String],
    builtins: Vec<Theme>,
    /// Each resolved file's theme, or `None` when it doesn't load.
    done: BTreeMap<String, Option<Theme>>,
    problems: Vec<String>,
}

impl Resolver<'_> {
    /// The theme of file `id`: what it extends, with its own roles on top. A file
    /// that extends its own id builds on the built-in of that name. `stack` holds
    /// the files being resolved that lead here, for finding cycles.
    fn resolve(&mut self, id: &str, stack: &mut Vec<String>) -> Option<Theme> {
        if let Some(done) = self.done.get(id) {
            return done.clone();
        }
        let theme = self.build(id, stack);
        self.done.insert(id.to_string(), theme.clone());
        theme
    }

    fn builtin(&self, name: &str) -> Option<Theme> {
        self.builtins.iter().find(|t| t.id == name).cloned()
    }

    fn build(&mut self, id: &str, stack: &mut Vec<String>) -> Option<Theme> {
        let files = self.files;
        let file = &files[id];
        let base = match &file.theme.extends {
            None => Theme::terminal(),
            Some(extends) => {
                let name = extends.get_ref();
                if name == id {
                    let Some(theme) = self.builtin(name) else {
                        self.problems.push(file.at_extends(&format!(
                            "extends itself, and {name:?} isn't a built-in theme"
                        )));
                        return None;
                    };
                    theme
                } else if let Some(at) = stack.iter().position(|s| s == name) {
                    let mut cycle = stack[at..].to_vec();
                    cycle.extend([id.to_string(), name.clone()]);
                    self.problems.push(
                        file.at_extends(&format!("extends in a cycle: {}", cycle.join(" → "))),
                    );
                    return None;
                } else if files.contains_key(name) {
                    stack.push(id.to_string());
                    let base = self.resolve(name, stack);
                    stack.pop();
                    let Some(base) = base else {
                        self.problems.push(
                            file.at_extends(&format!("extends {name:?}, which doesn't load")),
                        );
                        return None;
                    };
                    base
                } else if let Some(theme) = self.builtin(name) {
                    theme
                } else if self.broken.contains(name) {
                    self.problems
                        .push(file.at_extends(&format!("extends {name:?}, which doesn't load")));
                    return None;
                } else {
                    self.problems
                        .push(file.at_extends(&format!("extends {name:?}, which isn't a theme")));
                    return None;
                }
            }
        };
        let mut theme = file.theme.apply(base);
        theme.id = id.to_string();
        theme.label = match (&file.theme.label, self.builtin(id)) {
            (Some(label), _) => label.clone(),
            (None, Some(replaced)) => replaced.label,
            (None, None) => id.to_string(),
        };
        Some(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn find<'a>(loaded: &'a Loaded, id: &str) -> Option<&'a Theme> {
        loaded.themes.iter().find(|t| t.id == id)
    }

    #[test]
    fn a_file_extends_a_built_in_and_sets_roles() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "nova.toml",
            r##"extends = "catppuccin-mocha"
accent = "#f5a97f"
muted = "bright-black"
warn = 214
prefixes = ["#8bd5ca", "red"]
"##,
        );
        let loaded = load(dir.path(), Colors::Truecolor);
        assert_eq!(loaded.problems, Vec::<String>::new());
        let nova = find(&loaded, "nova").unwrap();
        let mocha = theme::named("catppuccin-mocha", Colors::Truecolor).unwrap();
        assert_eq!(nova.label, "nova");
        assert_eq!(nova.accent, Color::Rgb(0xf5, 0xa9, 0x7f));
        assert_eq!(nova.muted, Color::DarkGray);
        assert_eq!(nova.warn, Color::Indexed(214));
        assert_eq!(nova.prefixes, [Color::Rgb(0x8b, 0xd5, 0xca), Color::Red]);
        assert_eq!(nova.bg, mocha.bg, "the rest comes from what it extends");
        assert_eq!(
            loaded.themes.last().unwrap().id,
            "nova",
            "custom themes come after the built-ins"
        );
    }

    #[test]
    fn without_extends_a_file_builds_on_terminal() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "shop.toml", "label = \"Shop\"\nok = \"cyan\"\n");
        let loaded = load(dir.path(), Colors::Truecolor);
        let shop = find(&loaded, "shop").unwrap();
        assert_eq!(shop.label, "Shop");
        assert_eq!(
            *shop,
            Theme {
                id: "shop".into(),
                label: "Shop".into(),
                ok: Color::Cyan,
                ..Theme::terminal()
            }
        );
    }

    #[test]
    fn a_file_named_like_a_built_in_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "nord.toml",
            "extends = \"nord\"\nerror = \"#ff0000\"\n",
        );
        let loaded = load(dir.path(), Colors::Truecolor);
        assert_eq!(
            loaded.themes.len(),
            theme::builtins(Colors::Truecolor).len()
        );
        let nord = find(&loaded, "nord").unwrap();
        assert_eq!(nord.error, Color::Rgb(255, 0, 0));
        assert_eq!(nord.label, "Nord");
        assert_eq!(nord.bg, theme::named("nord", Colors::Truecolor).unwrap().bg);
    }

    #[test]
    fn custom_themes_extend_each_other() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "acme.toml",
            "extends = \"dracula\"\nok = \"green\"\n",
        );
        write(
            dir.path(),
            "mono.toml",
            "extends = \"acme\"\nwarn = \"yellow\"\n",
        );
        let loaded = load(dir.path(), Colors::Truecolor);
        let mono = find(&loaded, "mono").unwrap();
        assert_eq!((mono.ok, mono.warn), (Color::Green, Color::Yellow));
        assert_eq!(
            mono.bg,
            theme::named("dracula", Colors::Truecolor).unwrap().bg
        );
    }

    #[test]
    fn broken_files_are_problems_that_name_the_file_and_line() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "acme.toml",
            "ok = \"green\"\ncolour = \"red\"\n",
        );
        write(dir.path(), "nova.toml", "\nok = \"greenish\"\n");
        write(
            dir.path(),
            "shop.toml",
            "ok = \"green\"\nextends = \"nowhere\"\n",
        );
        write(dir.path(), "mono.toml", "ok = [\n");
        let loaded = load(dir.path(), Colors::Truecolor);
        assert_eq!(
            loaded.themes.len(),
            theme::builtins(Colors::Truecolor).len(),
            "none of them loads"
        );
        let problem = |name: &str| {
            loaded
                .problems
                .iter()
                .find(|p| p.contains(name))
                .unwrap_or_else(|| panic!("{name} in {:?}", loaded.problems))
                .clone()
        };
        let acme = problem("acme.toml");
        assert!(acme.contains("acme.toml:2:"), "{acme}");
        assert!(acme.contains("colour"), "{acme}");
        let nova = problem("nova.toml");
        assert!(nova.contains("nova.toml:2:"), "{nova}");
        assert!(nova.contains("greenish"), "{nova}");
        let shop = problem("shop.toml");
        assert!(shop.contains("shop.toml:2:"), "{shop}");
        assert!(shop.contains("nowhere"), "{shop}");
        assert!(problem("mono.toml").contains("mono.toml:"));
    }

    #[test]
    fn extends_cycles_are_problems() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "acme.toml", "extends = \"nova\"\n");
        write(dir.path(), "nova.toml", "extends = \"acme\"\n");
        write(dir.path(), "shop.toml", "extends = \"acme\"\n");
        let loaded = load(dir.path(), Colors::Truecolor);
        assert_eq!(
            loaded.themes.len(),
            theme::builtins(Colors::Truecolor).len()
        );
        assert_eq!(
            loaded.problems,
            [
                format!(
                    "{}:1: extends \"nova\", which doesn't load",
                    dir.path().join("acme.toml").display()
                ),
                format!(
                    "{}:1: extends in a cycle: acme → nova → acme",
                    dir.path().join("nova.toml").display()
                ),
                format!(
                    "{}:1: extends \"acme\", which doesn't load",
                    dir.path().join("shop.toml").display()
                ),
            ],
            "the file that closes the cycle is blamed for it"
        );
        assert_eq!(loaded.broken, ["acme", "nova", "shop"]);
    }

    #[test]
    fn a_broken_parent_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "acme.toml", "extends = \"nova\"\n");
        write(dir.path(), "nova.toml", "extends = \"nowhere\"\n");
        let loaded = load(dir.path(), Colors::Truecolor);
        assert_eq!(loaded.problems.len(), 2, "{:?}", loaded.problems);
        assert!(loaded.problems[0].ends_with("extends \"nova\", which doesn't load"));
        assert!(loaded.problems[1].ends_with("extends \"nowhere\", which isn't a theme"));
    }

    #[test]
    fn hidden_files_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "._shop.toml", "\u{0}\u{5}garbage");
        write(dir.path(), ".#nova.toml", "not = toml = at all");
        let loaded = load(dir.path(), Colors::Truecolor);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(
            loaded.themes.len(),
            theme::builtins(Colors::Truecolor).len()
        );
    }

    #[test]
    fn a_broken_replacement_is_listed_as_broken() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "nord.toml", "error = \"redish\"\n");
        let loaded = load(dir.path(), Colors::Truecolor);
        assert_eq!(loaded.broken, ["nord"]);
        assert_eq!(loaded.problems.len(), 1);
    }

    #[test]
    fn a_missing_folder_means_the_built_ins() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(&dir.path().join("themes"), Colors::Truecolor);
        assert_eq!(loaded.themes, theme::builtins(Colors::Truecolor));
        assert!(loaded.problems.is_empty());
    }
}
