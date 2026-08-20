use super::type_mapping::{NestedDataclass, collect_fields_from_format, uses_optional};
use crate::error::Result;
use config::node::{MessageFormat, SchemaType, TypeToken};
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
    /// Not `lines`: that name is taken by the [`PythonCodeBuilder::lines`]
    /// helper, and a field shadowed by a method of the same name reads as a
    /// bug even where the compiler is happy to tell them apart.
    body: Vec<String>,
    indent_level: usize,
}

impl PythonCodeBuilder {
    pub fn new() -> Self {
        Self {
            imports: BTreeSet::new(),
            body: Vec::new(),
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
        self.body.push(format!("{indent}{content}"));
    }

    /// Appends an empty line (no indentation).
    pub fn blank_line(&mut self) {
        self.body.push(String::new());
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

    /// Emits `header`, runs `body` one level deeper, then dedents.
    ///
    /// Exactly the sequence it replaces — `line(header); indent(); body;
    /// dedent();` — so not a byte of output moves. What it buys is that the
    /// pair can no longer be left unbalanced: written by hand, a `dedent()`
    /// ends up dozens of lines below its `indent()`, and a suite closed with a
    /// bare `dedent(); dedent();` is only checkable by counting upwards.
    ///
    /// Generic over the body's result so a block can propagate an error with
    /// `?` at the call site, or hand back a value it computed while emitting.
    pub fn block<R>(&mut self, header: &str, body: impl FnOnce(&mut Self) -> R) -> R {
        self.line(header);
        self.indent();
        let outcome = body(self);
        self.dedent();
        outcome
    }

    /// Appends every line at the current indentation level.
    ///
    /// Takes both a static `&["...", "..."]` run and the pre-assembled
    /// `Vec<String>` line lists the harness renderer builds ahead of emission.
    /// Relative indentation those lists bake into their strings as literal
    /// spaces is preserved verbatim: `line("    x")` at level N writes the same
    /// bytes as `indent(); line("x")`.
    pub fn lines<S: AsRef<str>>(&mut self, contents: impl IntoIterator<Item = S>) {
        for content in contents {
            self.line(content.as_ref());
        }
    }

    /// A call whose arguments each occupy a line: `open`, the arguments one
    /// level deeper, then `close` back at the original level.
    ///
    /// A conditional opening line is an `if` expression in argument position,
    /// which keeps the two arms from each carrying their own copy of the
    /// argument list.
    pub fn call(&mut self, open: &str, args: &[&str], close: &str) {
        self.block(open, |builder| builder.lines(args));
        self.line(close);
    }

    /// A one-line docstring: `"""text"""`.
    ///
    /// Beyond dropping the `\"\"\"` escaping, this keeps long docstrings
    /// wrapped: a docstring is by far the most common emitted line over 100
    /// characters, and unlike text inside a [`Self::py`] block a `&str`
    /// argument can still use Rust's `\` line continuation.
    pub fn docstring(&mut self, text: &str) {
        self.line(&format!("\"\"\"{text}\"\"\""));
    }

    /// Appends a block of literal Python, written as a Rust raw string.
    ///
    /// The text must open with a newline (`r#"` followed by a line break). The
    /// block's own common indentation is stripped, so the source can sit at
    /// column zero and what survives is the *relative* structure of the
    /// Python; every line is then emitted through [`Self::line`] at the
    /// builder's current level, and empty lines through [`Self::blank_line`].
    /// The Rust source ends up looking like the Python it emits.
    ///
    /// Deliberately takes no interpolation. The emitted Python is full of
    /// f-strings whose literal braces would every one need `{{`/`}}` doubling
    /// under `format!` — precisely in the lines whose readability matters most
    /// — and a placeholder syntax of our own would trade `format!`'s
    /// compile-time argument checking for a runtime panic. Anything dynamic
    /// stays on [`Self::line`] with `format!`; `py` is for the static runs,
    /// which is most of what these renderers emit.
    ///
    /// A block leaves `indent_level` untouched, so it must be
    /// indentation-balanced the way a Python suite is; a suite that stays open
    /// across dynamic emission keeps its explicit [`Self::block`].
    ///
    /// Panics on a tab or on trailing whitespace. Both are invisible in a Rust
    /// source file and would silently rewrite the generated Python, so they are
    /// rejected rather than repaired — which also means a blank line inside a
    /// block must be genuinely empty, not indented to match its neighbours.
    pub fn py(&mut self, source: &str) {
        let mut source_lines: Vec<&str> = source.split('\n').collect();
        assert!(
            source_lines.first().is_some_and(|first| first.is_empty()),
            "a `py` block must open with a newline directly after `r#\"`"
        );
        source_lines.remove(0);
        // The closing `"#` is written on its own line, at whatever indentation
        // the surrounding Rust sits at; it contributes no Python.
        if source_lines
            .last()
            .is_some_and(|last| last.trim().is_empty())
        {
            source_lines.pop();
        }
        // Blank lines never constrain the common indent: an editor that strips
        // them would otherwise silently change every other line's depth.
        let common = source_lines
            .iter()
            .filter(|line| !line.is_empty())
            .map(|line| line.len() - line.trim_start_matches(' ').len())
            .min()
            .unwrap_or(0);
        for source_line in source_lines {
            assert!(
                !source_line.contains('\t'),
                "a `py` block must not contain tabs: {source_line:?}"
            );
            assert_eq!(
                source_line.trim_end(),
                source_line,
                "a `py` block line carries trailing whitespace: {source_line:?}"
            );
            if source_line.is_empty() {
                self.blank_line();
            } else {
                self.line(&source_line[common..]);
            }
        }
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
        result.extend(self.body);
        result.join("\n")
    }
}

/// The Python container a `$length`-bearing array reads and writes as, and
/// the word its length-check message names: `bytes` for `u8` items, `list`
/// for everything else. One mapping, so the reader and the writer cannot
/// disagree about what they are counting.
pub fn container_name(items: &SchemaType) -> &'static str {
    match items.as_type_token() {
        Some(TypeToken::U8) => "bytes",
        _ => "list",
    }
}

/// Emits the `ValueError` raised when `var` does not hold exactly `len`
/// items; `container` names it in the message (`list` or `bytes`).
pub fn emit_fixed_length_check(
    builder: &mut PythonCodeBuilder,
    var: &str,
    field_name: &str,
    container: &str,
    len: usize,
) {
    builder.block(&format!("if len({var}) != {len}:"), |b| {
        b.line(&format!(
            "raise ValueError(\"invalid fixed {container} length for field '{field_name}': expected {len}, got \" + str(len({var})))"
        ));
    });
}

#[cfg(test)]
mod tests {
    use super::PythonCodeBuilder;

    /// `block` is the `line`/`indent`/`dedent` sequence it replaces, byte for
    /// byte, and hands back what its body computed.
    #[test]
    fn block_emits_the_header_then_the_body_one_level_deeper() {
        let mut builder = PythonCodeBuilder::new();
        let returned = builder.block("def f():", |b| {
            b.line("return 1");
            "computed"
        });
        assert_eq!(returned, "computed");
        builder.line("f()");

        let mut expected = PythonCodeBuilder::new();
        expected.line("def f():");
        expected.indent();
        expected.line("return 1");
        expected.dedent();
        expected.line("f()");

        assert_eq!(builder.build(), expected.build());
    }

    /// The pre-assembled line lists bake relative indentation in as literal
    /// spaces; `lines` must pass those through untouched.
    #[test]
    fn lines_preserves_baked_in_indentation_at_the_current_level() {
        let mut builder = PythonCodeBuilder::new();
        builder.indent();
        builder.lines(vec!["if ready:".to_string(), "    go()".to_string()]);
        assert_eq!(builder.build(), "    if ready:\n        go()");
    }

    #[test]
    fn call_closes_at_the_level_it_opened_on() {
        let mut builder = PythonCodeBuilder::new();
        builder.call("send(", &["first,", "second,"], ")");
        builder.line("after");
        assert_eq!(builder.build(), "send(\n    first,\n    second,\n)\nafter");
    }

    #[test]
    fn docstring_wraps_the_text_in_triple_quotes() {
        let mut builder = PythonCodeBuilder::new();
        builder.indent();
        builder.docstring("Does the thing.");
        assert_eq!(builder.build(), "    \"\"\"Does the thing.\"\"\"");
    }

    /// The block carries its own relative structure; the builder's level is
    /// added on top and is the same before and after.
    #[test]
    fn py_strips_the_common_indent_and_re_indents_at_the_current_level() {
        let mut builder = PythonCodeBuilder::new();
        builder.indent();
        builder.py(r#"
            def f():
                if ready:
                    return 1
                return 0
        "#);
        builder.line("after");
        assert_eq!(
            builder.build(),
            "    def f():\n        if ready:\n            return 1\n        return 0\n    after"
        );
    }

    /// A blank line inside a block is a real blank line, never an indented one:
    /// that is what `blank_line` writes, and what ruff would otherwise flag.
    #[test]
    fn py_emits_blank_lines_with_no_indentation() {
        let mut builder = PythonCodeBuilder::new();
        builder.indent();
        builder.py(r#"
            class C:
                x = 1

                y = 2
        "#);
        assert_eq!(
            builder.build(),
            "    class C:\n        x = 1\n\n        y = 2"
        );
    }

    /// The deepest line, not the first, would otherwise set the baseline.
    #[test]
    fn py_takes_the_common_indent_from_the_shallowest_line() {
        let mut builder = PythonCodeBuilder::new();
        builder.py(r#"
                nested = True
        if nested:
            pass
        "#);
        assert_eq!(
            builder.build(),
            "        nested = True\nif nested:\n    pass"
        );
    }

    #[test]
    #[should_panic(expected = "must open with a newline")]
    fn py_rejects_text_that_does_not_open_with_a_newline() {
        PythonCodeBuilder::new().py("pass\n");
    }

    #[test]
    #[should_panic(expected = "trailing whitespace")]
    fn py_rejects_trailing_whitespace() {
        PythonCodeBuilder::new().py("\npass \n");
    }

    /// An indented "blank" line is trailing whitespace too, and is rejected for
    /// the same reason: it is invisible in the Rust source but decides bytes in
    /// the generated Python.
    #[test]
    #[should_panic(expected = "trailing whitespace")]
    fn py_rejects_an_indented_blank_line() {
        PythonCodeBuilder::new().py("\nx = 1\n    \ny = 2\n");
    }

    #[test]
    #[should_panic(expected = "must not contain tabs")]
    fn py_rejects_tabs() {
        PythonCodeBuilder::new().py("\nif x:\n\tpass\n");
    }
}
