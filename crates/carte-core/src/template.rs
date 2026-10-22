//! The `{{ expr | filter }}` template language used in env templates.

use std::fmt;
use std::path::Path;

use thiserror::Error;

use crate::dirs;
use crate::model::{App, Project};

/// Resolves the URLs and hostnames that `url:<app>`, `public_url:<app>`,
/// `host:<app>` and `public_host:<app>` expand to, for the stack being rendered.
pub trait UrlResolver {
    fn url(&self, app: &App) -> String;
    /// `None` while the app is not shared.
    fn public_url(&self, app: &App) -> Option<String>;
    fn host(&self, app: &App) -> String;
    /// `None` while the app is not shared.
    fn public_host(&self, app: &App) -> Option<String>;
}

/// Everything a template can refer to. `app` is the app that `app.name`,
/// `app.short` and `url:app` mean: the instance's own app, or the iterated
/// app inside a key template. `state_dir` and `run_dir` stay the instance's own.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub project: &'a Project,
    pub app: &'a App,
    pub checkout: &'a str,
    pub stack: &'a str,
    pub branch: &'a str,
    pub port: u16,
    pub state_dir: &'a Path,
    pub run_dir: &'a Path,
    pub urls: &'a dyn UrlResolver,
}

impl<'a> Context<'a> {
    pub fn with_app(self, app: &'a App) -> Self {
        Self { app, ..self }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{kind} at column {column} in {template:?}", column = self.column())]
pub struct TemplateError {
    pub template: String,
    /// Byte offset into `template`.
    pub position: usize,
    pub kind: TemplateErrorKind,
}

impl TemplateError {
    /// 1-based character column of `position`.
    pub fn column(&self) -> usize {
        self.template
            .get(..self.position)
            .map_or(self.position, |s| s.chars().count())
            + 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateErrorKind {
    #[error("unclosed {{{{: expected }}}}")]
    Unclosed,
    #[error("empty expression")]
    EmptyExpression,
    #[error("expected a filter name after |")]
    EmptyFilter,
    #[error(
        "unknown variable {0:?} (expected app.name, app.short, project, checkout, stack, stack.id, branch, port, state_dir, run_dir, url:<app>, public_url:<app>, host:<app> or public_host:<app>)"
    )]
    UnknownVariable(String),
    #[error("unknown filter {0:?} (expected upper, snake or slug)")]
    UnknownFilter(String),
    #[error("expected an app name after {0}:")]
    MissingApp(String),
    #[error("unknown app {0:?} in this project")]
    UnknownApp(String),
    #[error("{0} has no public URL because it is not shared")]
    NotShared(String),
    #[error("filters don't apply to quoted text")]
    FilteredText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    source: String,
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Expr {
        expr: Expr,
        position: usize,
        filters: Vec<Filter>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    AppName,
    AppShort,
    Project,
    Checkout,
    Stack,
    StackId,
    Branch,
    Port,
    StateDir,
    RunDir,
    /// `url:`, `public_url:`, `host:` or `public_host:`.
    Url {
        app: AppRef,
        public: bool,
        host: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRef {
    /// The literal word `app`.
    Current,
    /// A full or short app name.
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Upper,
    Snake,
    Slug,
}

impl Filter {
    fn apply(self, input: &str) -> String {
        match self {
            Filter::Upper => underscored(input).to_ascii_uppercase(),
            Filter::Snake => underscored(input).to_ascii_lowercase(),
            Filter::Slug => slug(input),
        }
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, TemplateError> {
        let err = |position, kind| TemplateError {
            template: source.to_string(),
            position,
            kind,
        };
        let mut parts = Vec::new();
        let mut rest = 0;
        while let Some(found) = source[rest..].find("{{") {
            let open = rest + found;
            if open > rest {
                parts.push(Part::Text(source[rest..open].to_string()));
            }
            let inner_start = open + 2;
            let close = source[inner_start..]
                .find("}}")
                .map(|i| inner_start + i)
                .ok_or_else(|| err(open, TemplateErrorKind::Unclosed))?;

            let mut segments = Vec::new();
            let mut segment_start = inner_start;
            for (i, c) in source[inner_start..close].char_indices() {
                if c == '|' {
                    segments.push(segment_start..inner_start + i);
                    segment_start = inner_start + i + 1;
                }
            }
            segments.push(segment_start..close);

            let mut segments = segments.into_iter().map(|range| {
                let raw = &source[range.clone()];
                let leading = raw.len() - raw.trim_start().len();
                (range.start + leading, raw.trim())
            });
            let (position, text) = segments.next().unwrap_or((inner_start, ""));
            if text.is_empty() {
                return Err(err(position, TemplateErrorKind::EmptyExpression));
            }
            if let Some(literal) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                if let Some((position, _)) = segments.next() {
                    return Err(err(position, TemplateErrorKind::FilteredText));
                }
                parts.push(Part::Text(literal.to_string()));
                rest = close + 2;
                continue;
            }
            let expr = parse_expr(text).map_err(|(offset, kind)| err(position + offset, kind))?;
            let filters = segments
                .map(|(position, name)| match name {
                    "upper" => Ok(Filter::Upper),
                    "snake" => Ok(Filter::Snake),
                    "slug" => Ok(Filter::Slug),
                    "" => Err(err(position, TemplateErrorKind::EmptyFilter)),
                    other => Err(err(
                        position,
                        TemplateErrorKind::UnknownFilter(other.to_string()),
                    )),
                })
                .collect::<Result<_, _>>()?;
            parts.push(Part::Expr {
                expr,
                position,
                filters,
            });
            rest = close + 2;
        }
        if rest < source.len() {
            parts.push(Part::Text(source[rest..].to_string()));
        }
        Ok(Self {
            source: source.to_string(),
            parts,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn has_expressions(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Expr { .. }))
    }

    pub fn render(&self, ctx: &Context) -> Result<String, TemplateError> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Expr {
                    expr,
                    position,
                    filters,
                } => {
                    let value = eval(expr, ctx).map_err(|kind| TemplateError {
                        template: self.source.clone(),
                        position: *position,
                        kind,
                    })?;
                    out.push_str(
                        &filters
                            .iter()
                            .fold(value, |value, filter| filter.apply(&value)),
                    );
                }
            }
        }
        Ok(out)
    }
}

/// Parses and renders `source` in one step.
pub fn render(source: &str, ctx: &Context) -> Result<String, TemplateError> {
    Template::parse(source)?.render(ctx)
}

fn parse_expr(text: &str) -> Result<Expr, (usize, TemplateErrorKind)> {
    let unknown = || (0, TemplateErrorKind::UnknownVariable(text.to_string()));
    if let Some((lookup, app)) = text.split_once(':') {
        let (public, host) = match lookup.trim_end() {
            "url" => (false, false),
            "public_url" => (true, false),
            "host" => (false, true),
            "public_host" => (true, true),
            _ => return Err(unknown()),
        };
        let app_offset = lookup.len() + 1 + (app.len() - app.trim_start().len());
        let app = app.trim();
        if app.is_empty() {
            return Err((
                lookup.len(),
                TemplateErrorKind::MissingApp(lookup.trim_end().to_string()),
            ));
        }
        if !app
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err((app_offset, TemplateErrorKind::UnknownApp(app.to_string())));
        }
        let app = match app {
            "app" => AppRef::Current,
            name => AppRef::Named(name.to_string()),
        };
        return Ok(Expr::Url { app, public, host });
    }
    Ok(match text {
        "app.name" => Expr::AppName,
        "app.short" => Expr::AppShort,
        "project" => Expr::Project,
        "checkout" => Expr::Checkout,
        "stack" => Expr::Stack,
        "stack.id" => Expr::StackId,
        "branch" => Expr::Branch,
        "port" => Expr::Port,
        "state_dir" => Expr::StateDir,
        "run_dir" => Expr::RunDir,
        _ => return Err(unknown()),
    })
}

fn eval(expr: &Expr, ctx: &Context) -> Result<String, TemplateErrorKind> {
    Ok(match expr {
        Expr::AppName => ctx.app.name.clone(),
        Expr::AppShort => ctx.app.short.clone(),
        Expr::Project => ctx.project.name.clone(),
        Expr::Checkout => ctx.checkout.to_string(),
        Expr::Stack => ctx.stack.to_string(),
        Expr::StackId => dirs::stack_id(&ctx.project.name, ctx.stack),
        Expr::Branch => ctx.branch.to_string(),
        Expr::Port => ctx.port.to_string(),
        Expr::StateDir => ctx.state_dir.display().to_string(),
        Expr::RunDir => ctx.run_dir.display().to_string(),
        Expr::Url { app, public, host } => {
            let app = match app {
                AppRef::Current => ctx.app,
                AppRef::Named(name) => ctx
                    .project
                    .app(name)
                    .or_else(|| ctx.project.app_by_short(name))
                    .ok_or_else(|| TemplateErrorKind::UnknownApp(name.clone()))?,
            };
            let not_shared = || TemplateErrorKind::NotShared(app.name.clone());
            match (public, host) {
                (false, false) => ctx.urls.url(app),
                (false, true) => ctx.urls.host(app),
                (true, false) => ctx.urls.public_url(app).ok_or_else(not_shared)?,
                (true, true) => ctx.urls.public_host(app).ok_or_else(not_shared)?,
            }
        }
    })
}

fn underscored(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn slug(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{AppSettings, Scheme};
    use proptest::prelude::*;

    pub(crate) struct Urls {
        pub stack: &'static str,
        pub shared: Vec<&'static str>,
    }

    impl UrlResolver for Urls {
        fn url(&self, app: &App) -> String {
            format!("https://{}", self.host(app))
        }

        fn public_url(&self, app: &App) -> Option<String> {
            self.public_host(app).map(|host| format!("https://{host}"))
        }

        fn host(&self, app: &App) -> String {
            match self.stack {
                "main" => format!("{}.nova.test", app.short),
                stack => format!("{}.{stack}.nova.test", app.short),
            }
        }

        fn public_host(&self, app: &App) -> Option<String> {
            self.shared
                .contains(&app.name.as_str())
                .then(|| format!("{}--{}--nova.example.dev", app.short, self.stack))
        }
    }

    pub(crate) fn app(name: &str) -> App {
        App {
            name: name.into(),
            short: name.strip_prefix("nova-").unwrap_or(name).into(),
            path: PathBuf::from("/Users/alex/work").join(name),
            repo: None,
            branch_only: false,
            settings: AppSettings::default(),
        }
    }

    pub(crate) fn nova_project() -> Project {
        Project {
            folder: None,
            name: "nova".into(),
            config_path: "/Users/alex/.config/carte/projects/nova.toml".into(),
            strip_prefix: Some("nova-".into()),
            env_provider: None,
            env: Vec::new(),
            requires: Vec::new(),
            scheme: Scheme::Https,
            idle_timeout: None,
            teardown_delay: None,
            serve_files: None,
            from_template: false,
            apps: vec![
                app("nova-postcard-frontend"),
                app("nova-postcard-api"),
                app("nova-billing"),
            ],
        }
    }

    fn with_ctx<T>(f: impl FnOnce(&Context) -> T) -> T {
        let project = nova_project();
        let urls = Urls {
            stack: "feature-x",
            shared: vec!["nova-postcard-api"],
        };
        let ctx = Context {
            project: &project,
            app: &project.apps[0],
            checkout: "main",
            stack: "feature-x",
            branch: "feature/X",
            port: 51234,
            state_dir: Path::new("/state/data/nova/feature-x/nova-postcard-frontend"),
            run_dir: Path::new("/run/abcdefgh"),
            urls: &urls,
        };
        f(&ctx)
    }

    fn ok(source: &str) -> String {
        with_ctx(|ctx| render(source, ctx)).unwrap()
    }

    fn fail(source: &str) -> TemplateError {
        with_ctx(|ctx| render(source, ctx)).unwrap_err()
    }

    #[test]
    fn renders_variables() {
        assert_eq!(ok("{{app.name}}"), "nova-postcard-frontend");
        assert_eq!(ok("{{ app.short }}"), "postcard-frontend");
        assert_eq!(
            ok("{{project}}/{{checkout}}/{{stack}}"),
            "nova/main/feature-x"
        );
        assert_eq!(ok("{{branch}}:{{port}}"), "feature/X:51234");
        assert_eq!(
            ok("{{state_dir}}/redis"),
            "/state/data/nova/feature-x/nova-postcard-frontend/redis"
        );
        assert_eq!(
            ok("unix://{{ run_dir }}/redis.sock"),
            "unix:///run/abcdefgh/redis.sock"
        );
    }

    #[test]
    fn filters_dont_apply_to_quoted_text() {
        assert_eq!(
            fail("{{\"x\" | upper}}").kind,
            TemplateErrorKind::FilteredText
        );
    }

    #[test]
    fn quoted_text_renders_as_is() {
        assert_eq!(ok("{{\"{{\"}}.Names}}"), "{{.Names}}");
        assert_eq!(
            ok("{{ \"{{\" }}.ID}} {{app.short}}"),
            "{{.ID}} postcard-frontend"
        );
    }

    #[test]
    fn stack_id_is_the_stack_hash() {
        assert_eq!(ok("{{stack.id}}"), dirs::stack_id("nova", "feature-x"));
        assert_eq!(ok("{{stack.id}}").len(), 8);
        assert_eq!(
            ok("{{stack.id | upper}}"),
            ok("{{stack.id}}").to_uppercase()
        );
        assert_eq!(ok("{{run_dir | slug}}"), "run-abcdefgh");
    }

    #[test]
    fn keeps_surrounding_text() {
        assert_eq!(
            ok("postgres:///{{app.name | snake}}_{{checkout}}"),
            "postgres:///nova_postcard_frontend_main"
        );
        assert_eq!(ok("no expressions"), "no expressions");
        assert_eq!(ok(""), "");
        assert_eq!(ok("a } b }} c"), "a } b }} c");
    }

    #[test]
    fn applies_filters_in_order() {
        assert_eq!(ok("{{app.name | upper}}_URL"), "NOVA_POSTCARD_FRONTEND_URL");
        assert_eq!(ok("{{app.name|upper|snake}}"), "nova_postcard_frontend");
        assert_eq!(ok("{{branch | slug}}"), "feature-x");
        assert_eq!(ok("{{branch | slug | upper}}"), "FEATURE_X");
    }

    #[test]
    fn filters_replace_every_non_alphanumeric() {
        assert_eq!(Filter::Upper.apply("a.b c/d"), "A_B_C_D");
        assert_eq!(Filter::Snake.apply("Feature/Ünicode"), "feature__nicode");
        assert_eq!(Filter::Slug.apply("--Feature//X__y--"), "feature-x-y");
    }

    #[test]
    fn resolves_urls_by_full_name_short_name_and_app() {
        assert_eq!(
            ok("{{url:nova-postcard-api}}"),
            "https://postcard-api.feature-x.nova.test"
        );
        assert_eq!(ok("{{url:billing}}"), "https://billing.feature-x.nova.test");
        assert_eq!(
            ok("{{ url: app }}"),
            "https://postcard-frontend.feature-x.nova.test"
        );
        assert_eq!(
            ok("{{public_url:postcard-api}}"),
            "https://postcard-api--feature-x--nova.example.dev"
        );
    }

    #[test]
    fn resolves_hosts_by_full_name_short_name_and_app() {
        assert_eq!(
            ok("{{host:nova-postcard-api}}"),
            "postcard-api.feature-x.nova.test"
        );
        assert_eq!(ok("{{host:billing}}"), "billing.feature-x.nova.test");
        assert_eq!(
            ok("{{ host: app }}"),
            "postcard-frontend.feature-x.nova.test"
        );
        assert_eq!(
            ok("{{public_host:postcard-api}}"),
            "postcard-api--feature-x--nova.example.dev"
        );
        assert_eq!(
            ok("{{host:billing | upper}}"),
            "BILLING_FEATURE_X_NOVA_TEST"
        );
    }

    #[test]
    fn parses_host_lookups() {
        let expr = |source: &str| match &Template::parse(source).unwrap().parts[0] {
            Part::Expr { expr, .. } => expr.clone(),
            Part::Text(_) => unreachable!(),
        };
        assert_eq!(
            expr("{{host:app}}"),
            Expr::Url {
                app: AppRef::Current,
                public: false,
                host: true,
            }
        );
        assert_eq!(
            expr("{{public_host:billing}}"),
            Expr::Url {
                app: AppRef::Named("billing".into()),
                public: true,
                host: true,
            }
        );
        assert_eq!(
            expr("{{url:billing}}"),
            Expr::Url {
                app: AppRef::Named("billing".into()),
                public: false,
                host: false,
            }
        );
    }

    #[test]
    fn with_app_rebinds_app() {
        let project = nova_project();
        let urls = Urls {
            stack: "main",
            shared: vec![],
        };
        let ctx = Context {
            project: &project,
            app: &project.apps[0],
            checkout: "main",
            stack: "main",
            branch: "main",
            port: 3000,
            state_dir: Path::new("/state/data/nova/main/nova-postcard-frontend"),
            run_dir: Path::new("/run/abcdefgh"),
            urls: &urls,
        }
        .with_app(&project.apps[2]);
        assert_eq!(
            render("{{app.short}} {{url:app}}", &ctx).unwrap(),
            "billing https://billing.nova.test"
        );
        assert_eq!(render("{{host:app}}", &ctx).unwrap(), "billing.nova.test");
    }

    #[test]
    fn reports_unclosed_expression_with_position() {
        let err = fail("postgres:///{{app.name");
        assert_eq!(err.kind, TemplateErrorKind::Unclosed);
        assert_eq!(err.position, 12);
        assert_eq!(
            err.to_string(),
            "unclosed {{: expected }} at column 13 in \"postgres:///{{app.name\""
        );
    }

    #[test]
    fn reports_empty_expressions_and_filters() {
        assert_eq!(fail("{{ }}").kind, TemplateErrorKind::EmptyExpression);
        assert_eq!(
            fail("{{ | upper }}").kind,
            TemplateErrorKind::EmptyExpression
        );
        let err = fail("{{app.name | }}");
        assert_eq!(err.kind, TemplateErrorKind::EmptyFilter);
        assert_eq!(err.position, 13);
    }

    #[test]
    fn reports_unknown_variable() {
        let err = fail("x{{ app.nmae }}");
        assert_eq!(
            err.kind,
            TemplateErrorKind::UnknownVariable("app.nmae".into())
        );
        assert_eq!(err.column(), 5);
        assert!(err.to_string().contains("unknown variable \"app.nmae\""));
        assert!(matches!(
            fail("{{link:billing}}").kind,
            TemplateErrorKind::UnknownVariable(_)
        ));
    }

    #[test]
    fn reports_unknown_filter() {
        let err = fail("{{app.name | upper | kebab}}");
        assert_eq!(err.kind, TemplateErrorKind::UnknownFilter("kebab".into()));
        assert_eq!(err.position, 21);
        assert!(
            err.to_string()
                .starts_with("unknown filter \"kebab\" (expected upper, snake or slug)")
        );
    }

    #[test]
    fn reports_unknown_and_missing_apps() {
        let err = fail("API={{url:nova-postcard-ap}}");
        assert_eq!(
            err.kind,
            TemplateErrorKind::UnknownApp("nova-postcard-ap".into())
        );
        assert_eq!(err.position, 6);
        assert_eq!(
            fail("{{url:}}").kind,
            TemplateErrorKind::MissingApp("url".into())
        );
        assert!(matches!(
            fail("{{url:bad app}}").kind,
            TemplateErrorKind::UnknownApp(_)
        ));
        assert_eq!(
            fail("{{host: }}").kind,
            TemplateErrorKind::MissingApp("host".into())
        );
        assert_eq!(
            fail("{{public_host:}}").kind,
            TemplateErrorKind::MissingApp("public_host".into())
        );
        assert_eq!(
            fail("{{host:nova-postcard-ap}}").kind,
            TemplateErrorKind::UnknownApp("nova-postcard-ap".into())
        );
        assert_eq!(
            fail("{{host:nova-postcard-ap}}").to_string(),
            "unknown app \"nova-postcard-ap\" in this project at column 3 in \"{{host:nova-postcard-ap}}\""
        );
    }

    #[test]
    fn reports_public_url_while_not_shared() {
        let err = fail("{{public_url:billing}}");
        assert_eq!(
            err.kind,
            TemplateErrorKind::NotShared("nova-billing".into())
        );
        assert_eq!(
            err.to_string(),
            "nova-billing has no public URL because it is not shared at column 3 in \"{{public_url:billing}}\""
        );
    }

    #[test]
    fn reports_public_host_while_not_shared() {
        let err = fail("{{public_host:billing}}");
        assert_eq!(
            err.kind,
            TemplateErrorKind::NotShared("nova-billing".into())
        );
        assert_eq!(err.column(), 3);
    }

    #[test]
    fn columns_count_characters() {
        let err = fail("ø{{nope}}");
        assert_eq!(err.position, 4);
        assert_eq!(err.column(), 4);
    }

    #[test]
    fn parse_validates_without_context() {
        assert!(Template::parse("{{app.name | upper}}_URL").is_ok());
        assert!(Template::parse("{{url:anything}}").is_ok());
        assert!(Template::parse("{{public_host:anything}}").is_ok());
        assert!(Template::parse("{{hostname:anything}}").is_err());
        assert!(Template::parse("{{port | shout}}").is_err());
        assert!(Template::parse("{{app.name}}").unwrap().has_expressions());
        assert!(!Template::parse("plain").unwrap().has_expressions());
    }

    fn template_source() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            Just("{{".to_string()),
            Just("}}".to_string()),
            Just("|".to_string()),
            Just("\"".to_string()),
            Just(":".to_string()),
            proptest::sample::select(vec![
                "app.name",
                "port",
                "stack.id",
                "url",
                "public_host",
                "nova-billing",
                "upper",
                "snake",
                "slug",
                "shout",
                " ",
            ])
            .prop_map(str::to_string),
            "\\PC{0,4}",
        ];
        proptest::collection::vec(piece, 0..12).prop_map(|pieces| pieces.concat())
    }

    proptest! {
        #[test]
        fn templates_parse_and_render_without_panicking(source in template_source()) {
            match Template::parse(&source) {
                Ok(template) => {
                    prop_assert_eq!(template.source(), source.as_str());
                    if let Err(err) = with_ctx(|ctx| template.render(ctx)) {
                        prop_assert!(source.is_char_boundary(err.position));
                    }
                }
                Err(err) => {
                    prop_assert!(source.is_char_boundary(err.position), "{err:?}");
                    prop_assert!(err.column() <= source.chars().count() + 1);
                }
            }
        }

        #[test]
        fn text_without_braces_renders_to_itself(source in "\\PC{0,40}") {
            prop_assume!(!source.contains("{{"));
            prop_assert_eq!(ok(&source), source);
        }

        #[test]
        fn filters_keep_to_their_alphabets(input in "\\PC{0,40}") {
            let upper = Filter::Upper.apply(&input);
            prop_assert!(
                upper.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            );
            prop_assert_eq!(upper.chars().count(), input.chars().count());
            prop_assert_eq!(Filter::Snake.apply(&input), upper.to_lowercase());
            let slug = Filter::Slug.apply(&input);
            prop_assert!(crate::test_support::is_slug(&slug), "{slug:?}");
        }
    }
}
