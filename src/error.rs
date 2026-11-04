//! Python exceptions surfaced as Rust / MiniJinja errors.

use std::fmt;

use minijinja::{Error, ErrorKind};
use monty_types::MontyException;

/// A Python exception raised inside the sandbox, with a CPython-style
/// traceback whose line numbers refer to the user's own source.
#[derive(Debug, Clone)]
pub struct PythonError {
    exception: MontyException,
    /// Script name whose frames are shifted by `line_offset`.
    script: String,
    /// Lines of generated prelude before the user's source in `script`.
    line_offset: u32,
    /// Frames of `script` past this (user) line are generated dispatch code
    /// and are left out of the traceback.
    last_user_line: Option<u32>,
}

impl PythonError {
    pub(crate) fn new(exception: MontyException, script: &str, line_offset: u32) -> Self {
        Self {
            exception,
            script: script.to_owned(),
            line_offset,
            last_user_line: None,
        }
    }

    pub(crate) fn hide_frames_after(mut self, last_user_line: u32) -> Self {
        self.last_user_line = Some(last_user_line);
        self
    }

    /// The underlying Monty exception (unadjusted line numbers).
    pub fn exception(&self) -> &MontyException {
        &self.exception
    }

    /// `ExcType: message`, as the last line of a Python traceback shows it.
    pub fn summary(&self) -> String {
        self.exception.summary()
    }

    /// The full traceback, CPython style.
    pub fn traceback(&self) -> String {
        let mut out = String::new();
        let frames = self.exception.traceback();
        if !frames.is_empty() {
            out.push_str("Traceback (most recent call last):\n");
        }
        for frame in frames {
            let mut line = frame.start.line;
            if frame.filename == self.script {
                line = line.saturating_sub(self.line_offset);
                if self.last_user_line.is_some_and(|last| line > last) {
                    continue;
                }
            }
            out.push_str(&format!("  File \"{}\", line {line}", frame.filename));
            if let Some(name) = &frame.frame_name {
                out.push_str(&format!(", in {name}"));
            }
            out.push('\n');
            if let Some(preview) = &frame.preview_line {
                out.push_str(&format!("    {}\n", preview.trim()));
            }
        }
        out.push_str(&self.summary());
        out
    }

    pub(crate) fn into_template_error(self, context: &str) -> Error {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("{context}: {}", self.summary()),
        )
        .with_source(self)
    }
}

impl fmt::Display for PythonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.traceback())
    }
}

impl std::error::Error for PythonError {}

/// Failure to load a Python module of template extensions.
#[derive(Debug)]
pub enum LoadError {
    /// The module raised while executing its top level.
    Python(PythonError),
    /// The module's registrations are unusable (e.g. an anonymous lambda
    /// registered without an explicit name).
    Invalid(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Python(err) => write!(f, "python module failed to load:\n{err}"),
            Self::Invalid(msg) => write!(f, "invalid python module: {msg}"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Python(err) => Some(err),
            Self::Invalid(_) => None,
        }
    }
}
