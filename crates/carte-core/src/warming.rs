//! Learned warming: which apps usually open after which, and which to start ahead.
//!
//! An app's model is its last opens and, for each, the apps that followed it. Opens
//! are paired within one stack, but the model is per app, so every stack shares it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How soon after an open another app's open counts as following it.
pub const WINDOW: Duration = Duration::from_secs(120);

/// Opens kept per app.
const HISTORY: usize = 10;

/// Fewest followed opens before an app is warmed.
const MIN_HITS: usize = 3;

/// How often app `app` followed the opens of another app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub app: String,
    pub hits: usize,
    /// The opens it could have followed.
    pub opens: usize,
}

impl Edge {
    /// Followed at least half the opens, and often enough to go by.
    pub fn warms(&self) -> bool {
        self.hits >= MIN_HITS && self.hits * 2 >= self.opens
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct History {
    next: u64,
    opens: VecDeque<Open>,
}

#[derive(Debug, Clone, PartialEq)]
struct Open {
    seq: u64,
    followed_by: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Recent {
    project: String,
    stack: String,
    app: String,
    seq: u64,
    at: Instant,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Warming {
    /// Per project and app.
    apps: BTreeMap<String, BTreeMap<String, History>>,
    /// Opens within the window, oldest first.
    recent: Vec<Recent>,
    /// The latest open of each app per project and stack.
    latest: BTreeMap<(String, String, String), u64>,
}

/// `warming.json`: per project and app, its opens oldest first, each the apps that
/// followed it.
#[derive(Debug, Serialize, Deserialize)]
struct File {
    version: u32,
    projects: BTreeMap<String, BTreeMap<String, Vec<BTreeSet<String>>>>,
}

impl Warming {
    pub fn new() -> Self {
        Warming::default()
    }

    /// Records that `app` opened in `stack` at `now`, and that it followed the opens
    /// of other apps there within the window. Returns the apps to warm: those that
    /// followed enough of its earlier opens.
    pub fn opened(&mut self, project: &str, stack: &str, app: &str, now: Instant) -> Vec<Edge> {
        let warm = self.warms(project, app);
        self.recent
            .retain(|r| now.saturating_duration_since(r.at) <= WINDOW);
        let before: BTreeSet<String> = self
            .recent
            .iter()
            .filter(|r| r.project == project && r.stack == stack && r.app != app)
            .map(|r| r.app.clone())
            .collect();
        for earlier in before {
            self.follow(project, stack, &earlier, app);
        }
        let history = self
            .apps
            .entry(project.to_string())
            .or_default()
            .entry(app.to_string())
            .or_default();
        let seq = history.next;
        history.next += 1;
        history.opens.push_back(Open {
            seq,
            followed_by: BTreeSet::new(),
        });
        while history.opens.len() > HISTORY {
            history.opens.pop_front();
        }
        self.latest.insert(
            (project.to_string(), stack.to_string(), app.to_string()),
            seq,
        );
        self.recent.push(Recent {
            project: project.to_string(),
            stack: stack.to_string(),
            app: app.to_string(),
            seq,
            at: now,
        });
        warm
    }

    /// Marks `app` as following the latest open of `from` in `stack`, whenever it was:
    /// a request to `app` came from a page of `from`. Returns whether anything changed.
    pub fn referred(&mut self, project: &str, stack: &str, app: &str, from: &str) -> bool {
        app != from && self.follow(project, stack, from, app)
    }

    /// `app` was warmed after `from` opened and stopped unused: lowers the edge by
    /// taking it off the oldest open of `from` it followed.
    pub fn missed(&mut self, project: &str, from: &str, app: &str) -> bool {
        let Some(history) = self
            .apps
            .get_mut(project)
            .and_then(|apps| apps.get_mut(from))
        else {
            return false;
        };
        history
            .opens
            .iter_mut()
            .find(|open| open.followed_by.contains(app))
            .is_some_and(|open| open.followed_by.remove(app))
    }

    fn follow(&mut self, project: &str, stack: &str, from: &str, app: &str) -> bool {
        let latest = (project.to_string(), stack.to_string(), from.to_string());
        let Some(&seq) = self.latest.get(&latest) else {
            return false;
        };
        self.apps
            .get_mut(project)
            .and_then(|apps| apps.get_mut(from))
            .and_then(|history| history.opens.iter_mut().find(|open| open.seq == seq))
            .is_some_and(|open| open.followed_by.insert(app.to_string()))
    }

    /// Every app that followed opens of `app`, most often first.
    pub fn edges(&self, project: &str, app: &str) -> Vec<Edge> {
        let Some(history) = self.apps.get(project).and_then(|apps| apps.get(app)) else {
            return Vec::new();
        };
        let mut hits: BTreeMap<&str, usize> = BTreeMap::new();
        for open in &history.opens {
            for follower in &open.followed_by {
                *hits.entry(follower).or_default() += 1;
            }
        }
        let mut edges: Vec<Edge> = hits
            .into_iter()
            .map(|(follower, hits)| Edge {
                app: follower.to_string(),
                hits,
                opens: history.opens.len(),
            })
            .collect();
        edges.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.app.cmp(&b.app)));
        edges
    }

    /// The apps an open of `app` warms.
    pub fn warms(&self, project: &str, app: &str) -> Vec<Edge> {
        self.edges(project, app)
            .into_iter()
            .filter(Edge::warms)
            .collect()
    }

    /// Drops what's known about apps `keep` rejects, as opener or follower. Returns
    /// whether anything was dropped.
    pub fn retain(&mut self, keep: impl Fn(&str, &str) -> bool) -> bool {
        let before = self.clone();
        self.apps.retain(|project, apps| {
            apps.retain(|app, history| {
                for open in &mut history.opens {
                    open.followed_by.retain(|follower| keep(project, follower));
                }
                keep(project, app)
            });
            !apps.is_empty()
        });
        self.recent.retain(|r| keep(&r.project, &r.app));
        self.latest
            .retain(|(project, _, app), _| keep(project, app));
        *self != before
    }

    pub fn to_json(&self) -> String {
        let file = File {
            version: 1,
            projects: self
                .apps
                .iter()
                .map(|(project, apps)| {
                    let apps = apps
                        .iter()
                        .map(|(app, history)| {
                            let opens = history
                                .opens
                                .iter()
                                .map(|open| open.followed_by.clone())
                                .collect();
                            (app.clone(), opens)
                        })
                        .collect();
                    (project.clone(), apps)
                })
                .collect(),
        };
        serde_json::to_string_pretty(&file).expect("serializable")
    }

    /// Reads `warming.json`; `None` when it isn't one.
    pub fn from_json(text: &str) -> Option<Warming> {
        let file: File = serde_json::from_str(text).ok()?;
        if file.version != 1 {
            return None;
        }
        let apps = file
            .projects
            .into_iter()
            .map(|(project, apps)| {
                let apps = apps
                    .into_iter()
                    .map(|(app, opens)| {
                        let skip = opens.len().saturating_sub(HISTORY);
                        let opens: VecDeque<Open> = opens
                            .into_iter()
                            .skip(skip)
                            .enumerate()
                            .map(|(seq, followed_by)| Open {
                                seq: seq as u64,
                                followed_by,
                            })
                            .collect();
                        let next = opens.len() as u64;
                        (app, History { next, opens })
                    })
                    .collect();
                (project, apps)
            })
            .collect();
        Some(Warming {
            apps,
            ..Warming::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    struct Clock(Instant);

    impl Clock {
        fn after(&mut self, by: Duration) -> Instant {
            self.0 += by;
            self.0
        }
    }

    fn clock() -> Clock {
        Clock(Instant::now())
    }

    fn names(edges: &[Edge]) -> Vec<(&str, usize, usize)> {
        edges
            .iter()
            .map(|e| (e.app.as_str(), e.hits, e.opens))
            .collect()
    }

    /// `first` then `second` in `stack`, a few seconds apart, then a long pause.
    fn pair(w: &mut Warming, c: &mut Clock, stack: &str, first: &str, second: &str) -> Vec<Edge> {
        let warmed = w.opened("shop", stack, first, c.after(WINDOW * 10));
        w.opened("shop", stack, second, c.after(5 * SECOND));
        warmed
    }

    #[test]
    fn warms_after_three_followed_opens() {
        let mut w = Warming::new();
        let mut c = clock();
        assert!(pair(&mut w, &mut c, "main", "web", "billing").is_empty());
        assert!(pair(&mut w, &mut c, "main", "web", "billing").is_empty());
        assert!(pair(&mut w, &mut c, "main", "web", "billing").is_empty());
        let warmed = w.opened("shop", "main", "web", c.after(WINDOW * 10));
        assert_eq!(names(&warmed), [("billing", 3, 3)]);
        assert!(w.warms("shop", "billing").is_empty());
    }

    #[test]
    fn rate_must_be_at_least_half() {
        let mut w = Warming::new();
        let mut c = clock();
        for _ in 0..3 {
            pair(&mut w, &mut c, "main", "web", "billing");
        }
        for _ in 0..3 {
            w.opened("shop", "main", "web", c.after(WINDOW * 10));
        }
        assert_eq!(names(&w.warms("shop", "web")), [("billing", 3, 6)]);
        w.opened("shop", "main", "web", c.after(WINDOW * 10));
        assert_eq!(names(&w.edges("shop", "web")), [("billing", 3, 7)]);
        assert!(w.warms("shop", "web").is_empty());
    }

    #[test]
    fn only_opens_within_the_window_follow() {
        let mut w = Warming::new();
        let mut c = clock();
        w.opened("shop", "main", "web", c.after(SECOND));
        w.opened("shop", "main", "billing", c.after(WINDOW));
        w.opened("shop", "main", "auth", c.after(SECOND));
        let edges = w.edges("shop", "web");
        assert_eq!(names(&edges), [("billing", 1, 1)]);
        assert_eq!(names(&w.edges("shop", "billing")), [("auth", 1, 1)]);
        assert!(w.edges("shop", "auth").is_empty());
    }

    #[test]
    fn opens_pair_within_one_stack_and_feed_one_model() {
        let mut w = Warming::new();
        let mut c = clock();
        w.opened("shop", "main", "web", c.after(SECOND));
        w.opened("shop", "feature-x", "billing", c.after(SECOND));
        w.opened("nova", "main", "billing", c.after(SECOND));
        assert!(w.edges("shop", "web").is_empty());

        pair(&mut w, &mut c, "main", "web", "billing");
        pair(&mut w, &mut c, "feature-x", "web", "billing");
        pair(&mut w, &mut c, "feature-y", "web", "billing");
        let warmed = w.opened("shop", "feature-z", "web", c.after(WINDOW * 10));
        assert_eq!(names(&warmed), [("billing", 3, 4)]);
    }

    #[test]
    fn keeps_the_last_ten_opens() {
        let mut w = Warming::new();
        let mut c = clock();
        for _ in 0..5 {
            pair(&mut w, &mut c, "main", "web", "billing");
        }
        for _ in 0..8 {
            w.opened("shop", "main", "web", c.after(WINDOW * 10));
        }
        assert_eq!(names(&w.edges("shop", "web")), [("billing", 2, 10)]);
        for _ in 0..2 {
            w.opened("shop", "main", "web", c.after(WINDOW * 10));
        }
        assert!(w.edges("shop", "web").is_empty());
    }

    #[test]
    fn nothing_fades_while_an_app_is_unused() {
        let mut w = Warming::new();
        let mut c = clock();
        for _ in 0..3 {
            pair(&mut w, &mut c, "main", "web", "billing");
        }
        c.after(Duration::from_secs(90 * 24 * 3600));
        for _ in 0..20 {
            w.opened("shop", "main", "auth", c.after(WINDOW * 10));
        }
        assert_eq!(names(&w.warms("shop", "web")), [("billing", 3, 3)]);
    }

    #[test]
    fn a_referer_marks_the_latest_open_whenever_it_was() {
        let mut w = Warming::new();
        let mut c = clock();
        w.opened("shop", "main", "billing", c.after(SECOND));
        w.opened("shop", "main", "web", c.after(WINDOW * 10));
        assert!(w.edges("shop", "web").is_empty());
        assert!(w.referred("shop", "main", "billing", "web"));
        assert!(!w.referred("shop", "main", "billing", "web"));
        assert_eq!(names(&w.edges("shop", "web")), [("billing", 1, 1)]);
        assert!(!w.referred("shop", "feature-x", "billing", "auth"));
        assert!(!w.referred("shop", "main", "web", "web"));
    }

    #[test]
    fn misses_lower_the_edge() {
        let mut w = Warming::new();
        let mut c = clock();
        for _ in 0..4 {
            pair(&mut w, &mut c, "main", "web", "billing");
        }
        let warmed = w.opened("shop", "main", "web", c.after(WINDOW * 10));
        assert_eq!(names(&warmed), [("billing", 4, 4)]);
        assert!(w.missed("shop", "web", "billing"));
        assert_eq!(names(&w.edges("shop", "web")), [("billing", 3, 5)]);
        w.opened("shop", "main", "web", c.after(WINDOW * 10));
        assert!(w.missed("shop", "web", "billing"));
        assert_eq!(names(&w.edges("shop", "web")), [("billing", 2, 6)]);
        assert!(w.warms("shop", "web").is_empty());
        assert!(!w.missed("shop", "web", "auth"));
        assert!(!w.missed("nova", "web", "billing"));
    }

    #[test]
    fn retain_drops_gone_apps_and_projects() {
        let mut w = Warming::new();
        let mut c = clock();
        pair(&mut w, &mut c, "main", "web", "billing");
        pair(&mut w, &mut c, "main", "web", "auth");
        w.opened("nova", "main", "web", c.after(SECOND));
        assert!(w.retain(|project, app| project == "shop" && app != "auth"));
        assert_eq!(names(&w.edges("shop", "web")), [("billing", 1, 2)]);
        assert!(w.edges("shop", "auth").is_empty());
        assert!(!w.to_json().contains("nova"));
        assert!(!w.retain(|_, _| true));
        assert!(!w.referred("shop", "main", "web", "auth"));
    }

    #[test]
    fn survives_a_round_trip_but_not_a_bad_file() {
        let mut w = Warming::new();
        let mut c = clock();
        for _ in 0..3 {
            pair(&mut w, &mut c, "main", "web", "billing");
        }
        let loaded = Warming::from_json(&w.to_json()).unwrap();
        assert_eq!(names(&loaded.warms("shop", "web")), [("billing", 3, 3)]);
        let mut loaded = loaded;
        loaded.opened("shop", "main", "web", c.after(WINDOW * 10));
        assert_eq!(names(&loaded.edges("shop", "web")), [("billing", 3, 4)]);

        assert!(Warming::from_json("").is_none());
        assert!(Warming::from_json("{\"version\": 1}").is_none());
        assert!(Warming::from_json("{\"version\": 2, \"projects\": {}}").is_none());
    }
}
