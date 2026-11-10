use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// The first executable file called `name` in the directories of `path`.
pub fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|file| {
            file.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
