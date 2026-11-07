//! Whether two names are one file, for a tool that must not write over what it reads.
//!
//! Comparing paths, even canonical ones, misses a hard link: two paths and one file, so a write to
//! the second changes the first. This compares the file itself: the volume and file index on Windows,
//! the device and inode on Unix (`same_file::Handle`). Any tool in this crate can use it. It does not
//! close the race with a process that replaces a file between this check and a write.

use std::io;
use std::path::Path;

/// Whether `a` and `b` name one existing file. A name that does not exist is no file, so the answer
/// is then `false`. Any other failure to look is an error, since no answer is known: a caller
/// guarding a write must refuse.
pub fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    match same_file::is_same_file(a, b) {
        Ok(same) => Ok(same),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The first of `inputs` that is `output`, by [`same_file`].
pub fn input_that_is<'a>(output: &Path, inputs: &[&'a Path]) -> io::Result<Option<&'a Path>> {
    for input in inputs {
        if same_file(input, output)? {
            return Ok(Some(input));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("dss_file_identity_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_of_one_file_are_the_same_and_others_are_not() {
        let d = dir("names");
        let (a, b, link, gone) = (d.join("a"), d.join("b"), d.join("link"), d.join("gone"));
        fs::write(&a, b"same bytes").unwrap();
        fs::write(&b, b"same bytes").unwrap();
        fs::hard_link(&a, &link).unwrap();
        assert!(same_file(&a, &a).unwrap());
        assert!(same_file(&a, &d.join(".").join("a")).unwrap());
        assert!(
            same_file(&a, &link).unwrap(),
            "a hard link is the same file"
        );
        assert!(
            !same_file(&a, &b).unwrap(),
            "equal bytes are not the same file"
        );
        assert!(!same_file(&a, &gone).unwrap());
        assert!(!same_file(&gone, &a).unwrap());
        assert_eq!(
            input_that_is(&link, &[b.as_path(), a.as_path()]).unwrap(),
            Some(a.as_path())
        );
        assert_eq!(input_that_is(&gone, &[a.as_path()]).unwrap(), None);
        fs::remove_dir_all(&d).unwrap();
    }
}
