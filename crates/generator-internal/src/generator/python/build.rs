use crate::error::Result;
use crate::generator::types::CapnpSchema;
use std::collections::HashMap;
use std::path::Path;

pub fn write_capnp_schemas(schemas: &HashMap<String, CapnpSchema>, to_path: &Path) -> Result<()> {
    if schemas.is_empty() {
        return Ok(());
    }

    let capnp_dir = to_path.join("capnp");
    std::fs::create_dir_all(&capnp_dir)?;
    for schema in schemas.values() {
        let file_path = capnp_dir.join(format!("{}.capnp", schema.file_stem()));
        std::fs::write(&file_path, schema.schema())?;
    }

    Ok(())
}

pub fn write_parameters(parameters: &config::NodeArguments, to_path: &Path) -> Result<()> {
    let parameters_code = super::parameters::generate_python_parameters(parameters)?;
    let parameters_file = to_path.join("parameters.py");
    std::fs::write(&parameters_file, parameters_code)?;
    Ok(())
}
