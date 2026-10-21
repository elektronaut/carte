//! JS apps: a `package.json` with a `dev` script runs as the web process.

use std::path::Path;

/// The `dev` script of `dir/package.json`.
pub fn dev_script(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("scripts")?
        .get("dev")?
        .as_str()
        .map(str::to_string)
}

/// The web command for a checkout whose `package.json` has a `dev` script: the
/// script run by the checkout's package manager, with the flags its dev server needs
/// to listen on `$PORT` and only on loopback.
pub fn dev_command(dir: &Path) -> Option<String> {
    let script = dev_script(dir)?;
    let manager = package_manager(dir);
    let flags = dev_server_flags(&script);
    Some(match (manager, flags) {
        (manager, "") => format!("{manager} run dev"),
        ("npm", flags) => format!("npm run dev -- {flags}"),
        (manager, flags) => format!("{manager} run dev {flags}"),
    })
}

/// Whether the Vite that `dir` would load reads `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS`
/// as a single host: before 8.1 it doesn't split it on commas.
pub fn vite_takes_one_host(dir: &Path) -> bool {
    let Some(version) = dir.ancestors().find_map(|d| {
        let text = std::fs::read_to_string(d.join("node_modules/vite/package.json")).ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        json.get("version")?.as_str().map(str::to_string)
    }) else {
        return false;
    };
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) < (8, 1)
}

const LOCKFILES: [(&str, &str); 5] = [
    ("pnpm-lock.yaml", "pnpm"),
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
    ("yarn.lock", "yarn"),
    ("package-lock.json", "npm"),
];

/// The package manager for `dir`: from the nearest lockfile in it or above it, up to
/// its repo's root, then the nearest `packageManager` field, then npm.
pub fn package_manager(dir: &Path) -> &'static str {
    let ancestors = || {
        let mut done = false;
        dir.ancestors().take_while(move |d| {
            let take = !done;
            done = done || d.join(".git").exists();
            take
        })
    };
    if let Some(manager) = ancestors().find_map(|d| {
        LOCKFILES
            .iter()
            .find(|(file, _)| d.join(file).is_file())
            .map(|(_, manager)| *manager)
    }) {
        return manager;
    }
    ancestors()
        .find_map(|d| {
            let text = std::fs::read_to_string(d.join("package.json")).ok()?;
            let json: serde_json::Value = serde_json::from_str(&text).ok()?;
            let field = json.get("packageManager")?.as_str()?;
            let name = field.split('@').next()?;
            LOCKFILES
                .iter()
                .map(|(_, manager)| *manager)
                .find(|manager| *manager == name)
        })
        .unwrap_or("npm")
}

/// Flags for the dev server a script starts: `--port $PORT` for those that ignore
/// `PORT`, `--strictPort` where Vite would move to another port when it's busy, and
/// `-H 127.0.0.1` for `next dev`, which otherwise listens on every interface. None
/// when the script sets its own port, or runs more than one command, since the
/// package manager appends flags to the whole script.
pub fn dev_server_flags(script: &str) -> &'static str {
    if is_compound(script) {
        ""
    } else {
        first_command_flags(script)
    }
}

/// The flags the first command of a script that runs more than one would need,
/// which carte can't pass to it.
pub fn missing_dev_server_flags(script: &str) -> Option<&'static str> {
    Some(first_command_flags(script)).filter(|flags| is_compound(script) && !flags.is_empty())
}

fn is_compound(script: &str) -> bool {
    script.trim().contains(['&', ';', '|', '\n'])
}

fn first_command_flags(script: &str) -> &'static str {
    let first = script
        .split(['&', ';', '|', '\n'])
        .next()
        .unwrap_or_default();
    let mut words = first
        .split_whitespace()
        .skip_while(|w| w.contains('=') || matches!(*w, "cross-env" | "npx" | "bunx"));
    let (Some(program), next) = (words.next(), words.next()) else {
        return "";
    };
    if first
        .split_whitespace()
        .any(|w| w == "--port" || w == "-p" || w.starts_with("--port="))
    {
        return "";
    }
    let subcommand = next.filter(|w| !w.starts_with('-'));
    match (program, subcommand) {
        ("vite", None | Some("dev" | "serve")) | ("react-router", Some("dev")) => {
            "--port $PORT --strictPort"
        }
        ("astro" | "rsbuild" | "vinxi", Some("dev"))
        | ("ng" | "webpack", Some("serve"))
        | ("webpack-dev-server", _) => "--port $PORT",
        ("next", Some("dev")) => "-H 127.0.0.1",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn adds_the_flags_each_dev_server_needs() {
        for (script, flags) in [
            ("vite", "--port $PORT --strictPort"),
            ("vite --host", "--port $PORT --strictPort"),
            ("vite dev", "--port $PORT --strictPort"),
            ("vite build --watch", ""),
            ("NODE_ENV=development vite", "--port $PORT --strictPort"),
            ("cross-env FOO=1 vite", "--port $PORT --strictPort"),
            ("react-router dev", "--port $PORT --strictPort"),
            ("astro dev", "--port $PORT"),
            ("ng serve", "--port $PORT"),
            ("webpack serve --mode development", "--port $PORT"),
            ("webpack-dev-server", "--port $PORT"),
            ("rsbuild dev", "--port $PORT"),
            ("vinxi dev", "--port $PORT"),
            ("next dev", "-H 127.0.0.1"),
            ("next dev --turbopack", "-H 127.0.0.1"),
            ("nuxt dev", ""),
            ("vite --port 3000", ""),
            ("next dev -p 3000", ""),
            ("tsup --watch", ""),
            ("turbo dev", ""),
            ("vite && echo done", ""),
            ("vite & tsc --watch", ""),
            ("vite\n", "--port $PORT --strictPort"),
        ] {
            assert_eq!(dev_server_flags(script), flags, "{script}");
        }
        assert_eq!(
            missing_dev_server_flags("vite & tsc --watch"),
            Some("--port $PORT --strictPort")
        );
        assert_eq!(missing_dev_server_flags("vite"), None);
        assert_eq!(missing_dev_server_flags("tsc --watch & vite"), None);
        assert_eq!(missing_dev_server_flags("vite --port 3000 && echo"), None);
    }

    fn package(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("package.json"), json).unwrap();
    }

    #[test]
    fn finds_the_package_manager_from_the_lockfile_upwards() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("mono");
        let app = repo.join("apps/web");
        package(&app, r#"{"scripts": {"dev": "vite"}}"#);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(package_manager(&app), "npm");

        std::fs::write(tmp.path().join("yarn.lock"), "").unwrap();
        assert_eq!(package_manager(&app), "npm", "stops at the repo's root");

        package(&repo, r#"{"packageManager": "pnpm@9.1.0"}"#);
        assert_eq!(package_manager(&app), "pnpm");

        std::fs::write(repo.join("bun.lock"), "").unwrap();
        assert_eq!(package_manager(&app), "bun");
        assert_eq!(
            dev_command(&app).as_deref(),
            Some("bun run dev --port $PORT --strictPort")
        );
    }

    #[test]
    fn reads_the_installed_vite_version() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("apps/web");
        std::fs::create_dir_all(&app).unwrap();
        assert!(!vite_takes_one_host(&app));
        let vite = tmp.path().join("node_modules/vite");
        package(&vite, r#"{"name": "vite", "version": "7.3.6"}"#);
        assert!(vite_takes_one_host(&app));
        package(&vite, r#"{"name": "vite", "version": "8.1.0"}"#);
        assert!(!vite_takes_one_host(&app));
    }

    #[test]
    fn npm_passes_flags_after_a_separator() {
        let tmp = tempfile::tempdir().unwrap();
        package(tmp.path(), r#"{"scripts": {"dev": "vite"}}"#);
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            dev_command(tmp.path()).as_deref(),
            Some("npm run dev -- --port $PORT --strictPort")
        );
        package(tmp.path(), r#"{"scripts": {"dev": "nuxt dev"}}"#);
        assert_eq!(dev_command(tmp.path()).as_deref(), Some("npm run dev"));
        package(tmp.path(), r#"{"scripts": {"build": "vite build"}}"#);
        assert_eq!(dev_command(tmp.path()), None);
    }

    proptest! {
        #[test]
        fn no_flags_when_the_first_command_passes_a_port(
            words in proptest::collection::vec(
                proptest::sample::select(vec![
                    "vite", "next", "dev", "serve", "astro", "webpack-dev-server", "NODE_ENV=1",
                    "cross-env", "npx", "--host", "--strictPort", "3000",
                ]),
                0..6,
            ),
            port in proptest::sample::select(vec!["--port", "-p", "--port=3000"]),
            at in any::<proptest::sample::Index>(),
            rest in "|( && | & | ; | \\| |\n)[a-z -]{0,12}",
        ) {
            let mut words = words;
            words.insert(at.index(words.len() + 1), port);
            let script = format!("{}{rest}", words.join(" "));
            prop_assert_eq!(dev_server_flags(&script), "", "{}", script);
            prop_assert_eq!(missing_dev_server_flags(&script), None, "{}", script);
        }
    }
}
