use std::ffi::{OsStr, OsString};
use std::fs::DirBuilder;
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
