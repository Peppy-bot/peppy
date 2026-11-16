//! The compression of the squashfs of an image.
//!
//! An image is a SIF file: a header, a table of descriptors, and the data
//! objects that the descriptors name. One data object is the primary system
//! partition: the squashfs that apptainer mounts as the root file system of
//! the container. The superblock at the start of a squashfs holds the id of
//! the compression that packs it.
//!
//! Each number of a SIF file and of a squashfs superblock is little-endian.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The size of the header of a SIF file.
const SIF_HEADER_SIZE: usize = 128;

/// The bytes that mark a SIF file.
const SIF_MAGIC: &[u8] = b"SIF_MAGIC\0";

/// Where the header holds [`SIF_MAGIC`], after the launch script.
const SIF_MAGIC_OFFSET: usize = 32;

/// Where the header holds the count of the descriptors of the table.
const SIF_DESCRIPTOR_COUNT_OFFSET: usize = 88;

/// Where the header holds the offset of the descriptor table in the file.
const SIF_DESCRIPTOR_TABLE_OFFSET: usize = 96;

/// The size of one descriptor of the table.
const SIF_DESCRIPTOR_SIZE: usize = 585;

/// Where a descriptor holds the type of its data object.
const DESCRIPTOR_DATA_TYPE_OFFSET: usize = 0;

/// The data type of a partition: a file system.
const DATA_TYPE_PARTITION: i32 = 0x4004;

/// Where a descriptor holds the byte that tells whether it is in use.
const DESCRIPTOR_USED_OFFSET: usize = 4;

/// Where a descriptor holds the offset of its data object in the file.
const DESCRIPTOR_DATA_OFFSET_OFFSET: usize = 17;

/// Where the descriptor of a partition holds the type of its file system.
const PARTITION_FILE_SYSTEM_OFFSET: usize = 201;

/// The file system type of a squashfs.
const FILE_SYSTEM_SQUASHFS: i32 = 1;

/// Where the descriptor of a partition holds the type of the partition.
const PARTITION_TYPE_OFFSET: usize = 205;

/// The partition type of the primary system partition: the root file system
/// of the container.
const PARTITION_PRIMARY_SYSTEM: i32 = 2;

/// The part of a squashfs superblock that this module reads: from its start
/// to the end of the compression id.
const SQUASHFS_SUPERBLOCK_SIZE: usize = 22;

/// The bytes that a squashfs superblock starts with.
const SQUASHFS_MAGIC: &[u8] = b"hsqs";

/// Where a squashfs superblock holds the id of its compression.
const SQUASHFS_COMPRESSION_ID_OFFSET: usize = 20;

/// A compression of a squashfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SquashfsCompression {
    Gzip,
    Lzma,
    Lzo,
    Xz,
    Lz4,
    Zstd,
}

impl SquashfsCompression {
    /// Each compression that a squashfs superblock can name.
    pub(crate) const ALL: [Self; 6] = [
        Self::Gzip,
        Self::Lzma,
        Self::Lzo,
        Self::Xz,
        Self::Lz4,
        Self::Zstd,
    ];

    /// The id of this compression in a squashfs superblock.
    fn id(self) -> u16 {
        match self {
            Self::Gzip => 1,
            Self::Lzma => 2,
            Self::Lzo => 3,
            Self::Xz => 4,
            Self::Lz4 => 5,
            Self::Zstd => 6,
        }
    }

    /// The name of this compression for `mksquashfs -comp`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Lzma => "lzma",
            Self::Lzo => "lzo",
            Self::Xz => "xz",
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        }
    }

    /// The compression of the root file system of the image at `image`.
    ///
    /// `None` for an image that tells none: a file that cannot be read, a
    /// file that is not a SIF file (an image of a registry, a sandbox
    /// directory), a SIF file without a squashfs as its primary system
    /// partition, and a compression id that this module does not know.
    pub(crate) fn of_image(image: &Path) -> Option<Self> {
        Self::of_sif(&mut File::open(image).ok()?)
    }

    /// [`SquashfsCompression::of_image`] for the bytes of an image.
    fn of_sif(sif: &mut (impl Read + Seek)) -> Option<Self> {
        let header: [u8; SIF_HEADER_SIZE] = read_at(sif, 0)?;
        let magic = header.get(SIF_MAGIC_OFFSET..SIF_MAGIC_OFFSET + SIF_MAGIC.len())?;
        if magic != SIF_MAGIC {
            return None;
        }
        let descriptor_count = file_offset_at(&header, SIF_DESCRIPTOR_COUNT_OFFSET)?;
        let table_offset = file_offset_at(&header, SIF_DESCRIPTOR_TABLE_OFFSET)?;
        let descriptor_size = u64::try_from(SIF_DESCRIPTOR_SIZE).ok()?;
        // A descriptor that cannot be read ends the search: the file is
        // shorter than its header says.
        for index in 0..descriptor_count {
            let descriptor_offset =
                table_offset.checked_add(index.checked_mul(descriptor_size)?)?;
            let descriptor: [u8; SIF_DESCRIPTOR_SIZE] = read_at(sif, descriptor_offset)?;
            if let Some(squashfs_offset) = primary_squashfs_offset(&descriptor) {
                return Self::of_squashfs_at(sif, squashfs_offset);
            }
        }
        None
    }

    /// The compression that the superblock of the squashfs at `offset` of
    /// `file` names.
    fn of_squashfs_at(file: &mut (impl Read + Seek), offset: u64) -> Option<Self> {
        let superblock: [u8; SQUASHFS_SUPERBLOCK_SIZE] = read_at(file, offset)?;
        if !superblock.starts_with(SQUASHFS_MAGIC) {
            return None;
        }
        let id = u16::from_le_bytes(bytes_at(&superblock, SQUASHFS_COMPRESSION_ID_OFFSET)?);
        Self::ALL
            .into_iter()
            .find(|compression| compression.id() == id)
    }
}

/// The offset in the file of the squashfs that `descriptor` names, or `None`
/// when the descriptor is not the one of a primary system partition that is
/// a squashfs.
fn primary_squashfs_offset(descriptor: &[u8]) -> Option<u64> {
    let is_used = *descriptor.get(DESCRIPTOR_USED_OFFSET)? != 0;
    let data_type = i32::from_le_bytes(bytes_at(descriptor, DESCRIPTOR_DATA_TYPE_OFFSET)?);
    let file_system = i32::from_le_bytes(bytes_at(descriptor, PARTITION_FILE_SYSTEM_OFFSET)?);
    let partition_type = i32::from_le_bytes(bytes_at(descriptor, PARTITION_TYPE_OFFSET)?);
    let is_primary_squashfs = is_used
        && data_type == DATA_TYPE_PARTITION
        && file_system == FILE_SYSTEM_SQUASHFS
        && partition_type == PARTITION_PRIMARY_SYSTEM;
    if !is_primary_squashfs {
        return None;
    }
    file_offset_at(descriptor, DESCRIPTOR_DATA_OFFSET_OFFSET)
}

/// The `N` bytes of `bytes` from `offset`, or `None` when `bytes` ends
/// before them.
fn bytes_at<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset + N)?.try_into().ok()
}

/// The count or the file offset that `bytes` holds at `offset` as a signed
/// 64-bit number, or `None` when the number is negative.
fn file_offset_at(bytes: &[u8], offset: usize) -> Option<u64> {
    u64::try_from(i64::from_le_bytes(bytes_at(bytes, offset)?)).ok()
}

/// The `N` bytes of `file` from `offset`, or `None` when the file ends
/// before them or cannot be read.
fn read_at<const N: usize>(file: &mut (impl Read + Seek), offset: u64) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// The bytes of image files for tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// Where the fixtures put the descriptor table: the offset that
    /// apptainer 1.5.5 writes.
    const TABLE_OFFSET: usize = 4096;

    /// The count of descriptors in the table of the fixtures. Apptainer
    /// 1.5.5 writes 48.
    const DESCRIPTOR_COUNT: usize = 4;

    /// Where the fixtures put the squashfs: after the table.
    const SQUASHFS_OFFSET: usize = TABLE_OFFSET + DESCRIPTOR_COUNT * SIF_DESCRIPTOR_SIZE;

    /// The data type of a definition file, the first data object of an
    /// image that apptainer builds.
    const DATA_TYPE_DEFINITION_FILE: i32 = 0x4001;

    fn write_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_offset(bytes: &mut [u8], offset: usize, value: usize) {
        let value = i64::try_from(value).expect("a fixture offset fits in 64 bits");
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// The first bytes of a squashfs whose superblock names the compression
    /// `id`.
    pub(crate) fn squashfs_superblock(id: u16) -> Vec<u8> {
        let mut superblock = vec![0; SQUASHFS_SUPERBLOCK_SIZE];
        superblock[..SQUASHFS_MAGIC.len()].copy_from_slice(SQUASHFS_MAGIC);
        superblock[SQUASHFS_COMPRESSION_ID_OFFSET..].copy_from_slice(&id.to_le_bytes());
        superblock
    }

    /// The bytes of a SIF file as apptainer builds it: a definition file,
    /// then a partition of `file_system` and `partition_type` whose data is
    /// `partition_data`.
    pub(crate) fn sif_with_partition(
        file_system: i32,
        partition_type: i32,
        partition_data: &[u8],
    ) -> Vec<u8> {
        let mut sif = vec![0; SQUASHFS_OFFSET];
        sif[SIF_MAGIC_OFFSET..SIF_MAGIC_OFFSET + SIF_MAGIC.len()].copy_from_slice(SIF_MAGIC);
        write_offset(&mut sif, SIF_DESCRIPTOR_COUNT_OFFSET, DESCRIPTOR_COUNT);
        write_offset(&mut sif, SIF_DESCRIPTOR_TABLE_OFFSET, TABLE_OFFSET);

        let definition_file = TABLE_OFFSET;
        write_i32(
            &mut sif,
            definition_file + DESCRIPTOR_DATA_TYPE_OFFSET,
            DATA_TYPE_DEFINITION_FILE,
        );
        sif[definition_file + DESCRIPTOR_USED_OFFSET] = 1;
        write_offset(
            &mut sif,
            definition_file + DESCRIPTOR_DATA_OFFSET_OFFSET,
            SIF_HEADER_SIZE,
        );

        let partition = TABLE_OFFSET + SIF_DESCRIPTOR_SIZE;
        write_i32(
            &mut sif,
            partition + DESCRIPTOR_DATA_TYPE_OFFSET,
            DATA_TYPE_PARTITION,
        );
        sif[partition + DESCRIPTOR_USED_OFFSET] = 1;
        write_offset(
            &mut sif,
            partition + DESCRIPTOR_DATA_OFFSET_OFFSET,
            SQUASHFS_OFFSET,
        );
        write_i32(
            &mut sif,
            partition + PARTITION_FILE_SYSTEM_OFFSET,
            file_system,
        );
        write_i32(&mut sif, partition + PARTITION_TYPE_OFFSET, partition_type);

        sif.extend_from_slice(partition_data);
        sif
    }

    /// The bytes of an image that apptainer builds with `compression`.
    pub(crate) fn sif_packed_with(compression: SquashfsCompression) -> Vec<u8> {
        sif_with_partition(
            FILE_SYSTEM_SQUASHFS,
            PARTITION_PRIMARY_SYSTEM,
            &squashfs_superblock(compression.id()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{sif_packed_with, sif_with_partition, squashfs_superblock};
    use super::*;
    use crate::apptainer::scratch_dir;
    use std::fs;
    use std::io::Cursor;

    /// The file system type of an ext3 image.
    const FILE_SYSTEM_EXT3: i32 = 2;

    /// The partition type of a data partition.
    const PARTITION_DATA: i32 = 3;

    fn compression_of(sif: Vec<u8>) -> Option<SquashfsCompression> {
        SquashfsCompression::of_sif(&mut Cursor::new(sif))
    }

    #[test]
    fn an_image_tells_the_compression_of_its_root_file_system() {
        for compression in SquashfsCompression::ALL {
            assert_eq!(
                compression_of(sif_packed_with(compression)),
                Some(compression),
                "an image packed with {}",
                compression.name()
            );
        }
    }

    /// The ids are those of the squashfs format, which `mksquashfs` writes.
    #[test]
    fn the_superblock_names_gzip_with_1_and_zstd_with_6() {
        let gzip = sif_with_partition(
            FILE_SYSTEM_SQUASHFS,
            PARTITION_PRIMARY_SYSTEM,
            &squashfs_superblock(1),
        );
        let zstd = sif_with_partition(
            FILE_SYSTEM_SQUASHFS,
            PARTITION_PRIMARY_SYSTEM,
            &squashfs_superblock(6),
        );

        assert_eq!(compression_of(gzip), Some(SquashfsCompression::Gzip));
        assert_eq!(compression_of(zstd), Some(SquashfsCompression::Zstd));
    }

    #[test]
    fn a_compression_id_that_is_not_known_tells_no_compression() {
        let sif = sif_with_partition(
            FILE_SYSTEM_SQUASHFS,
            PARTITION_PRIMARY_SYSTEM,
            &squashfs_superblock(7),
        );

        assert_eq!(compression_of(sif), None);
    }

    #[test]
    fn a_file_that_is_not_a_sif_file_tells_no_compression() {
        let bare_squashfs = squashfs_superblock(6);
        let text = b"Bootstrap: docker\nFrom: ubuntu:24.04\n".to_vec();

        assert_eq!(compression_of(bare_squashfs), None);
        assert_eq!(compression_of(text), None);
        assert_eq!(compression_of(Vec::new()), None);
    }

    #[test]
    fn a_partition_that_is_not_a_squashfs_tells_no_compression() {
        let ext3 = sif_with_partition(
            FILE_SYSTEM_EXT3,
            PARTITION_PRIMARY_SYSTEM,
            &squashfs_superblock(6),
        );
        let without_squashfs_magic =
            sif_with_partition(FILE_SYSTEM_SQUASHFS, PARTITION_PRIMARY_SYSTEM, &[0; 64]);

        assert_eq!(compression_of(ext3), None);
        assert_eq!(compression_of(without_squashfs_magic), None);
    }

    #[test]
    fn a_partition_that_is_not_the_primary_system_tells_no_compression() {
        let data_partition = sif_with_partition(
            FILE_SYSTEM_SQUASHFS,
            PARTITION_DATA,
            &squashfs_superblock(6),
        );

        assert_eq!(compression_of(data_partition), None);
    }

    #[test]
    fn an_image_that_ends_before_its_squashfs_tells_no_compression() {
        let mut cut_in_the_superblock = sif_packed_with(SquashfsCompression::Zstd);
        cut_in_the_superblock.truncate(cut_in_the_superblock.len() - 1);
        let mut cut_in_the_table = sif_packed_with(SquashfsCompression::Zstd);
        cut_in_the_table.truncate(SIF_HEADER_SIZE + 1);

        assert_eq!(compression_of(cut_in_the_superblock), None);
        assert_eq!(compression_of(cut_in_the_table), None);
    }

    /// A header with a descriptor count that the file does not hold ends the
    /// search at the end of the file.
    #[test]
    fn a_header_with_more_descriptors_than_the_file_holds_tells_no_compression() {
        let mut sif = sif_with_partition(FILE_SYSTEM_EXT3, PARTITION_PRIMARY_SYSTEM, &[]);
        sif[SIF_DESCRIPTOR_COUNT_OFFSET..SIF_DESCRIPTOR_COUNT_OFFSET + 8]
            .copy_from_slice(&i64::MAX.to_le_bytes());

        assert_eq!(compression_of(sif), None);
    }

    #[test]
    fn a_header_with_a_negative_offset_tells_no_compression() {
        let mut sif = sif_packed_with(SquashfsCompression::Zstd);
        sif[SIF_DESCRIPTOR_TABLE_OFFSET..SIF_DESCRIPTOR_TABLE_OFFSET + 8]
            .copy_from_slice(&(-1_i64).to_le_bytes());

        assert_eq!(compression_of(sif), None);
    }

    #[test]
    fn the_compression_of_an_image_file_is_read() {
        let scratch = scratch_dir();
        let image = scratch.path().join("node.sif");
        fs::write(&image, sif_packed_with(SquashfsCompression::Xz)).expect("write the image");

        assert_eq!(
            SquashfsCompression::of_image(&image),
            Some(SquashfsCompression::Xz)
        );
    }

    #[test]
    fn an_image_that_is_not_a_file_tells_no_compression() {
        let scratch = scratch_dir();

        assert_eq!(SquashfsCompression::of_image(scratch.path()), None);
        assert_eq!(
            SquashfsCompression::of_image(&scratch.path().join("no-such-image.sif")),
            None
        );
        assert_eq!(
            SquashfsCompression::of_image(Path::new("docker://alpine")),
            None
        );
    }
}
