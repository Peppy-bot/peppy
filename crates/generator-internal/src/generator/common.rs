use crate::error::{Error, Result};
use askama::Template;
use rust_embed::Embed;
use std::{
    fs,
    io::{self, ErrorKind},
    path::Path,
};

#[derive(Embed)]
#[folder = "templates/"]
struct EmbeddedTemplates;

#[derive(Template)]
#[template(path = "peppygen/python/pyproject.toml.j2", escape = "none")]
struct PeppyPythonConfigTemplate<'a> {
    python_min_version: &'a str,
    python_max_version: &'a str,
}

impl PeppyPythonConfigTemplate<'_> {
    pub const TEMPLATE_PATH: &'static str = "peppygen/python/pyproject.toml.j2";
}

pub(crate) fn copy_embedded_templates(prefix: &str, to: &Path) -> Result<()> {
    let prefix_with_slash = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{}/", prefix)
    };

    for file_path in EmbeddedTemplates::iter() {
        let file_path_str = file_path.as_ref();

        // Only process files under the specified prefix
        let Some(relative_path) = file_path_str.strip_prefix(&prefix_with_slash) else {
            continue;
        };

        // Skip .gitkeep files
        if relative_path.ends_with(".gitkeep") {
            continue;
        }

        let destination = if relative_path.ends_with(".j2") {
            // Remove the .j2 extension for template files
            let without_ext = relative_path.strip_suffix(".j2").unwrap_or(relative_path);
            to.join(without_ext)
        } else {
            to.join(relative_path)
        };

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        if relative_path.ends_with(".j2") {
            // Render template files
            let rendered = render_template(file_path_str)?;
            fs::write(&destination, rendered)?;
        } else {
            // Copy regular files
            let content = EmbeddedTemplates::get(file_path_str).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("embedded template not found: {file_path_str}"),
                )
            })?;
            fs::write(&destination, content.data.as_ref())?;
        }
    }

    Ok(())
}

fn render_template(template_path: &str) -> Result<String> {
    match template_path {
        PeppyPythonConfigTemplate::TEMPLATE_PATH => {
            let tpl = PeppyPythonConfigTemplate {
                python_min_version: config::consts::PYTHON_MIN_VERSION,
                python_max_version: config::consts::PYTHON_MAX_VERSION,
            };
            Ok(tpl.render()?)
        }
        _ => Err(Error::UnknownTemplate(template_path.to_string())),
    }
}
