use super::type_mapping::{NestedDataclass, collect_fields_from_format, uses_optional};
use crate::error::Result;
use config::node::MessageFormat;
use std::collections::BTreeSet;

/// Emits all nested dataclass definitions collected during field collection.
pub fn emit_nested_classes(builder: &mut PythonCodeBuilder, nested_classes: &[NestedDataclass]) {
    for class_def in nested_classes {
        let fields: Vec<(&str, &str)> = class_def
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_str.as_str()))
            .collect();
        builder.dataclass(&class_def.name, &fields);
    }
}

/// Collects fields from a message format, emits any nested dataclasses, adds
/// `Optional` import when needed, and emits the top-level dataclass.
pub fn emit_format_as_dataclass(
    builder: &mut PythonCodeBuilder,
    class_name: &str,
    format: &MessageFormat,
) -> Result<()> {
    let mut nested_classes = Vec::new();
    let fields = collect_fields_from_format(format, class_name, &mut nested_classes)?;
    if uses_optional(&fields, &nested_classes) {
        builder.add_import("from typing import Optional");
    }
    emit_nested_classes(builder, &nested_classes);
    let field_refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f.name.as_str(), f.type_str.as_str()))
        .collect();
    builder.dataclass(class_name, &field_refs);
    Ok(())
}

/// Accumulates lines of Python code with proper indentation handling.
pub struct PythonCodeBuilder {
    imports: BTreeSet<String>,
    lines: Vec<String>,
    indent_level: usize,
}

impl PythonCodeBuilder {
    pub fn new() -> Self {
        Self {
            imports: BTreeSet::new(),
            lines: Vec::new(),
            indent_level: 0,
        }
    }

    /// Registers an import line (e.g. `"import peppylib"`).
    /// Imports are deduplicated and emitted sorted at the top of the output.
    pub fn add_import(&mut self, import_line: &str) {
        self.imports.insert(import_line.to_string());
    }

    /// Appends a single line at the current indentation level.
    pub fn line(&mut self, content: &str) {
        let indent = "    ".repeat(self.indent_level);
        self.lines.push(format!("{indent}{content}"));
    }

    /// Appends an empty line (no indentation).
    pub fn blank_line(&mut self) {
        self.lines.push(String::new());
    }

    /// Increases indentation by one level.
    pub fn indent(&mut self) {
        self.indent_level += 1;
    }

    /// Decreases indentation by one level.
    pub fn dedent(&mut self) {
        assert!(self.indent_level > 0, "cannot dedent below level 0");
        self.indent_level -= 1;
    }

    /// Writes a `@dataclass` decorated class with typed fields.
    /// Automatically registers the `from dataclasses import dataclass` import.
    pub fn dataclass(&mut self, class_name: &str, fields: &[(&str, &str)]) {
        self.add_import("from dataclasses import dataclass");
        self.line("@dataclass");
        self.class_def(class_name, fields);
    }

    /// Writes a plain class (no `@dataclass` decorator).
    pub fn class_def(&mut self, class_name: &str, fields: &[(&str, &str)]) {
        self.line(&format!("class {class_name}:"));
        self.indent();
        if fields.is_empty() {
            self.line("pass");
        } else {
            for (name, ty) in fields {
                self.line(&format!("{name}: {ty}"));
            }
        }
        self.dedent();
        self.blank_line();
    }

    /// Consumes the builder and returns the final Python code string.
    /// Registered imports are emitted sorted at the top, followed by a blank line.
    pub fn build(self) -> String {
        let mut result = Vec::new();
        if !self.imports.is_empty() {
            for import in &self.imports {
                result.push(import.clone());
            }
            result.push(String::new());
        }
        result.extend(self.lines);
        result.join("\n")
    }
}
