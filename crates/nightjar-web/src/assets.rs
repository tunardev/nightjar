//! No build step and no CDN.

/// Served from its own route, not inlined into the page. This is what
/// lets the CSP forbid inline style outright.
pub(crate) const STYLE: &str = include_str!("style.css");

/// Served from its own route, not inlined into the page. This is what
/// lets the CSP forbid inline script outright. Progressive enhancement
/// only: every page works even if this never loads.
pub(crate) const SCRIPT: &str = include_str!("script.js");
