use crate::{
    error::{Error, Result},
    generator::types::InterfaceArtifact,
};
use askama::Template;
use std::{fs, path::Path};

#[derive(Template)]
#[template(path = "peppygen/rust/Cargo.toml.j2", escape = "none")]
struct PeppyConfigTemplate<'a> {
    peppylib_version: &'a str,
}

impl<'a> PeppyConfigTemplate<'a> {
    pub const TEMPLATE_PATH: &'static str = "peppygen/rust/Cargo.toml.j2";
}

pub fn generate_lib_structure(to_path: impl AsRef<Path>) -> Result<()> {
    let templates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let template_root = templates_dir.join("peppygen/rust");
    let to_path = to_path.as_ref();

    fs::create_dir_all(to_path)?;

    copy_templates(&templates_dir, &template_root, to_path)
}

pub fn add_artifacts_to_lib(
    lib_path: impl AsRef<Path>,
    artifacts: Vec<InterfaceArtifact>,
) -> Result<()> {
    // TODO: add the generated artifacts in the correct modules:
    // - peppygen::topics::<node_name>::<topic_name>(<topic_parameters>)
    // For example: peppygen::topics::uvc_camera::push_frame(header, encoding, width, height, image);
    // where:
    // - `peppygen` is the name of the crate
    // - `topics` because it's a topic (InterfaceKind::ExposedTopic and InterfaceKind::SubscribedTopic)
    // - `uvc_camera` is the name of the node that emits this topic (InterfaceArtifact::node_name)
    // - `push_frame(header, encoding, width, height, image)` is coming from InterfaceArtifact::code_output
    Ok(())
}

fn copy_templates(root: &Path, from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        let destination = to.join(entry.file_name());

        if file_type.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_templates(root, &path, &destination)?;
        } else if file_type.is_file() {
            let ext = path.extension().and_then(|ext| ext.to_str());
            if ext == Some(".gitkeep") {
                continue;
            } else if ext == Some("j2") {
                let rendered = render_template(root, &path)?;
                let destination = destination.with_extension("");

                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }

                fs::write(&destination, rendered)?;
                continue;
            }

            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }

            fs::copy(&path, &destination)?;
        }
    }

    Ok(())
}

fn render_template(root: &Path, template_path: &Path) -> Result<String> {
    let relative = template_path
        .strip_prefix(root)
        .unwrap_or(template_path)
        .to_string_lossy()
        .into_owned();

    match relative.as_str() {
        PeppyConfigTemplate::TEMPLATE_PATH => {
            let tpl = PeppyConfigTemplate {
                peppylib_version: env!("CARGO_PKG_VERSION"),
            };
            Ok(tpl.render()?)
        }
        _ => Err(Error::UnknownTemplate(relative)),
    }
}
