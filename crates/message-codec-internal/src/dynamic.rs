//! Untyped access to Cap'n Proto structs and lists.
//!
//! The `capnp` crate hands out its low-level struct, list and pointer
//! accessors only through the traits its generated code implements. The
//! types here implement those traits for values whose shape is known at run
//! time only, so the codec reaches the same accessors generated code uses
//! without any generated type.

use capnp::introspect::{Introspect, Type};
use capnp::private::layout::{
    PointerBuilder, PointerReader, StructBuilder, StructReader, StructSize,
};
use capnp::struct_list;
use capnp::traits::{
    FromPointerBuilder, FromPointerReader, HasStructSize, IntoInternalStructReader, OwnedStruct,
    SetterInput,
};
use capnp::{Result, Word};
use encoding::wire_layout::StructShape;

/// The root pointer of a message, for reading.
pub(crate) struct RootReader<'a>(pub(crate) PointerReader<'a>);

impl<'a> FromPointerReader<'a> for RootReader<'a> {
    fn get_from_pointer(reader: &PointerReader<'a>, _default: Option<&'a [Word]>) -> Result<Self> {
        Ok(Self(*reader))
    }
}

/// The root pointer of a message, for writing.
pub(crate) struct RootBuilder<'a>(pub(crate) PointerBuilder<'a>);

impl<'a> FromPointerBuilder<'a> for RootBuilder<'a> {
    fn init_pointer(builder: PointerBuilder<'a>, _length: u32) -> Self {
        Self(builder)
    }

    fn get_from_pointer(builder: PointerBuilder<'a>, _default: Option<&'a [Word]>) -> Result<Self> {
        Ok(Self(builder))
    }
}

/// The element type of a struct list whose shape is only known at run time.
///
/// A struct list is sized when it is initialised, through
/// [`init_struct_list`]; the elements are then reached through
/// [`struct_list::Builder::get`] and [`struct_list::Reader::get`], which
/// expose the element builders and readers this type wraps.
pub(crate) struct DynamicStruct;

impl Introspect for DynamicStruct {
    /// The codec never introspects its own lists; the type is a placeholder
    /// the list wrappers require.
    fn introspect() -> Type {
        <() as Introspect>::introspect()
    }
}

impl OwnedStruct for DynamicStruct {
    type Reader<'a> = DynamicStructReader<'a>;
    type Builder<'a> = DynamicStructBuilder<'a>;
}

pub(crate) struct DynamicStructReader<'a>(pub(crate) StructReader<'a>);

impl<'a> From<StructReader<'a>> for DynamicStructReader<'a> {
    fn from(reader: StructReader<'a>) -> Self {
        Self(reader)
    }
}

impl<'a> IntoInternalStructReader<'a> for DynamicStructReader<'a> {
    fn into_internal_struct_reader(self) -> StructReader<'a> {
        self.0
    }
}

impl SetterInput<DynamicStruct> for DynamicStructReader<'_> {
    fn set_pointer_builder(
        mut builder: PointerBuilder<'_>,
        input: Self,
        canonicalize: bool,
    ) -> Result<()> {
        builder.set_struct(&input.0, canonicalize)
    }
}

pub(crate) struct DynamicStructBuilder<'a>(pub(crate) StructBuilder<'a>);

impl<'a> From<StructBuilder<'a>> for DynamicStructBuilder<'a> {
    fn from(builder: StructBuilder<'a>) -> Self {
        Self(builder)
    }
}

impl HasStructSize for DynamicStructBuilder<'_> {
    /// The empty shape: opening an existing struct list through
    /// [`struct_list::Builder`] keeps whatever element size the list was
    /// initialised with, because a list is only ever re-laid out when its
    /// elements are smaller than this constant.
    const STRUCT_SIZE: StructSize = StructSize {
        data: 0,
        pointers: 0,
    };
}

pub(crate) fn struct_size(shape: StructShape) -> StructSize {
    StructSize {
        data: shape.data_words,
        pointers: shape.pointer_count,
    }
}

/// Initialises a struct list of `count` elements shaped like `shape` behind
/// `pointer` and opens it for element access.
pub(crate) fn init_struct_list<'a>(
    mut pointer: PointerBuilder<'a>,
    count: u32,
    shape: StructShape,
) -> Result<struct_list::Builder<'a, DynamicStruct>> {
    pointer
        .reborrow()
        .init_struct_list(count, struct_size(shape));
    struct_list::Builder::get_from_pointer(pointer, None)
}

/// Opens the struct list behind `pointer` for reading; a null pointer reads
/// as an empty list.
pub(crate) fn read_struct_list<'a>(
    pointer: PointerReader<'a>,
) -> Result<struct_list::Reader<'a, DynamicStruct>> {
    struct_list::Reader::get_from_pointer(&pointer, None)
}
