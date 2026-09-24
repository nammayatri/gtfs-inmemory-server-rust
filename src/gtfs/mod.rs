//! The GTFS Schedule reference as data, and the feed as GTFS: reading a zip,
//! the in-memory model of a feed, writing it back and comparing two of them.
//! docs/gtfs-editor.md section 18.
//!
//! Nothing here touches the database: the editor's tables are read and written
//! by `crate::editor`, which uses [`spec`] to know every file and field.

pub mod compare;
pub mod model;
pub mod read;
pub mod rules;
pub mod spec;
pub mod validate;
pub mod write;

use serde::Serialize;

/// Something an import or export noticed about a feed. An `error` stops a
/// seed; a `warning` says what was changed or left out on the way in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub level: Level,
    pub code: &'static str,
    /// The file it is about (`stops.txt`), when it is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The line in that file, 1 being the header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

impl Finding {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Finding {
            level: Level::Error,
            code,
            file: None,
            line: None,
            field: None,
            message: message.into(),
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Finding {
            level: Level::Warning,
            ..Finding::error(code, message)
        }
    }

    pub fn at(mut self, file: &str, line: Option<usize>, field: Option<&str>) -> Self {
        self.file = Some(file.to_string());
        self.line = line;
        self.field = field.map(str::to_string);
        self
    }
}

/// Whether any finding is an error.
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.level == Level::Error)
}
