use crate::error::{ParsingError, Result};
use std::{fs, path::Path};

pub(crate) fn read_non_empty_file(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path)
        .map_err(|e| ParsingError::CannotRead(path.display().to_string(), e.kind()))?;

    if content.trim().is_empty() {
        Err(ParsingError::EmptyContent(path.display().to_string()).into())
    } else {
        Ok(content)
    }
}
