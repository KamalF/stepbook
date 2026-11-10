use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::DirBuilder;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The first executable file called `name` in the directories of `path`.
pub fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|file| {
            file.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The directory of the run files, which only the user can read (spec
/// 8.3): the scripts can hold secrets. Dropping it deletes it.
pub struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    /// Creates `stepbook-<pid>` in `parent`. It fails if the directory
    /// exists: in a shared temp dir, another user could have made it.
    pub fn create(parent: &Path) -> std::io::Result<Workspace> {
        let dir = parent.join(format!("stepbook-{}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Workspace { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `$XDG_RUNTIME_DIR` if it is an absolute path, else the temp dir.
pub fn workspace_parent(runtime_dir: Option<OsString>) -> PathBuf {
    runtime_dir
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// The exported variables and the working directory that a run starts
/// with (spec 9).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub dir: PathBuf,
    pub vars: BTreeMap<OsString, OsString>,
}

/// The variables that bash sets on its own, and those of stepbook.
fn is_excluded(name: &OsStr) -> bool {
    matches!(name.as_bytes(), b"_" | b"SHLVL" | b"PWD" | b"OLDPWD")
        || name.as_bytes().starts_with(b"STEPBOOK_")
}

/// Whether bash can hold a variable of this name. bash passes the others
/// from its environment to its children, but a script cannot change them,
/// and `compgen -e` does not list them.
fn is_identifier(name: &OsStr) -> bool {
    let name = name.as_bytes();
    name.first().is_some_and(|c| !c.is_ascii_digit())
        && name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the runs of the next commit use it")
)]
impl State {
    pub fn new(dir: PathBuf, vars: impl IntoIterator<Item = (OsString, OsString)>) -> State {
        let vars = vars.into_iter().filter(|(name, _)| !is_excluded(name));
        State {
            dir,
            vars: vars.collect(),
        }
    }

    /// Reads a state file (spec 9.2): entries that end with a NUL byte, the
    /// directory first, then each variable as `NAME=value`, then an empty
    /// entry. Without the empty entry, the file is partial: the wrapper was
    /// killed while it wrote it, and the missing variables would count as
    /// unset.
    pub fn parse(bytes: &[u8]) -> Option<State> {
        let mut entries = bytes.strip_suffix(b"\0\0")?.split(|&b| b == 0);
        let dir = PathBuf::from(OsStr::from_bytes(entries.next()?));
        let mut vars = Vec::new();
        for entry in entries {
            let equals = entry.iter().position(|&b| b == b'=')?;
            let (name, value) = (&entry[..equals], &entry[equals + 1..]);
            vars.push((
                OsStr::from_bytes(name).into(),
                OsStr::from_bytes(value).into(),
            ));
        }
        Some(State::new(dir, vars))
    }

    /// Applies what a run changed from `start` to `end` (spec 9.3). The
    /// session may have changed since `start`, by another run: only the
    /// changes of this run apply.
    pub fn apply(&mut self, start: &State, end: &State) {
        for (name, value) in &end.vars {
            if start.vars.get(name) != Some(value) {
                self.vars.insert(name.clone(), value.clone());
            }
        }
        for name in start.vars.keys() {
            if !end.vars.contains_key(name) && is_identifier(name) {
                self.vars.remove(name);
            }
        }
        // A block can unset PWD, or set it to a relative path.
        if end.dir != start.dir && end.dir.is_absolute() {
            self.dir = end.dir.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn only_the_user_can_read_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::create(tmp.path()).unwrap();
        assert_eq!(
            workspace.dir(),
            tmp.path().join(format!("stepbook-{}", std::process::id()))
        );
        assert_eq!(mode(workspace.dir()), 0o700);
    }

    #[test]
    fn dropping_the_workspace_deletes_it_with_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::create(tmp.path()).unwrap();
        fs::write(workspace.dir().join("run-1.sh"), "ls\n").unwrap();
        let dir = workspace.dir().to_owned();
        drop(workspace);
        assert!(!dir.exists());
    }

    #[test]
    fn the_workspace_must_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(format!("stepbook-{}", std::process::id()));
        std::os::unix::fs::symlink(tmp.path(), &dir).unwrap();
        assert!(Workspace::create(tmp.path()).is_err());
    }

    #[test]
    fn the_workspace_goes_in_the_runtime_dir_if_it_is_absolute() {
        let temp = std::env::temp_dir();
        assert_eq!(
            workspace_parent(Some("/run/user/1".into())),
            Path::new("/run/user/1")
        );
        assert_eq!(workspace_parent(Some("run".into())), temp);
        assert_eq!(workspace_parent(Some("".into())), temp);
        assert_eq!(workspace_parent(None), temp);
    }

    fn make_file(dir: &std::path::Path, mode: u32) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let file = dir.join("bash");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        file
    }

    #[test]
    fn finds_the_first_executable_file_on_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let not_exec = tmp.path().join("a");
        let folder = tmp.path().join("b");
        let exec = tmp.path().join("c");
        make_file(&not_exec, 0o644);
        fs::create_dir_all(folder.join("bash")).unwrap();
        let found = make_file(&exec, 0o755);
        make_file(&tmp.path().join("d"), 0o755);
        let path =
            std::env::join_paths([&not_exec, &folder, &exec, &tmp.path().join("d")]).unwrap();
        assert_eq!(find_in_path("bash", &path), Some(found));
    }

    #[test]
    fn finds_nothing_on_an_empty_path() {
        assert_eq!(find_in_path("bash", OsStr::new("")), None);
    }

    fn state(dir: &str, vars: &[(&str, &str)]) -> State {
        let vars = vars
            .iter()
            .map(|(n, v)| (OsString::from(n), OsString::from(v)));
        State::new(dir.into(), vars)
    }

    /// The bytes that the wrapper writes for a state.
    fn file(state: &State) -> Vec<u8> {
        let mut bytes = state.dir.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        for (name, value) in &state.vars {
            bytes.extend([name.as_bytes(), b"=", value.as_bytes(), b"\0"].concat());
        }
        bytes.push(0);
        bytes
    }

    #[test]
    fn a_state_file_gives_the_directory_and_the_variables() {
        let bytes = b"/tmp/a\nb\0A=1\0B=x=y\0C=\0\0";
        let expected = state("/tmp/a\nb", &[("A", "1"), ("B", "x=y"), ("C", "")]);
        assert_eq!(State::parse(bytes), Some(expected));
        assert_eq!(State::parse(b"\0\0"), Some(state("", &[])));
    }

    #[test]
    fn a_partial_state_file_gives_no_state() {
        for bytes in [
            &b""[..],
            b"/tmp\0",
            b"/tmp\0A=1\0",
            b"/tmp\0A=1",
            b"/tmp\0A\0\0",
        ] {
            assert_eq!(State::parse(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn excluded_variables_never_enter_the_state() {
        let names = ["_", "SHLVL", "PWD", "OLDPWD", "STEPBOOK_FILE", "STEPBOOK_"];
        let vars: Vec<_> = names
            .iter()
            .map(|n| (*n, "1"))
            .chain([("KEPT", "1")])
            .collect();
        let kept = state("/", &[("KEPT", "1")]);
        assert_eq!(state("/", &vars), kept);
        let unfiltered = State {
            dir: "/".into(),
            vars: (vars.iter().map(|(n, v)| (n.into(), v.into()))).collect(),
        };
        assert_eq!(State::parse(&file(&unfiltered)), Some(kept));
    }

    #[test]
    fn a_run_applies_only_its_own_changes() {
        let start = state("/a", &[("SET", "1"), ("UNSET", "1"), ("KEPT", "1")]);
        // Another run changed KEPT and the directory since the start.
        let mut session = state("/b", &[("SET", "1"), ("UNSET", "1"), ("KEPT", "2")]);
        let end = state("/a", &[("SET", "2"), ("KEPT", "1"), ("NEW", "1")]);
        session.apply(&start, &end);
        assert_eq!(
            session,
            state("/b", &[("SET", "2"), ("KEPT", "2"), ("NEW", "1")])
        );
    }

    #[test]
    fn the_run_that_ends_last_wins() {
        let start = state("/a", &[("X", "0")]);
        let mut session = start.clone();
        session.apply(&start, &state("/b", &[("X", "1")]));
        session.apply(&start, &state("/c", &[("X", "2")]));
        assert_eq!(session, state("/c", &[("X", "2")]));
    }

    #[test]
    fn variables_that_bash_cannot_hold_are_never_unset() {
        let start = state("/a", &[("a-b", "1"), ("1A", "1"), ("", "1"), ("A_1", "1")]);
        let mut session = start.clone();
        session.apply(&start, &state("/a", &[]));
        assert_eq!(
            session,
            state("/a", &[("a-b", "1"), ("1A", "1"), ("", "1")])
        );
    }

    #[test]
    fn an_empty_or_relative_directory_changes_nothing() {
        let start = state("/a", &[]);
        let mut session = start.clone();
        session.apply(&start, &state("", &[]));
        session.apply(&start, &state("b", &[]));
        assert_eq!(session.dir, Path::new("/a"));
    }

    fn name() -> impl Strategy<Value = OsString> {
        "[A-Z_][A-Z0-9_]{0,3}".prop_filter_map("excluded", |n| {
            let n = OsString::from(n);
            (!is_excluded(&n)).then_some(n)
        })
    }

    fn value() -> impl Strategy<Value = OsString> {
        let bytes = prop::collection::vec(1..=255u8, 0..12);
        bytes.prop_map(|b| OsStr::from_bytes(&b).to_owned())
    }

    fn any_state() -> impl Strategy<Value = State> {
        let dir = prop_oneof![Just("/a"), Just("/a\nb=c"), Just("/\u{e9}")];
        let vars = prop::collection::btree_map(name(), value(), 0..6);
        (dir, vars).prop_map(|(dir, vars)| State {
            dir: dir.into(),
            vars,
        })
    }

    proptest! {
        #[test]
        fn any_state_survives_the_state_file(state in any_state()) {
            prop_assert_eq!(State::parse(&file(&state)), Some(state));
        }

        #[test]
        fn applying_a_run_to_its_start_gives_its_end(start in any_state(), end in any_state()) {
            let mut session = start.clone();
            session.apply(&start, &end);
            prop_assert_eq!(session, end);
        }
    }
}
