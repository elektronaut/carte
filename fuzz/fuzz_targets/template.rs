#![no_main]

use std::path::{Path, PathBuf};

use carte_core::model::{App, AppSettings, Project, Scheme};
use carte_core::template::{Context, Template, UrlResolver};
use libfuzzer_sys::fuzz_target;

struct Urls;

impl UrlResolver for Urls {
    fn url(&self, app: &App) -> String {
        format!("https://{}", self.host(app))
    }

    fn public_url(&self, app: &App) -> Option<String> {
        self.public_host(app).map(|host| format!("https://{host}"))
    }

    fn host(&self, app: &App) -> String {
        format!("{}.feature-x.shop.test", app.short)
    }

    fn public_host(&self, app: &App) -> Option<String> {
        (app.short == "api").then(|| format!("{}--feature-x--shop.example.dev", app.short))
    }
}

fn app(short: &str) -> App {
    App {
        name: format!("shop-{short}"),
        short: short.into(),
        path: PathBuf::from("/w").join(short),
        repo: None,
        branch_only: false,
        settings: AppSettings::default(),
    }
}

fuzz_target!(|source: &str| {
    let template = match Template::parse(source) {
        Ok(template) => template,
        Err(err) => {
            assert!(source.is_char_boundary(err.position));
            let _ = err.to_string();
            return;
        }
    };
    assert_eq!(template.source(), source);
    let project = Project {
        name: "shop".into(),
        config_path: "/c/projects/shop.toml".into(),
        folder: None,
        strip_prefix: Some("shop-".into()),
        env_provider: None,
        env: Vec::new(),
        requires: Vec::new(),
        scheme: Scheme::Https,
        idle_timeout: None,
        teardown_delay: None,
        from_template: false,
        apps: vec![app("web"), app("api")],
    };
    let ctx = Context {
        project: &project,
        app: &project.apps[0],
        checkout: "main",
        stack: "feature-x",
        branch: "feature/x",
        port: 51234,
        state_dir: Path::new("/state/shop"),
        run_dir: Path::new("/run/shop"),
        urls: &Urls,
    };
    if let Err(err) = template.render(&ctx) {
        assert!(source.is_char_boundary(err.position));
    }
});
