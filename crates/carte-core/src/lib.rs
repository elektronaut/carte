//! Pure logic for carte: configuration, naming, resolution, templates and policies.
//! Nothing in this crate supervises processes or listens on sockets; it only runs
//! short-lived commands such as git and env providers.

pub mod address;
pub mod duration;
pub mod env;
pub mod lifecycle;
pub mod model;
pub mod naming;
pub mod slug;
pub mod template;

#[cfg(test)]
mod test_support;
