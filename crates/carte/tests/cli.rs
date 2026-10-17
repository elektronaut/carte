use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use carte_api::{EnvResponse, ProjectList};
use tempfile::TempDir;

/// A sandbox: config, state and `$HOME` all live in one temp dir.
struct Sandbox {
    _dir: TempDir,
    root: PathBuf,
}

struct Run {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Sandbox { _dir: dir, root }
    }

    fn project_file(&self, name: &str) -> PathBuf {
        self.root.join(format!("config/projects/{name}.toml"))
    }

    fn write_project(&self, name: &str, text: &str) {
        let path = self.project_file(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_carte"));
        cmd.args(args)
            .env("CARTE_CONFIG_DIR", self.root.join("config"))
            .env("CARTE_STATE_HOME", self.root.join("state"))
            .env("CARTE_RUNTIME_HOME", self.root.join("tmp"))
            .env("HOME", &self.root)
            .env_remove("VISUAL")
            .env_remove("EDITOR")
            .current_dir(&self.root)
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Run {
        run(&mut self.command(args))
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Run {
        run(self.command(args).current_dir(dir))
    }

    /// A git repo at `rel` with `files`.
    fn repo(&self, rel: &str, files: &[(&str, &str)]) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        for (name, content) in files {
            fs::write(path.join(name), content).unwrap();
        }
        path
    }
}

fn run(cmd: &mut Command) -> Run {
    let Output {
        status,
        stdout,
        stderr,
    } = cmd.output().unwrap();
    Run {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=carte",
            "-c",
            "user.email=carte@example.com",
        ])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn without_a_command_and_a_terminal_prints_help() {
    let out = Sandbox::new().run(&[]);
    assert!(!out.success);
    assert!(out.stdout.contains("Usage: carte"), "{}", out.stdout);
    assert!(
        out.stdout.contains("opens the terminal UI"),
        "{}",
        out.stdout
    );
}

#[test]
fn adds_a_single_repo() {
    let sb = Sandbox::new();
    let repo = sb.repo(
        "myapp",
        &[(
            "Procfile.dev",
            "web: bin/rails server -p 3000\ncss: bin/rails tailwindcss:watch\n",
        )],
    );
    let out = sb.run(&["project", "add", repo.to_str().unwrap(), "--yes"]);
    assert!(out.success, "{}", out.stderr);
    assert_eq!(
        fs::read_to_string(sb.project_file("myapp")).unwrap(),
        "repos = \"~/myapp\"\n"
    );
    assert!(out.stdout.contains("https://myapp.test"), "{}", out.stdout);
    assert!(out.stdout.contains("web, css (Procfile.dev)"));
    assert!(
        out.stdout
            .contains("`bin/rails server -p 3000` runs as `bin/rails server -p $PORT`"),
        "{}",
        out.stdout
    );
}

#[test]
fn uses_the_machine_ports_in_urls() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.root.join("config")).unwrap();
    fs::write(
        sb.root.join("config/config.toml"),
        "tld = \"carte-test\"\nhttps_port = 8443\n",
    )
    .unwrap();
    let repo = sb.repo("myapp", &[("config.ru", "run App\n")]);
    let out = sb.run(&["project", "add", repo.to_str().unwrap(), "--yes"]);
    assert!(out.success, "{}", out.stderr);
    assert!(
        out.stdout.contains("https://myapp.carte-test:8443"),
        "{}",
        out.stdout
    );
}

#[test]
fn adds_the_current_directory() {
    let sb = Sandbox::new();
    let repo = sb.repo("shop", &[("config.ru", "run App\n")]);
    let out = sb.run_in(&repo, &["project", "add", "--yes"]);
    assert!(out.success, "{}", out.stderr);
    assert!(sb.project_file("shop").is_file());
}

#[test]
fn refuses_to_overwrite_a_project() {
    let sb = Sandbox::new();
    let repo = sb.repo("myapp", &[("config.ru", "run App\n")]);
    let path = repo.to_str().unwrap();
    assert!(sb.run(&["project", "add", path, "--yes"]).success);
    fs::write(sb.project_file("myapp"), "repos = \"~/myapp\"\n# mine\n").unwrap();

    let out = sb.run(&["project", "add", path, "--yes"]);
    assert!(!out.success);
    assert!(out.stderr.contains("already exists"), "{}", out.stderr);
    assert!(out.stderr.contains("carte project edit myapp"));
    assert_eq!(
        fs::read_to_string(sb.project_file("myapp")).unwrap(),
        "repos = \"~/myapp\"\n# mine\n"
    );
}

#[test]
fn requires_a_name_when_the_directory_name_is_invalid() {
    let sb = Sandbox::new();
    let repo = sb.repo("My_App", &[("config.ru", "run App\n")]);
    let path = repo.to_str().unwrap();
    let out = sb.run(&["project", "add", path, "--yes"]);
    assert!(!out.success);
    assert!(out.stderr.contains("pass --name my-app"), "{}", out.stderr);

    let out = sb.run(&["project", "add", path, "--yes", "--name", "my-app"]);
    assert!(out.success, "{}", out.stderr);
    assert_eq!(
        fs::read_to_string(sb.project_file("my-app")).unwrap(),
        "repos = \"~/My_App\"\n\n[apps.My_App]\nname = \"my-app\"\n"
    );
    assert!(out.stdout.contains("https://my-app.test"), "{}", out.stdout);
}

#[test]
fn adds_a_directory_of_repos_with_a_prefix() {
    let sb = Sandbox::new();
    sb.repo("work/nova-api", &[("config.ru", "run App\n")]);
    sb.repo("work/nova-frontend", &[("Procfile", "web: bin/serve\n")]);
    sb.repo("work/nova-search", &[]);
    sb.repo("work/other", &[("config.ru", "run App\n")]);
    let work = sb.root.join("work");
    let work = work.to_str().unwrap();

    let out = sb.run(&["project", "add", work, "--yes"]);
    assert!(!out.success);
    assert!(out.stderr.contains("nova-search"), "{}", out.stderr);
    assert!(out.stderr.contains("--command"));
    assert!(!sb.project_file("work").exists());

    let out = sb.run(&[
        "project",
        "add",
        work,
        "--yes",
        "--command",
        "bin/search -p $PORT",
    ]);
    assert!(out.success, "{}", out.stderr);
    assert_eq!(
        fs::read_to_string(sb.project_file("work")).unwrap(),
        "repos = \"~/work/nova-*\"\nstrip_prefix = \"nova-\"\n\n\
         [apps.nova-search.processes.web]\ncommand = \"bin/search -p $PORT\"\n"
    );
    assert!(out.stdout.contains("Left out (no \"nova-\" prefix): other"));
    assert!(
        out.stdout.contains("https://api.work.test"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("https://search.work.test"));
}

#[test]
fn takes_the_prefix_and_name_from_flags() {
    let sb = Sandbox::new();
    sb.repo("work/acme-api", &[("config.ru", "run App\n")]);
    sb.repo("work/acme-web", &[("config.ru", "run App\n")]);
    let work = sb.root.join("work");
    let out = sb.run(&[
        "project",
        "add",
        work.to_str().unwrap(),
        "--yes",
        "--name",
        "acme",
        "--strip-prefix",
        "acme-",
    ]);
    assert!(out.success, "{}", out.stderr);
    assert!(out.stdout.contains("https://web.acme.test"));
    assert!(
        fs::read_to_string(sb.project_file("acme"))
            .unwrap()
            .contains("repos = \"~/work/acme-*\"")
    );
}

#[test]
fn lists_projects_as_json() {
    let sb = Sandbox::new();
    let repo = sb.repo("shop", &[("config.ru", "run App\n")]);
    sb.repo("tools", &[]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feature/x", "../shop-x"],
    );
    sb.write_project("shop", "repos = [\"~/shop\", \"~/tools\"]\n");
    sb.write_project("broken", "repos = \n");

    let out = sb.run(&["project", "ls", "--json"]);
    assert!(out.success, "{}", out.stderr);
    let list: ProjectList = serde_json::from_str(&out.stdout).unwrap();
    let names: Vec<&str> = list.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["broken", "shop"]);
    let broken = &list.projects[0];
    assert!(broken.apps.is_empty());
    assert_eq!(broken.problems.len(), 1);
    let shop = &list.projects[1];
    assert_eq!(shop.stacks, ["feature-x"]);
    let runnable: Vec<(&str, bool)> = shop
        .apps
        .iter()
        .map(|a| (a.name.as_str(), a.runnable))
        .collect();
    assert_eq!(runnable, [("shop", true), ("tools", false)]);

    let out = sb.run(&["project", "ls"]);
    assert!(out.success);
    assert!(
        out.stdout.contains("shop        2  feature-x"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("  ! "), "{}", out.stdout);
}

fn env_project(sb: &Sandbox) -> PathBuf {
    let repo = sb.repo(
        "shop",
        &[
            ("Procfile", "web: bin/serve\nworker: bin/work\n"),
            (".env", "GREETING=from-file\nSECRET=s3cret\n"),
        ],
    );
    sb.write_project(
        "shop",
        "repos = \"~/shop\"\nenv_provider = \"none\"\n\n[env]\nGREETING = \"hello {{app.name}}\"\n\n\
         [apps.shop]\nenv_files = [\".env\"]\n",
    );
    repo
}

#[test]
fn prints_the_env_with_sources() {
    let sb = Sandbox::new();
    let repo = env_project(&sb);
    let out = sb.run_in(&repo, &["env"]);
    assert!(out.success, "{}", out.stderr);
    let line = |key: &str| {
        out.stdout
            .lines()
            .find(|l| l.starts_with(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no {key} in {}", out.stdout))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(
        out.stdout.starts_with("shop@main/web  ~/shop"),
        "{}",
        out.stdout
    );
    assert_eq!(line("GREETING"), "GREETING project template hello shop");
    assert!(
        out.stdout.contains("↳ overrode .env:1: ••••"),
        "{}",
        out.stdout
    );
    assert_eq!(line("SECRET"), "SECRET .env:2 ••••");
    assert!(
        out.stdout.contains("`--values` shows them"),
        "{}",
        out.stdout
    );
    assert_eq!(line("PORT"), "PORT carte <assigned at start>");
    assert_eq!(line("CARTE_URL"), "CARTE_URL carte https://shop.test");
    assert_eq!(line("CARTE_BRANCH"), "CARTE_BRANCH carte main");
    assert_eq!(
        line("NODE_EXTRA_CA_CERTS"),
        format!(
            "NODE_EXTRA_CA_CERTS carte {}",
            sb.root.join("state/ca.pem").display()
        )
    );
    assert_eq!(
        line("PIDFILE"),
        format!(
            "PIDFILE carte {}",
            sb.root.join("state/run/shop/shop@main.pid").display()
        )
    );
}

#[test]
fn prints_the_env_as_json() {
    let sb = Sandbox::new();
    env_project(&sb);
    let out = sb.run(&["env", "shop", "--json"]);
    assert!(out.success, "{}", out.stderr);
    let env: EnvResponse = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(env.instance, "shop@main/web");
    for own in ["CARTE_CONFIG_DIR", "CARTE_STATE_HOME", "CARTE_RUNTIME_HOME"] {
        assert!(
            !env.vars.iter().any(|v| v.key == own),
            "{own} leaked into the app env"
        );
    }
    let greeting = env.vars.iter().find(|v| v.key == "GREETING").unwrap();
    assert_eq!(greeting.value.as_deref(), Some("hello shop"));
    assert!(!greeting.masked);
    let secret = env.vars.iter().find(|v| v.key == "SECRET").unwrap();
    assert_eq!((secret.value.as_deref(), secret.masked), (None, true));
    assert!(
        greeting.source.starts_with("project ("),
        "{}",
        greeting.source
    );
    assert_eq!(
        greeting.overrides,
        [format!("{}:1", sb.root.join("shop/.env").display())]
    );
}

#[test]
fn shows_hidden_values_by_name_or_with_values() {
    let sb = Sandbox::new();
    let repo = env_project(&sb);
    let one = sb.run_in(&repo, &["env", "SECRET"]);
    assert!(one.success, "{}", one.stderr);
    assert_eq!(
        one.stdout.lines().filter(|l| l.contains("s3cret")).count(),
        1
    );
    assert!(!one.stdout.contains("GREETING"), "{}", one.stdout);

    let named = sb.run(&["env", "shop", "GREETING", "--json"]);
    let env: EnvResponse = serde_json::from_str(&named.stdout).unwrap();
    assert_eq!(env.vars.len(), 1);

    let all = sb.run(&["env", "shop", "--values"]);
    assert!(all.stdout.contains("s3cret"), "{}", all.stdout);
    assert!(
        all.stdout.contains("↳ overrode .env:1: from-file"),
        "{}",
        all.stdout
    );
    assert!(!all.stdout.contains("••••"), "{}", all.stdout);

    assert_eq!(sb.run_in(&repo, &["env", "NOPE"]).code, Some(4));
}

#[test]
fn picks_the_process_from_the_address_or_flag() {
    let sb = Sandbox::new();
    env_project(&sb);
    let out = sb.run(&["env", "shop/worker", "--json"]);
    assert!(out.success, "{}", out.stderr);
    let env: EnvResponse = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(env.instance, "shop@main/worker");

    let out = sb.run(&["env", "shop", "--process", "worker", "--json"]);
    let env: EnvResponse = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(env.instance, "shop@main/worker");

    let out = sb.run(&["env", "--project", "shop", "shop/worker", "--json"]);
    let env: EnvResponse = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(env.instance, "shop@main/worker");

    let out = sb.run(&["env", "--project", "shop"]);
    assert!(!out.success);
    assert!(out.stderr.contains("whole project"), "{}", out.stderr);

    let out = sb.run(&["env", "shop/nope"]);
    assert!(!out.success);
    assert!(
        out.stderr.contains("choose from web, worker"),
        "{}",
        out.stderr
    );
}

#[test]
fn env_needs_an_app() {
    let sb = Sandbox::new();
    env_project(&sb);
    let out = sb.run(&["env"]);
    assert_eq!(out.code, Some(4), "{}", out.stderr);
    assert!(out.stderr.contains("not inside an app"), "{}", out.stderr);
}

#[test]
fn exit_codes_tell_failures_apart() {
    let sb = Sandbox::new();
    let repo = env_project(&sb);
    assert_eq!(sb.run_in(&repo, &["env", "nosuchapp"]).code, Some(4));
    assert_eq!(sb.run_in(&repo, &["env", "-p", "nope"]).code, Some(4));
    let ls = sb.run(&["ls"]);
    assert_eq!(ls.code, Some(3), "{}", ls.stderr);
    assert!(ls.stderr.contains("isn't running"), "{}", ls.stderr);
    assert_eq!(sb.run(&["up", "--bogus"]).code, Some(2));
}

/// An `$EDITOR` that replaces the file with `$NEW_CONTENT`.
fn editor(sb: &Sandbox) -> PathBuf {
    let path = sb.root.join("editor.sh");
    fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' \"$NEW_CONTENT\" > \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn edit(sb: &Sandbox, content: &str) -> Run {
    let editor = editor(sb);
    run(sb
        .command(&["project", "edit", "shop"])
        .env("EDITOR", &editor)
        .env("NEW_CONTENT", content))
}

#[test]
fn edit_validates_the_saved_file() {
    let sb = Sandbox::new();
    env_project(&sb);

    let out = edit(&sb, "repos = \"~/shop\"\nenv_provider = \"none\"");
    assert!(out.success, "{}", out.stderr);
    assert!(out.stdout.contains("is valid: 1 apps"), "{}", out.stdout);

    let out = edit(&sb, "repos = \"~/shop\"\nenv_provider = \"none\"");
    assert!(out.stdout.contains("No changes."));

    let out = edit(&sb, "repos = [");
    assert!(!out.success);
    assert!(out.stdout.contains("error:"), "{}", out.stdout);

    let out = edit(&sb, "repos = \"~/nothing-here\"");
    assert!(out.success);
    assert!(
        out.stdout.contains("matches no git repositories"),
        "{}",
        out.stdout
    );
}

#[test]
fn edit_needs_an_existing_project() {
    let sb = Sandbox::new();
    let out = edit(&sb, "");
    assert!(!out.success);
    assert!(out.stderr.contains("carte project add"), "{}", out.stderr);
}

#[test]
fn removes_a_project_with_yes() {
    let sb = Sandbox::new();
    env_project(&sb);
    let out = sb.run(&["project", "rm", "shop"]);
    assert!(!out.success);
    assert!(out.stderr.contains("--yes"), "{}", out.stderr);
    assert!(sb.project_file("shop").exists());

    let out = sb.run(&["project", "rm", "shop", "--yes"]);
    assert!(out.success, "{}", out.stderr);
    assert!(!sb.project_file("shop").exists());

    let out = sb.run(&["project", "rm", "shop", "--yes"]);
    assert!(!out.success);
}
