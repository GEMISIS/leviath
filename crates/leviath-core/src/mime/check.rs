//! A check a registry row can put on a type.
//!
//! A mime type is a claim. The syntax of the claim is checked wherever a
//! type is written (`type/subtype`, no parameters), and the registry's magic
//! prefixes and extensions decide a type for bytes nobody named, but nothing
//! in the core asks whether bytes that *arrive* as `image/png` are a PNG.
//! A row's `check` is where that question is asked: a script (or, for an
//! embedder, any implementation of [`MimeCheck`]) that sees the bytes and
//! the type they claim, and says what is wrong when something is. It runs
//! once, where bytes are stored, so every ingress (an upload, a tool result,
//! a `read_file`, a model reply, an artifact) is covered by one line.
//!
//! The core holds the check as a trait object because it cannot run Rhai:
//! the scripting crate compiles the file a row names and hands the registry
//! something that answers this trait.

use super::MimeType;

/// Something that can say whether bytes are what they claim to be.
pub trait MimeCheck: Send + Sync {
    /// `Ok(())` when `bytes` may be stored as `mime_type`, or the reason
    /// they may not. The reason reaches whoever handed the bytes in: the
    /// model, the API caller, the person attaching a file.
    fn check(&self, mime_type: &MimeType, bytes: &[u8]) -> Result<(), String>;

    /// What this check is, for a listing: the script path as written.
    fn describe(&self) -> String;
}

impl std::fmt::Debug for dyn MimeCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MimeCheck({})", self.describe())
    }
}

/// A check built from a closure, for embedders and tests.
pub struct FnCheck<F> {
    name: String,
    f: F,
}

impl<F> FnCheck<F>
where
    F: Fn(&MimeType, &[u8]) -> Result<(), String> + Send + Sync,
{
    /// A check called `name` that answers with `f`.
    pub fn new(name: impl Into<String>, f: F) -> Self {
        Self {
            name: name.into(),
            f,
        }
    }
}

impl<F> MimeCheck for FnCheck<F>
where
    F: Fn(&MimeType, &[u8]) -> Result<(), String> + Send + Sync,
{
    fn check(&self, mime_type: &MimeType, bytes: &[u8]) -> Result<(), String> {
        (self.f)(mime_type, bytes)
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closure_check_answers_and_names_itself() {
        let check = FnCheck::new("starts-with-a", |t: &MimeType, bytes: &[u8]| {
            match bytes.first() {
                Some(b'a') => Ok(()),
                _ => Err(format!("{t} bytes must start with a")),
            }
        });
        let t = MimeType::parse("text/x-a").unwrap();
        assert_eq!(check.check(&t, b"abc"), Ok(()));
        assert_eq!(
            check.check(&t, b"xyz"),
            Err("text/x-a bytes must start with a".to_string())
        );
        let boxed: std::sync::Arc<dyn MimeCheck> = std::sync::Arc::new(check);
        assert_eq!(format!("{boxed:?}"), "MimeCheck(starts-with-a)");
    }
}
