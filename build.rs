//! Records which commit the build came from, for the `start` event and
//! `--version`. See "Which build ran" in docs/ARCHITECTURE.md.
//!
//! Always sets `JAKKALS_COMMIT` (40 hex digits) and `JAKKALS_DIRTY` (`1`
//! or `0`), both empty when unknown, and `JAKKALS_VERSION_TEXT`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The files a build is made from: a change to any of them outside a
/// commit makes the build dirty.
const BUILD_INPUTS: [&str; 4] = ["src", "build.rs", "Cargo.toml", "Cargo.lock"];

/// `--version` shows this many of the commit's hex digits, as git's
/// short hashes do with room to spare.
const SHORT_COMMIT_LEN: usize = 12;

struct Commit {
    sha1: String,
    dirty: bool,
}

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    println!("cargo:rerun-if-changed=build.rs");

    // A packaged crate says where it came from; it is checked first
    // because its unpacked folder may sit inside some other repository.
    let vcs_info = root.join(".cargo_vcs_info.json");
    let commit = if vcs_info.is_file() {
        println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
        std::fs::read_to_string(&vcs_info)
            .ok()
            .and_then(|text| from_vcs_info(&text))
    } else {
        from_git(&root)
    };

    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets it");
    let (sha1, dirty, text) = match &commit {
        None => ("", "", version),
        Some(Commit { sha1, dirty: false }) => {
            let text = format!("{version} ({})", &sha1[..SHORT_COMMIT_LEN]);
            (sha1.as_str(), "0", text)
        }
        Some(Commit { sha1, dirty: true }) => {
            let text = format!("{version} ({}, dirty)", &sha1[..SHORT_COMMIT_LEN]);
            (sha1.as_str(), "1", text)
        }
    };
    println!("cargo:rustc-env=JAKKALS_COMMIT={sha1}");
    println!("cargo:rustc-env=JAKKALS_DIRTY={dirty}");
    println!("cargo:rustc-env=JAKKALS_VERSION_TEXT={text}");
}

/// Reads cargo's `.cargo_vcs_info.json`, which holds `"sha1": "<hex>"`
/// and, only when the package was made from a dirty tree,
/// `"dirty": true`. Read by hand: a JSON crate here would be a build
/// dependency for two fields.
fn from_vcs_info(text: &str) -> Option<Commit> {
    let after = &text[text.find("\"sha1\"")? + "\"sha1\"".len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let sha1 = after.strip_prefix('"')?.split('"').next()?;
    let dirty = text.contains("\"dirty\": true") || text.contains("\"dirty\":true");
    is_sha1(sha1).then(|| Commit {
        sha1: sha1.to_owned(),
        dirty,
    })
}

/// Asks git, when `root` is the top of a checkout. Any failure (no git,
/// not a checkout, a checkout of something else) is unknown.
fn from_git(root: &Path) -> Option<Commit> {
    let top = git(root, &["rev-parse", "--show-toplevel"])?;
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same(Path::new(&top), root) {
        return None;
    }
    watch_head(root);
    for input in BUILD_INPUTS {
        println!("cargo:rerun-if-changed={input}");
    }

    let sha1 = git(root, &["rev-parse", "HEAD"])?;
    if !is_sha1(&sha1) {
        return None;
    }
    let mut status = vec!["--no-optional-locks", "status", "--porcelain", "--"];
    status.extend(BUILD_INPUTS);
    let changes = git(root, &status)?;
    Some(Commit {
        sha1,
        dirty: !changes.is_empty(),
    })
}

/// Reruns this script when HEAD moves: a commit, a checkout or a reset
/// updates HEAD's reflog, the branch's ref or packed-refs. Only files
/// that exist are named, since cargo reruns every build for a missing
/// one.
fn watch_head(root: &Path) {
    let mut files = vec![
        "HEAD".to_owned(),
        "logs/HEAD".to_owned(),
        "packed-refs".to_owned(),
    ];
    if let Some(branch) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
        files.push(branch);
    }
    for file in files {
        if let Some(path) = git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-path", &file],
        ) && Path::new(&path).is_file()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Runs git in `dir`, returning its trimmed output when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn is_sha1(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}
