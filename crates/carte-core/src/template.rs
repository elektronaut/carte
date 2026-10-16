//! The `{{ expr | filter }}` template language used in env templates.

use std::fmt;

use thiserror::Error;

use crate::model::{App, Project};

/// Resolves the URLs that `url:<app>` and `public_url:<app>` expand to,
/// for the stack being rendered.
pub trait UrlResolver {
    fn url(&self, app: &App) -> String;
    /// `None` while the app is not shared.
    fn public_url(&self, app: &App) -> Option<String>;
}

/// Everything a template can refer to. `app` is the app that `app.name`,
/// `app.short` and `url:app` mean: the instance's own app, or the iterated
/// app inside a key template.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub project: &'a Project,
    pub app: &'a App,
    pub checkout: &'a str,
    pub stack: &'a str,
    pub branch: &'a str,
    pub port: u16,
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
        "unknown variable {0:?} (expected app.name, app.short, project, checkout, stack, branch, port, url:<app> or public_url:<app>)"
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
    Branch,
    Port,
    Url { app: AppRef, public: bool },
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
        let public = match lookup.trim_end() {
            "url" => false,
            "public_url" => true,
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
        return Ok(Expr::Url { app, public });
    }
    Ok(match text {
        "app.name" => Expr::AppName,
        "app.short" => Expr::AppShort,
        "project" => Expr::Project,
        "checkout" => Expr::Checkout,
        "stack" => Expr::Stack,
        "branch" => Expr::Branch,
        "port" => Expr::Port,
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
        Expr::Branch => ctx.branch.to_string(),
        Expr::Port => ctx.port.to_string(),
        Expr::Url { app, public } => {
            let app = match app {
                AppRef::Current => ctx.app,
                AppRef::Named(name) => ctx
                    .project
                    .app(name)
                    .or_else(|| ctx.project.app_by_short(name))
                    .ok_or_else(|| TemplateErrorKind::UnknownApp(name.clone()))?,
            };
            if *public {
                ctx.urls
                    .public_url(app)
                    .ok_or_else(|| TemplateErrorKind::NotShared(app.name.clone()))?
            } else {
                ctx.urls.url(app)
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

    pub(crate) struct Urls {
        pub stack: &'static str,
        pub shared: Vec<&'static str>,
    }

    impl UrlResolver for Urls {
        fn url(&self, app: &App) -> String {
            match self.stack {
                "main" => format!("https://{}.make.test", app.short),
                stack => format!("https://{}.{stack}.make.test", app.short),
            }
        }

        fn public_url(&self, app: &App) -> Option<String> {
            self.shared
                .contains(&app.name.as_str())
                .then(|| format!("https://{}--{}--make.example.dev", app.short, self.stack))
        }
    }

    pub(crate) fn app(name: &str) -> App {
        App {
            name: name.into(),
            short: name.strip_prefix("make-").unwrap_or(name).into(),
            path: PathBuf::from("/Users/inge/work").join(name),
            settings: AppSettings::default(),
        }
    }

    pub(crate) fn make_project() -> Project {
        Project {
            name: "make".into(),
            config_path: "/Users/inge/.config/carte/projects/make.toml".into(),
            strip_prefix: Some("make-".into()),
            env_provider: None,
            env: Vec::new(),
            requires: Vec::new(),
            scheme: Scheme::Https,
            idle_timeout: None,
            apps: vec![
                app("make-newsmail-frontend"),
                app("make-newsmail-api"),
                app("make-billing"),
            ],
        }
    }

    fn with_ctx<T>(f: impl FnOnce(&Context) -> T) -> T {
        let project = make_project();
        let urls = Urls {
            stack: "feature-x",
            shared: vec!["make-newsmail-api"],
        };
        let ctx = Context {
            project: &project,
            app: &project.apps[0],
            checkout: "main",
            stack: "feature-x",
            branch: "feature/X",
            port: 51234,
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
        assert_eq!(ok("{{app.name}}"), "make-newsmail-frontend");
        assert_eq!(ok("{{ app.short }}"), "newsmail-frontend");
        assert_eq!(
            ok("{{project}}/{{checkout}}/{{stack}}"),
            "make/main/feature-x"
        );
        assert_eq!(ok("{{branch}}:{{port}}"), "feature/X:51234");
    }

    #[test]
    fn keeps_surrounding_text() {
        assert_eq!(
            ok("postgres:///{{app.name | snake}}_{{checkout}}"),
            "postgres:///make_newsmail_frontend_main"
        );
        assert_eq!(ok("no expressions"), "no expressions");
        assert_eq!(ok(""), "");
        assert_eq!(ok("a } b }} c"), "a } b }} c");
    }

    #[test]
    fn applies_filters_in_order() {
        assert_eq!(ok("{{app.name | upper}}_URL"), "MAKE_NEWSMAIL_FRONTEND_URL");
        assert_eq!(ok("{{app.name|upper|snake}}"), "make_newsmail_frontend");
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
            ok("{{url:make-newsmail-api}}"),
            "https://newsmail-api.feature-x.make.test"
        );
        assert_eq!(ok("{{url:billing}}"), "https://billing.feature-x.make.test");
        assert_eq!(
            ok("{{ url: app }}"),
            "https://newsmail-frontend.feature-x.make.test"
        );
        assert_eq!(
            ok("{{public_url:newsmail-api}}"),
            "https://newsmail-api--feature-x--make.example.dev"
        );
    }

    #[test]
    fn with_app_rebinds_app() {
        let project = make_project();
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
            urls: &urls,
        }
        .with_app(&project.apps[2]);
        assert_eq!(
            render("{{app.short}} {{url:app}}", &ctx).unwrap(),
            "billing https://billing.make.test"
        );
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
        let err = fail("API={{url:make-newsmail-ap}}");
        assert_eq!(
            err.kind,
            TemplateErrorKind::UnknownApp("make-newsmail-ap".into())
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
    }

    #[test]
    fn reports_public_url_while_not_shared() {
        let err = fail("{{public_url:billing}}");
        assert_eq!(
            err.kind,
            TemplateErrorKind::NotShared("make-billing".into())
        );
        assert_eq!(
            err.to_string(),
            "make-billing has no public URL because it is not shared at column 3 in \"{{public_url:billing}}\""
        );
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
        assert!(Template::parse("{{port | shout}}").is_err());
        assert!(Template::parse("{{app.name}}").unwrap().has_expressions());
        assert!(!Template::parse("plain").unwrap().has_expressions());
    }
}
