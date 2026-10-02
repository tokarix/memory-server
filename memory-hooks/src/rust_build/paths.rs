//! Literal path joins preserve filesystem component semantics for the evaluator.

use std::path::{Path, PathBuf};

use crate::limits;

use super::Failure;

/// Validate a nonempty UTF-8 path without resolving or cancelling components.
///
/// # Errors
/// NUL, non-UTF-8, empty or oversized paths fail conservatively.
pub fn validate(path: &Path) -> Result<(), Failure> {
    let value = path.to_str().ok_or(Failure::Read)?;
    if value.is_empty() || value.contains('\0') || value.len() > limits::PATH_BYTES {
        return Err(Failure::Limit);
    }
    Ok(())
}

/// Join a literal path to its absolute source base, without lexical cleanup.
/// Storage evaluation must subsequently resolve symlink and `..` components.
///
/// # Errors
/// Invalid paths, a relative base, and join overflow fail conservatively.
pub fn absolute(base: &Path, value: &Path) -> Result<PathBuf, Failure> {
    validate(base)?;
    validate(value)?;
    if !base.is_absolute() {
        return Err(Failure::Cwd);
    }
    let result = base.join(value);
    validate(&result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::absolute;

    #[test]
    fn a_symlink_parent_transition_is_not_cancelled() {
        assert_eq!(
            absolute(Path::new("/work"), Path::new("link/../output")).unwrap(),
            Path::new("/work/link/../output")
        );
        assert_eq!(
            absolute(Path::new("/work"), Path::new("/absolute/space here")).unwrap(),
            Path::new("/absolute/space here")
        );
    }
}
