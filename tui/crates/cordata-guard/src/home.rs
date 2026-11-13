//! Where Cordata keeps its state. `CORDATA_HOME` moves it, and with it the person's settings and their own unconfined
//! hooks, so a value a repository could have chosen (a direnv `.envrc`, say) must not be followed. Every binary calls
//! [`enforce`] first thing in `main`.

use std::path::{Component, Path, PathBuf};

/// Is `home` one the repository around `cwd` could have chosen? True when it is relative, has `.` or `..` in it, or
/// lies inside that repository: the outermost folder above `cwd` holding a `.git` (so a planted `sub/.git` does not
/// narrow it), or `cwd` itself when there is none. `$HOME` and `/` never count as the repository (a dotfiles checkout
/// at `$HOME`; an app started from Finder, whose folder is `/`).
pub fn untrusted(home: &Path, cwd: &Path, user_home: Option<&Path>) -> bool {
    if home.is_relative() || home.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir)) {
        return true;
    }
    let user_home = user_home.and_then(|h| std::fs::canonicalize(h).ok());
    let Ok(cwd) = std::fs::canonicalize(cwd) else { return true };
    let excluded = |d: &Path| d == Path::new("/") || Some(d) == user_home.as_deref();
    let repo = cwd.ancestors().filter(|d| !excluded(d) && d.join(".git").exists()).last();
    let boundary = match repo {
        Some(repo) => repo.to_path_buf(),
        None if excluded(&cwd) => return false,
        None => cwd.clone(),
    };
    resolved(home).starts_with(boundary)
}

/// `path` with symlinks resolved: the deepest part that exists, canonicalized, and the rest appended (it has no `..`).
fn resolved(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(ToOwned::to_owned), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = std::fs::canonicalize(&existing).unwrap_or(existing);
    out.extend(rest.into_iter().rev());
    out
}

/// Drops a `CORDATA_HOME` that [`untrusted`] refuses, with a line on stderr, so `~/.cordata` is used. Call it first
/// in `main`, before anything reads the environment from another thread.
pub fn enforce() {
    let Some(home) = std::env::var_os("CORDATA_HOME").filter(|h| !h.is_empty()) else { return };
    let cwd = std::env::current_dir().unwrap_or_default();
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    if untrusted(Path::new(&home), &cwd, user_home.as_deref()) {
        eprintln!("cordata: ignoring CORDATA_HOME={} (relative, or inside this repository); using ~/.cordata", home.to_string_lossy());
        // SAFETY: called first in `main`; nothing else reads or writes the environment yet.
        unsafe { std::env::remove_var("CORDATA_HOME") };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_the_repository_could_choose_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("me");
        let repo = user.join("code/repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("sub/.git")).unwrap();
        std::fs::create_dir_all(user.join("Downloads/x")).unwrap();
        let me = Some(user.as_path());
        assert!(untrusted(Path::new(".cordata"), &repo, me), "relative");
        assert!(untrusted(&repo.join("state"), &repo.join("sub"), me), "a planted sub/.git does not narrow the repository");
        assert!(untrusted(&user.join("gone/../code/repo/state"), &repo, me), "`..` through a missing folder");
        assert!(!untrusted(&user.join("elsewhere"), &repo, me), "outside the repository");
        // $HOME is a dotfiles repository: a plain folder under it is its own boundary, not the whole home.
        std::fs::create_dir_all(user.join(".git")).unwrap();
        assert!(untrusted(&user.join("Downloads/x/.cordata"), &user.join("Downloads/x"), me));
        assert!(!untrusted(&user.join(".cordata-elsewhere"), &user.join("Downloads/x"), me));
        // Started from Finder: the folder is `/`, nothing is a repository.
        assert!(!untrusted(&user.join("anything"), Path::new("/"), me));
    }
}
