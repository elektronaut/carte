//! Pure logic for carte: configuration, naming, resolution, templates and policies.
//! Nothing in this crate supervises processes or listens on sockets; it only runs
//! short-lived commands such as git and env providers.

pub mod address;
pub mod config;
pub mod config_edit;
pub mod dirs;
pub mod discovery;
pub mod duration;
pub mod env;
pub mod include;
pub mod kube;
pub mod lifecycle;
pub mod model;
pub mod naming;
pub mod node;
pub mod procfile;
pub mod prune;
pub mod slug;
pub mod teardown;
pub mod template;
pub mod warming;

#[cfg(test)]
mod test_support;
