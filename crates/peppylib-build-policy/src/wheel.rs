//! Reading peppylib's native extension out of a maturin wheel.
//!
//! The macOS build cross-compiles the Linux extensions with maturin, which
//! packs each one into a wheel: a zip archive that holds the extension at
//! `peppylib/_peppylib.abi3.so`. The build script embeds the bytes that
//! [`native_extension_of_wheel`] reads. Only a macOS build script calls it, and
//! it lives here so that the Linux hosts of the CI compile and test it too.

use std::io::{Read, Seek};

/// The file name of peppylib's native extension inside a wheel.
const NATIVE_EXTENSION_FILE_NAME: &str = "_peppylib.abi3.so";

/// Why a wheel gives no native extension.
#[derive(Debug)]
pub enum WheelError {
    /// The wheel is not a zip archive that can be read, or the name or the
    /// header of one of its entries cannot be read.
    Archive(zip::result::ZipError),
    /// No entry of the wheel is the native extension.
    NoNativeExtension,
    /// More than one entry of the wheel is a native extension, so the one to
    /// embed is not known. Holds the names of those entries.
    SeveralNativeExtensions(Vec<String>),
    /// The native extension entry cannot be decompressed.
    Read(std::io::Error),
}

impl std::fmt::Display for WheelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Archive(e) => write!(f, "the wheel cannot be read: {e}"),
            Self::NoNativeExtension => {
                write!(f, "the wheel holds no {NATIVE_EXTENSION_FILE_NAME}")
            }
            Self::SeveralNativeExtensions(names) => write!(
                f,
                "the wheel holds more than one {NATIVE_EXTENSION_FILE_NAME}: {}",
                names.join(", ")
            ),
            Self::Read(e) => write!(f, "the native extension cannot be decompressed: {e}"),
        }
    }
}

impl std::error::Error for WheelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Archive(e) => Some(e),
            Self::Read(e) => Some(e),
            Self::NoNativeExtension | Self::SeveralNativeExtensions(_) => None,
        }
    }
}

/// Reads the bytes of peppylib's native extension out of `wheel`: the one
/// entry whose file name is `_peppylib.abi3.so`, in any directory of the
/// wheel.
pub fn native_extension_of_wheel<R: Read + Seek>(wheel: R) -> Result<Vec<u8>, WheelError> {
    let mut archive = zip::ZipArchive::new(wheel).map_err(WheelError::Archive)?;
    let entry_name = native_extension_entry_name(&archive)?;
    let mut entry = archive.by_name(&entry_name).map_err(WheelError::Archive)?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).map_err(WheelError::Read)?;
    Ok(bytes)
}

/// The name of the one entry of `archive` that is the native extension.
fn native_extension_entry_name<R: Read + Seek>(
    archive: &zip::ZipArchive<R>,
) -> Result<String, WheelError> {
    let entry_names = archive
        .file_names()
        .collect::<Result<Vec<_>, _>>()
        .map_err(WheelError::Archive)?;
    let extension_names: Vec<String> = entry_names
        .into_iter()
        .filter(|name| is_native_extension(name))
        .map(|name| name.into_owned())
        .collect();
    match extension_names.as_slice() {
        [] => Err(WheelError::NoNativeExtension),
        [name] => Ok(name.clone()),
        _ => Err(WheelError::SeveralNativeExtensions(extension_names)),
    }
}

/// True for an entry whose file name, the part after its last `/`, is the
/// native extension's. An entry such as `peppylib/other_peppylib.abi3.so`,
/// whose name only ends the same way, is not the native extension.
fn is_native_extension(entry_name: &str) -> bool {
    entry_name.rsplit('/').next() == Some(NATIVE_EXTENSION_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::CompressionMethod;
    use zip::write::{SimpleFileOptions, ZipWriter};

    const EXTENSION_BYTES: &[u8] = b"\x7fELF the native extension";

    /// A wheel that holds `entries` (name, contents), each one deflated as
    /// maturin deflates the entries of the wheels it packs.
    fn wheel(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, contents) in entries {
            writer
                .start_file(*name, options)
                .expect("start a wheel entry");
            writer.write_all(contents).expect("write a wheel entry");
        }
        let mut archive = writer.finish().expect("finish the wheel");
        archive.set_position(0);
        archive
    }

    #[test]
    fn reads_the_native_extension_of_a_maturin_wheel() {
        let wheel = wheel(&[
            ("peppylib/__init__.py", b"from ._peppylib import *\n"),
            ("peppylib/_peppylib.abi3.so", EXTENSION_BYTES),
            (
                "peppylib-0.0.1.dist-info/RECORD",
                b"peppylib/__init__.py,,\n",
            ),
        ]);

        let extension = native_extension_of_wheel(wheel).expect("the wheel holds the extension");

        assert_eq!(extension, EXTENSION_BYTES);
    }

    #[test]
    fn reads_the_native_extension_in_any_directory_of_the_wheel() {
        let wheel = wheel(&[("_peppylib.abi3.so", EXTENSION_BYTES)]);

        let extension = native_extension_of_wheel(wheel).expect("the wheel holds the extension");

        assert_eq!(extension, EXTENSION_BYTES);
    }

    #[test]
    fn a_wheel_without_the_native_extension_is_refused() {
        let wheel = wheel(&[("peppylib/__init__.py", b"")]);

        let error = native_extension_of_wheel(wheel).expect_err("no extension to read");

        assert!(
            matches!(error, WheelError::NoNativeExtension),
            "got: {error:?}"
        );
    }

    #[test]
    fn an_entry_whose_name_only_ends_like_the_extension_is_not_the_extension() {
        let wheel = wheel(&[("peppylib/other_peppylib.abi3.so", EXTENSION_BYTES)]);

        let error = native_extension_of_wheel(wheel).expect_err("no extension to read");

        assert!(
            matches!(error, WheelError::NoNativeExtension),
            "got: {error:?}"
        );
    }

    #[test]
    fn a_wheel_with_several_native_extensions_is_refused() {
        let wheel = wheel(&[
            ("peppylib/_peppylib.abi3.so", EXTENSION_BYTES),
            ("vendored/_peppylib.abi3.so", EXTENSION_BYTES),
        ]);

        let error = native_extension_of_wheel(wheel).expect_err("the extension is ambiguous");

        let WheelError::SeveralNativeExtensions(names) = error else {
            panic!("expected SeveralNativeExtensions, got: {error:?}");
        };
        assert_eq!(
            names,
            ["peppylib/_peppylib.abi3.so", "vendored/_peppylib.abi3.so"]
        );
    }

    #[test]
    fn a_file_that_is_not_a_zip_archive_is_refused() {
        let not_a_wheel = Cursor::new(b"not a zip archive".to_vec());

        let error = native_extension_of_wheel(not_a_wheel).expect_err("not a wheel");

        assert!(matches!(error, WheelError::Archive(_)), "got: {error:?}");
    }
}
