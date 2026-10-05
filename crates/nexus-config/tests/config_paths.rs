#![forbid(unsafe_code)]

//! Path-resolution coverage for `nexus_config::store`: CLI over environment
//! over platform default, the empty-string rule, the
//! `XDG_CONFIG_HOME`/`HOME` default matrix, and the documented names.
//!
//! Every precedence case injects its own environment lookup, so no test sets
//! a variable: `std::env::set_var` is unsafe on this toolchain and would also
//! make the suite order-dependent. Only `default_path`/`resolve_path`
//! equivalence checks read the real environment, and only through safe
//! `std::env::var`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use nexus_config::store::{
    CONFIG_ENV_VAR, CONFIG_FILE_NAME, default_path, default_path_with, resolve_path,
    resolve_path_with,
};

/// Application directory holding the configuration document.
const APP_DIR: &str = "nexus-agent";

/// Builds an expected path from text, keeping assertions platform-neutral.
fn path(value: &str) -> PathBuf {
    Path::new(value).to_path_buf()
}

/// Copies an optional literal into the owned form the resolver takes.
fn owned(value: Option<&str>) -> Option<String> {
    value.map(str::to_owned)
}

/// The real environment lookup, read-only, for equivalence checks.
fn real_env(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

/// Asserts the two trailing components are `<app dir>/<config file>`.
fn assert_app_layout(resolved: &Path) {
    assert_eq!(
        resolved.file_name().and_then(OsStr::to_str),
        Some(CONFIG_FILE_NAME),
        "document file name"
    );
    assert_eq!(
        resolved
            .parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str),
        Some(APP_DIR),
        "parent directory"
    );
}

#[test]
fn names_are_the_documented_ones() {
    assert_eq!(CONFIG_ENV_VAR, "NEXUS_CONFIG");
    assert_eq!(CONFIG_FILE_NAME, "config.json");
}

#[test]
fn cli_argument_wins_and_the_environment_is_never_read() {
    assert_eq!(
        resolve_path_with(Some("/cli/config.json"), |var| {
            panic!("environment must not be read once a CLI path is present: {var}")
        }),
        Some(path("/cli/config.json")),
        "a CLI path short-circuits every later source"
    );
    assert_eq!(
        resolve_path_with(Some("/cli/config.json"), |_| {
            Some("/env/config.json".to_owned())
        }),
        Some(path("/cli/config.json")),
        "the CLI path beats a set environment variable"
    );
    assert_eq!(
        resolve_path_with(Some("relative/config.json"), |_| None),
        Some(path("relative/config.json")),
        "the CLI path beats the platform default and stays verbatim"
    );
}

#[test]
fn environment_beats_the_default_for_an_absent_or_empty_cli() {
    let mut queried: Vec<String> = Vec::new();
    let resolved = resolve_path_with(None, |var| {
        queried.push(var.to_owned());
        Some("/env/config.json".to_owned())
    });
    assert_eq!(
        queried,
        vec![CONFIG_ENV_VAR.to_owned()],
        "only the documented variable is consulted"
    );
    assert_eq!(
        resolved,
        Some(path("/env/config.json")),
        "the environment is used when the CLI path is absent"
    );
    assert_eq!(
        resolve_path_with(Some(""), |_| Some("/env/config.json".to_owned())),
        Some(path("/env/config.json")),
        "an empty CLI path is ignored, so the environment wins"
    );
}

#[test]
fn only_the_empty_string_is_ignored() {
    assert_eq!(
        resolve_path_with(Some(""), |_| Some(String::new())),
        default_path(),
        "empty CLI and empty environment both fall through"
    );
    assert_eq!(
        resolve_path_with(None, |_| Some(String::new())),
        default_path(),
        "an empty environment value falls through"
    );
    assert_eq!(
        resolve_path_with(Some("/cli/config.json"), |_| Some(String::new())),
        Some(path("/cli/config.json")),
        "an empty environment value never displaces a CLI path"
    );
    assert_eq!(
        resolve_path_with(Some(" "), |_| Some("/env/config.json".to_owned())),
        Some(path(" ")),
        "emptiness is the only filter: nothing is trimmed or validated"
    );
}

#[test]
fn resolution_falls_back_to_the_platform_default() {
    assert_eq!(
        resolve_path_with(None, |_| None),
        default_path(),
        "no CLI path and no environment value is the default"
    );
    assert_eq!(
        resolve_path_with(Some(""), |_| None),
        default_path(),
        "an empty CLI path reaches the default too"
    );
    if let Some(resolved) = default_path() {
        assert_app_layout(&resolved);
    }
}

#[test]
fn default_path_matrix_prefers_xdg_then_home() {
    fn check(case: &str, xdg: Option<&str>, home: Option<&str>, expected: Option<&str>) {
        let resolved = default_path_with(owned(xdg), owned(home));
        assert_eq!(resolved, expected.map(PathBuf::from), "{case}");
        if let Some(found) = &resolved {
            assert_app_layout(found);
        }
    }
    check(
        "xdg set, home set",
        Some("/xdg"),
        Some("/home/user"),
        Some("/xdg/nexus-agent/config.json"),
    );
    check(
        "xdg set, home unset",
        Some("/xdg"),
        None,
        Some("/xdg/nexus-agent/config.json"),
    );
    check(
        "xdg set, home empty",
        Some("/xdg"),
        Some(""),
        Some("/xdg/nexus-agent/config.json"),
    );
    check(
        "xdg unset, home set",
        None,
        Some("/home/user"),
        Some("/home/user/.config/nexus-agent/config.json"),
    );
    check("xdg unset, home unset", None, None, None);
    check("xdg unset, home empty", None, Some(""), None);
    check(
        "xdg empty, home set",
        Some(""),
        Some("/home/user"),
        Some("/home/user/.config/nexus-agent/config.json"),
    );
    check("xdg empty, home unset", Some(""), None, None);
    check("xdg empty, home empty", Some(""), Some(""), None);
    check(
        "xdg trailing separator",
        Some("/xdg/"),
        Some("/home/user"),
        Some("/xdg/nexus-agent/config.json"),
    );
    check(
        "relative xdg base wins over a relative home",
        Some("xdg"),
        Some("home"),
        Some("xdg/nexus-agent/config.json"),
    );
}
#[test]
fn resolve_path_matches_the_injected_precedence_over_the_real_environment() {
    assert_eq!(
        resolve_path(Some("/cli/config.json")),
        Some(path("/cli/config.json")),
        "a CLI path is returned whatever the environment holds"
    );
    assert_eq!(
        resolve_path(Some("")),
        resolve_path_with(Some(""), real_env),
        "an empty CLI path behaves like an absent one"
    );
    assert_eq!(
        resolve_path(None),
        resolve_path_with(None, real_env),
        "the real lookup is the same precedence as the injected one"
    );
}

#[test]
fn default_path_reads_the_real_environment_deterministically() {
    let resolved = default_path();
    assert_eq!(resolved, default_path(), "repeated reads agree");
    assert_eq!(
        resolved,
        default_path_with(real_env("XDG_CONFIG_HOME"), real_env("HOME")),
        "delegates to the pure computation over the same environment"
    );
    if let Some(found) = resolved {
        assert_app_layout(&found);
        assert!(
            found.ends_with(format!("{APP_DIR}/{CONFIG_FILE_NAME}")),
            "{} ends with the application directory and document name",
            found.display()
        );
    }
}
