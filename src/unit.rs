//! Functions for parsing DWARF `.debug_info` and `.debug_types` sections.

use constants;
use abbrev::{DebugAbbrev, DebugAbbrevOffset, Abbreviations, Abbreviation, AttributeSpecification};
use endianity::{Endianity, EndianBuf};
use fallible_iterator::FallibleIterator;
use line::DebugLineOffset;
use loc::DebugLocOffset;
use op::Expression;
use parser::{Error, Result, Format, DebugMacinfoOffset, parse_initial_length};
use ranges::DebugRangesOffset;
use reader::{Reader, ReaderOffset};
use std::cell::Cell;
use std::ops::{Range, RangeFrom, RangeTo};
use std::{u8, u16};
use str::{DebugStr, DebugStrOffset};
use Section;

/// An offset into the `.debug_types` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DebugTypesOffset<T = usize>(pub T);

impl<T: ReaderOffset> DebugTypesOffset<T> {
    /// Convert an offset to be relative to the start of the given unit,
    /// instead of relative to the start of the .debug_types section.
    /// Returns `None` if the offset is not within the unit entries.
    pub fn to_unit_offset<R>(&self, unit: &TypeUnitHeader<R, R::Offset>) -> Option<UnitOffset<T>>
        where R: Reader<Offset = T>
    {
        let offset = match self.0.checked_sub(unit.offset.0) {
            Some(offset) => UnitOffset(offset),
            None => return None,
        };
        if !unit.header.is_valid_offset(offset) {
            return None;
        }
        Some(offset)
    }
}

/// A type signature as used in the `.debug_types` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DebugTypeSignature(pub u64);

/// An offset into the `.debug_info` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DebugInfoOffset<T = usize>(pub T);

impl<T: ReaderOffset> DebugInfoOffset<T> {
    /// Convert an offset to be relative to the start of the given unit,
    /// instead of relative to the start of the .debug_info section.
    /// Returns `None` if the offset is not within this unit entries.
    pub fn to_unit_offset<R>(&self,
                             unit: &CompilationUnitHeader<R, R::Offset>)
                             -> Option<UnitOffset<T>>
        where R: Reader<Offset = T>
    {
        let offset = match self.0.checked_sub(unit.offset.0) {
            Some(offset) => UnitOffset(offset),
            None => return None,
        };
        if !unit.header.is_valid_offset(offset) {
            return None;
        }
        Some(offset)
    }
}

/// An offset into the current compilation or type unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct UnitOffset<T = usize>(pub T);

impl<T: ReaderOffset> UnitOffset<T> {
    /// Convert an offset to be relative to the start of the .debug_info section,
    /// instead of relative to the start of the given compilation unit.
    pub fn to_debug_info_offset<R>(&self,
                                   unit: &CompilationUnitHeader<R, R::Offset>)
                                   -> DebugInfoOffset<T>
        where R: Reader<Offset = T>
    {
        DebugInfoOffset(unit.offset.0 + self.0)
    }

    /// Convert an offset to be relative to the start of the .debug_types section,
    /// instead of relative to the start of the given type unit.
    pub fn to_debug_types_offset<R>(&self,
                                    unit: &TypeUnitHeader<R, R::Offset>)
                                    -> DebugTypesOffset<T>
        where R: Reader<Offset = T>
    {
        DebugTypesOffset(unit.offset.0 + self.0)
    }
}

/// The `DebugInfo` struct represents the DWARF debugging information found in
/// the `.debug_info` section.
#[derive(Debug, Clone, Copy)]
pub struct DebugInfo<R: Reader> {
    debug_info_section: R,
}

impl<'input, Endian> DebugInfo<EndianBuf<'input, Endian>>
    where Endian: Endianity
{
    /// Construct a new `DebugInfo` instance from the data in the `.debug_info`
    /// section.
    ///
    /// It is the caller's responsibility to read the `.debug_info` section and
    /// present it as a `&[u8]` slice. That means using some ELF loader on
    /// Linux, a Mach-O loader on OSX, etc.
    ///
    /// ```
    /// use gimli::{DebugInfo, LittleEndian};
    ///
    /// # let buf = [0x00, 0x01, 0x02, 0x03];
    /// # let read_debug_info_section_somehow = || &buf;
    /// let debug_info = DebugInfo::new(read_debug_info_section_somehow(), LittleEndian);
    /// ```
    pub fn new(debug_info_section: &'input [u8], endian: Endian) -> Self {
        Self::from(EndianBuf::new(debug_info_section, endian))
    }
}

impl<R: Reader> DebugInfo<R> {
    /// Iterate the compilation- and partial-units in this
    /// `.debug_info` section.
    ///
    /// ```
    /// use gimli::{DebugInfo, LittleEndian};
    ///
    /// # let buf = [];
    /// # let read_debug_info_section_somehow = || &buf;
    /// let debug_info = DebugInfo::new(read_debug_info_section_somehow(), LittleEndian);
    ///
    /// let mut iter = debug_info.units();
    /// while let Some(unit) = iter.next().unwrap() {
    ///     println!("unit's length is {}", unit.unit_length());
    /// }
    /// ```
    ///
    /// Can be [used with
    /// `FallibleIterator`](./index.html#using-with-fallibleiterator).
    pub fn units(&self) -> CompilationUnitHeadersIter<R> {
        CompilationUnitHeadersIter {
            input: self.debug_info_section.clone(),
            offset: DebugInfoOffset(R::Offset::from_u8(0)),
        }
    }

    /// Get the CompilationUnitHeader located at offset from this .debug_info section.
    ///
    ///
    pub fn header_from_offset(&self,
                              offset: DebugInfoOffset<R::Offset>)
                              -> Result<CompilationUnitHeader<R, R::Offset>> {
        let input = &mut self.debug_info_section.clone();
        input.skip(offset.0)?;
        CompilationUnitHeader::parse(input, offset)
    }
}

impl<R: Reader> Section<R> for DebugInfo<R> {
    fn section_name() -> &'static str {
        ".debug_info"
    }
}

impl<R: Reader> From<R> for DebugInfo<R> {
    fn from(debug_info_section: R) -> Self {
        DebugInfo { debug_info_section }
    }
}

/// An iterator over the compilation- and partial-units of a section.
///
/// See the [documentation on
/// `DebugInfo::units`](./struct.DebugInfo.html#method.units) for more detail.
#[derive(Clone, Debug)]
pub struct CompilationUnitHeadersIter<R: Reader> {
    input: R,
    offset: DebugInfoOffset<R::Offset>,
}

impl<R: Reader> CompilationUnitHeadersIter<R> {
    /// Advance the iterator to the next unit header.
    pub fn next(&mut self) -> Result<Option<CompilationUnitHeader<R, R::Offset>>> {
        if self.input.is_empty() {
            Ok(None)
        } else {
            let len = self.input.len();
            match CompilationUnitHeader::parse(&mut self.input, self.offset) {
                Ok(header) => {
                    self.offset.0 += len - self.input.len();
                    Ok(Some(header))
                }
                Err(e) => {
                    self.input.empty();
                    Err(e)
                }
            }
        }
    }
}

impl<R: Reader> FallibleIterator for CompilationUnitHeadersIter<R> {
    type Item = CompilationUnitHeader<R, R::Offset>;
    type Error = Error;

    fn next(&mut self) -> ::std::result::Result<Option<Self::Item>, Self::Error> {
        CompilationUnitHeadersIter::next(self)
    }
}

/// The header of a compilation unit's debugging information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompilationUnitHeader<R, Offset = usize>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    header: UnitHeader<R, Offset>,
    offset: DebugInfoOffset<Offset>,
}

impl<R, Offset> CompilationUnitHeader<R, Offset>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    /// Return the serialized size of the compilation unit header for the given
    /// DWARF format.
    pub fn size_of_header(format: Format) -> usize {
        UnitHeader::<R, _>::size_of_header(format)
    }

    /// Get the offset of this compilation unit within the .debug_info section.
    pub fn offset(&self) -> DebugInfoOffset<R::Offset> {
        self.offset
    }

    /// Get the length of the debugging info for this compilation unit, not
    /// including the byte length of the encoded length itself.
    pub fn unit_length(&self) -> R::Offset {
        self.header.unit_length
    }

    /// Get the length of the debugging info for this compilation unit,
    /// including the byte length of the encoded length itself.
    pub fn length_including_self(&self) -> R::Offset {
        self.header.length_including_self()
    }

    /// Get the DWARF version of the debugging info for this compilation unit.
    pub fn version(&self) -> u16 {
        self.header.version
    }

    /// The offset into the `.debug_abbrev` section for this compilation unit's
    /// debugging information entries' abbreviations.
    pub fn debug_abbrev_offset(&self) -> DebugAbbrevOffset<R::Offset> {
        self.header.debug_abbrev_offset
    }

    /// The size of addresses (in bytes) in this type-unit.
    pub fn address_size(&self) -> u8 {
        self.header.address_size
    }

    /// Whether this type unit is encoded in 64- or 32-bit DWARF.
    pub fn format(&self) -> Format {
        self.header.format
    }

    /// The serialized size of the header for this compilation unit.
    pub fn header_size(&self) -> R::Offset {
        self.header.header_size()
    }

    /// Navigate this compilation unit's `DebuggingInformationEntry`s.
    pub fn entries<'me, 'abbrev>(&'me self,
                                 abbreviations: &'abbrev Abbreviations)
                                 -> EntriesCursor<'abbrev, 'me, R> {
        self.header.entries(abbreviations)
    }

    /// Navigate this compilation unit's `DebuggingInformationEntry`s
    /// starting at the given offset.
    pub fn entries_at_offset<'me, 'abbrev>(&'me self,
                                           abbreviations: &'abbrev Abbreviations,
                                           offset: UnitOffset<R::Offset>)
                                           -> Result<EntriesCursor<'abbrev, 'me, R>> {
        self.header.entries_at_offset(abbreviations, offset)
    }

    /// Navigate this compilation unit's `DebuggingInformationEntry`s as a tree
    /// starting at the given offset.
    pub fn entries_tree<'me, 'abbrev>(&'me self,
                                      abbreviations: &'abbrev Abbreviations,
                                      offset: Option<UnitOffset<R::Offset>>)
                                      -> Result<EntriesTree<'abbrev, 'me, R>> {
        self.header.entries_tree(abbreviations, offset)
    }

    /// Parse this compilation unit's abbreviations.
    ///
    /// ```
    /// use gimli::DebugAbbrev;
    /// # use gimli::{DebugInfo, LittleEndian};
    /// # let info_buf = [
    /// #     // Comilation unit header
    /// #
    /// #     // 32-bit unit length = 25
    /// #     0x19, 0x00, 0x00, 0x00,
    /// #     // Version 4
    /// #     0x04, 0x00,
    /// #     // debug_abbrev_offset
    /// #     0x00, 0x00, 0x00, 0x00,
    /// #     // Address size
    /// #     0x04,
    /// #
    /// #     // DIEs
    /// #
    /// #     // Abbreviation code
    /// #     0x01,
    /// #     // Attribute of form DW_FORM_string = "foo\0"
    /// #     0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #       // Children
    /// #
    /// #       // Abbreviation code
    /// #       0x01,
    /// #       // Attribute of form DW_FORM_string = "foo\0"
    /// #       0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #         // Children
    /// #
    /// #         // Abbreviation code
    /// #         0x01,
    /// #         // Attribute of form DW_FORM_string = "foo\0"
    /// #         0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #           // Children
    /// #
    /// #           // End of children
    /// #           0x00,
    /// #
    /// #         // End of children
    /// #         0x00,
    /// #
    /// #       // End of children
    /// #       0x00,
    /// # ];
    /// # let debug_info = DebugInfo::new(&info_buf, LittleEndian);
    /// #
    /// # let abbrev_buf = [
    /// #     // Code
    /// #     0x01,
    /// #     // DW_TAG_subprogram
    /// #     0x2e,
    /// #     // DW_CHILDREN_yes
    /// #     0x01,
    /// #     // Begin attributes
    /// #       // Attribute name = DW_AT_name
    /// #       0x03,
    /// #       // Attribute form = DW_FORM_string
    /// #       0x08,
    /// #     // End attributes
    /// #     0x00,
    /// #     0x00,
    /// #     // Null terminator
    /// #     0x00
    /// # ];
    /// #
    /// # let get_some_unit = || debug_info.units().next().unwrap().unwrap();
    ///
    /// let unit = get_some_unit();
    ///
    /// # let read_debug_abbrev_section_somehow = || &abbrev_buf;
    /// let debug_abbrev = DebugAbbrev::new(read_debug_abbrev_section_somehow(), LittleEndian);
    /// let abbrevs_for_unit = unit.abbreviations(&debug_abbrev).unwrap();
    /// ```
    pub fn abbreviations(&self, debug_abbrev: &DebugAbbrev<R>) -> Result<Abbreviations> {
        self.header.abbreviations(debug_abbrev)
    }

    /// Parse a compilation unit header.
    fn parse(input: &mut R,
             offset: DebugInfoOffset<R::Offset>)
             -> Result<CompilationUnitHeader<R, R::Offset>> {
        let header = parse_unit_header(input)?;
        Ok(CompilationUnitHeader {
               header: header,
               offset: offset,
           })
    }
}

/// Parse the DWARF version from the compilation unit header.
fn parse_version<R: Reader>(input: &mut R) -> Result<u16> {
    let val = input.read_u16()?;

    // DWARF 1 was very different, and is obsolete, so isn't supported by this
    // reader.
    if 2 <= val && val <= 4 {
        Ok(val)
    } else {
        Err(Error::UnknownVersion)
    }
}

/// Parse the `debug_abbrev_offset` in the compilation unit header.
fn parse_debug_abbrev_offset<R: Reader>(input: &mut R,
                                        format: Format)
                                        -> Result<DebugAbbrevOffset<R::Offset>> {
    input.read_offset(format).map(DebugAbbrevOffset)
}

/// Parse the `debug_info_offset` in the arange header.
pub fn parse_debug_info_offset<R: Reader>(input: &mut R,
                                          format: Format)
                                          -> Result<DebugInfoOffset<R::Offset>> {
    input.read_offset(format).map(DebugInfoOffset)
}

/// The common fields for the headers of compilation units and
/// type units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitHeader<R, Offset = usize>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    unit_length: Offset,
    version: u16,
    debug_abbrev_offset: DebugAbbrevOffset<Offset>,
    address_size: u8,
    format: Format,
    entries_buf: R,
}

/// Static methods.
impl<R, Offset> UnitHeader<R, Offset>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    /// Construct a new `UnitHeader`.
    pub fn new(unit_length: R::Offset,
               version: u16,
               debug_abbrev_offset: DebugAbbrevOffset<R::Offset>,
               address_size: u8,
               format: Format,
               entries_buf: R)
               -> Self {
        UnitHeader {
            unit_length: unit_length,
            version: version,
            debug_abbrev_offset: debug_abbrev_offset,
            address_size: address_size,
            format: format,
            entries_buf: entries_buf,
        }
    }

    /// Return the serialized size of the `unit_length` attribute for the given
    /// DWARF format.
    pub fn size_of_unit_length(format: Format) -> usize {
        match format {
            Format::Dwarf32 => 4,
            Format::Dwarf64 => 12,
        }
    }

    /// Return the serialized size of the common unit header for the given
    /// DWARF format.
    pub fn size_of_header(format: Format) -> usize {
        let unit_length_size = Self::size_of_unit_length(format);
        let version_size = 2;
        let debug_abbrev_offset_size = match format {
            Format::Dwarf32 => 4,
            Format::Dwarf64 => 8,
        };
        let address_size_size = 1;

        unit_length_size + version_size + debug_abbrev_offset_size + address_size_size
    }
}

/// Instance methods.
impl<R, Offset> UnitHeader<R, Offset>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    /// Get the length of the debugging info for this compilation unit, not
    /// including the byte length of the encoded length itself.
    pub fn unit_length(&self) -> R::Offset {
        self.unit_length
    }

    /// Get the length of the debugging info for this compilation unit,
    /// including the byte length of the encoded length itself.
    pub fn length_including_self(&self) -> R::Offset {
        match self.format {
            // Length of the 32-bit header plus the unit length.
            Format::Dwarf32 => R::Offset::from_u8(4) + self.unit_length,
            // Length of the 4 byte 0xffffffff value to enable 64-bit mode plus
            // the actual 64-bit length.
            Format::Dwarf64 => R::Offset::from_u8(4 + 8) + self.unit_length,
        }
    }

    /// Get the DWARF version of the debugging info for this compilation unit.
    pub fn version(&self) -> u16 {
        self.version
    }

    /// The offset into the `.debug_abbrev` section for this compilation unit's
    /// debugging information entries' abbreviations.
    pub fn debug_abbrev_offset(&self) -> DebugAbbrevOffset<R::Offset> {
        self.debug_abbrev_offset
    }

    /// The size of addresses (in bytes) in this compilation unit.
    pub fn address_size(&self) -> u8 {
        self.address_size
    }

    /// Whether this compilation unit is encoded in 64- or 32-bit DWARF.
    pub fn format(&self) -> Format {
        self.format
    }

    /// The serialized size of the header for this compilation unit.
    pub fn header_size(&self) -> R::Offset {
        self.length_including_self() - self.entries_buf.len()
    }

    fn is_valid_offset(&self, offset: UnitOffset<R::Offset>) -> bool {
        let size_of_header = self.header_size();
        if offset.0 < size_of_header {
            return false;
        }

        let relative_to_entries_buf = offset.0 - size_of_header;
        relative_to_entries_buf < self.entries_buf.len()
    }

    /// Get the underlying bytes for the supplied range.
    pub fn range(&self, idx: Range<UnitOffset<R::Offset>>) -> Result<R> {
        if !self.is_valid_offset(idx.start) {
            return Err(Error::OffsetOutOfBounds);
        }
        if !self.is_valid_offset(idx.end) {
            return Err(Error::OffsetOutOfBounds);
        }
        assert!(idx.start <= idx.end);
        let size_of_header = self.header_size();
        let start = idx.start.0 - size_of_header;
        let end = idx.end.0 - size_of_header;
        let mut input = self.entries_buf.clone();
        input.skip(start)?;
        input.truncate(end - start)?;
        Ok(input)
    }

    /// Get the underlying bytes for the supplied range.
    pub fn range_from(&self, idx: RangeFrom<UnitOffset<R::Offset>>) -> Result<R> {
        if !self.is_valid_offset(idx.start) {
            return Err(Error::OffsetOutOfBounds);
        }
        let start = idx.start.0 - self.header_size();
        let mut input = self.entries_buf.clone();
        input.skip(start)?;
        Ok(input)
    }

    /// Get the underlying bytes for the supplied range.
    pub fn range_to(&self, idx: RangeTo<UnitOffset<R::Offset>>) -> Result<R> {
        if !self.is_valid_offset(idx.end) {
            return Err(Error::OffsetOutOfBounds);
        }
        let end = idx.end.0 - self.header_size();
        let mut input = self.entries_buf.clone();
        input.truncate(end)?;
        Ok(input)
    }

    /// Navigate this unit's `DebuggingInformationEntry`s.
    pub fn entries<'me, 'abbrev>(&'me self,
                                 abbreviations: &'abbrev Abbreviations)
                                 -> EntriesCursor<'abbrev, 'me, R> {
        EntriesCursor {
            unit: self,
            input: self.entries_buf.clone(),
            abbreviations: abbreviations,
            cached_current: None,
            delta_depth: 0,
        }
    }

    /// Navigate this compilation unit's `DebuggingInformationEntry`s
    /// starting at the given offset.
    pub fn entries_at_offset<'me, 'abbrev>(&'me self,
                                           abbreviations: &'abbrev Abbreviations,
                                           offset: UnitOffset<R::Offset>)
                                           -> Result<EntriesCursor<'abbrev, 'me, R>> {
        let input = self.range_from(offset..)?;
        Ok(EntriesCursor {
               unit: self,
               input: input,
               abbreviations: abbreviations,
               cached_current: None,
               delta_depth: 0,
           })
    }

    /// Navigate this unit's `DebuggingInformationEntry`s as a tree
    /// starting at the given offset.
    pub fn entries_tree<'me, 'abbrev>(&'me self,
                                      abbreviations: &'abbrev Abbreviations,
                                      offset: Option<UnitOffset<R::Offset>>)
                                      -> Result<EntriesTree<'abbrev, 'me, R>> {
        let input = match offset {
            Some(offset) => self.range_from(offset..)?,
            None => self.entries_buf.clone(),
        };
        Ok(EntriesTree::new(input, self, abbreviations))
    }

    /// Parse this unit's abbreviations.
    pub fn abbreviations(&self, debug_abbrev: &DebugAbbrev<R>) -> Result<Abbreviations> {
        debug_abbrev.abbreviations(self.debug_abbrev_offset())
    }
}

/// Parse a compilation unit header.
fn parse_unit_header<R: Reader>(input: &mut R) -> Result<UnitHeader<R, R::Offset>> {
    let (unit_length, format) = parse_initial_length(input)?;
    let unit_length = R::Offset::from_u64(unit_length)?;
    let mut rest = input.split(unit_length)?;

    let version = parse_version(&mut rest)?;
    let offset = parse_debug_abbrev_offset(&mut rest, format)?;
    let address_size = rest.read_u8()?;

    Ok(UnitHeader::new(unit_length, version, offset, address_size, format, rest))
}

/// A Debugging Information Entry (DIE).
///
/// DIEs have a set of attributes and optionally have children DIEs as well.
#[derive(Clone, Debug)]
pub struct DebuggingInformationEntry<'abbrev, 'unit, R, Offset = usize>
    where R: Reader<Offset = Offset> + 'unit,
          Offset: ReaderOffset + 'unit
{
    offset: UnitOffset<Offset>,
    attrs_slice: R,
    attrs_len: Cell<Option<Offset>>,
    abbrev: &'abbrev Abbreviation,
    unit: &'unit UnitHeader<R, Offset>,
}

impl<'abbrev, 'unit, R, Offset> DebuggingInformationEntry<'abbrev, 'unit, R, Offset>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    /// Get this entry's code.
    pub fn code(&self) -> u64 {
        self.abbrev.code()
    }

    /// Get this entry's offset.
    pub fn offset(&self) -> UnitOffset<R::Offset> {
        self.offset
    }

    /// Get this entry's `DW_TAG_whatever` tag.
    ///
    /// ```
    /// # use gimli::{DebugAbbrev, DebugInfo, LittleEndian};
    /// # let info_buf = [
    /// #     // Comilation unit header
    /// #
    /// #     // 32-bit unit length = 12
    /// #     0x0c, 0x00, 0x00, 0x00,
    /// #     // Version 4
    /// #     0x04, 0x00,
    /// #     // debug_abbrev_offset
    /// #     0x00, 0x00, 0x00, 0x00,
    /// #     // Address size
    /// #     0x04,
    /// #
    /// #     // DIEs
    /// #
    /// #     // Abbreviation code
    /// #     0x01,
    /// #     // Attribute of form DW_FORM_string = "foo\0"
    /// #     0x66, 0x6f, 0x6f, 0x00,
    /// # ];
    /// # let debug_info = DebugInfo::new(&info_buf, LittleEndian);
    /// # let abbrev_buf = [
    /// #     // Code
    /// #     0x01,
    /// #     // DW_TAG_subprogram
    /// #     0x2e,
    /// #     // DW_CHILDREN_no
    /// #     0x00,
    /// #     // Begin attributes
    /// #       // Attribute name = DW_AT_name
    /// #       0x03,
    /// #       // Attribute form = DW_FORM_string
    /// #       0x08,
    /// #     // End attributes
    /// #     0x00,
    /// #     0x00,
    /// #     // Null terminator
    /// #     0x00
    /// # ];
    /// # let debug_abbrev = DebugAbbrev::new(&abbrev_buf, LittleEndian);
    /// # let unit = debug_info.units().next().unwrap().unwrap();
    /// # let abbrevs = unit.abbreviations(&debug_abbrev).unwrap();
    /// # let mut cursor = unit.entries(&abbrevs);
    /// # let (_, entry) = cursor.next_dfs().unwrap().unwrap();
    /// # let mut get_some_entry = || entry;
    /// let entry = get_some_entry();
    ///
    /// match entry.tag() {
    ///     gimli::DW_TAG_subprogram =>
    ///         println!("this entry contains debug info about a function"),
    ///     gimli::DW_TAG_inlined_subroutine =>
    ///         println!("this entry contains debug info about a particular instance of inlining"),
    ///     gimli::DW_TAG_variable =>
    ///         println!("this entry contains debug info about a local variable"),
    ///     gimli::DW_TAG_formal_parameter =>
    ///         println!("this entry contains debug info about a function parameter"),
    ///     otherwise =>
    ///         println!("this entry is some other kind of data: {:?}", otherwise),
    /// };
    /// ```
    pub fn tag(&self) -> constants::DwTag {
        self.abbrev.tag()
    }

    /// Return true if this entry's type can have children, false otherwise.
    pub fn has_children(&self) -> bool {
        self.abbrev.has_children()
    }

    /// Iterate over this entry's set of attributes.
    ///
    /// ```
    /// use gimli::{DebugAbbrev, DebugInfo, LittleEndian};
    ///
    /// // Read the `.debug_info` section.
    ///
    /// # let info_buf = [
    /// #     // Comilation unit header
    /// #
    /// #     // 32-bit unit length = 12
    /// #     0x0c, 0x00, 0x00, 0x00,
    /// #     // Version 4
    /// #     0x04, 0x00,
    /// #     // debug_abbrev_offset
    /// #     0x00, 0x00, 0x00, 0x00,
    /// #     // Address size
    /// #     0x04,
    /// #
    /// #     // DIEs
    /// #
    /// #     // Abbreviation code
    /// #     0x01,
    /// #     // Attribute of form DW_FORM_string = "foo\0"
    /// #     0x66, 0x6f, 0x6f, 0x00,
    /// # ];
    /// # let read_debug_info_section_somehow = || &info_buf;
    /// let debug_info = DebugInfo::new(read_debug_info_section_somehow(), LittleEndian);
    ///
    /// // Get the data about the first compilation unit out of the `.debug_info`.
    ///
    /// let unit = debug_info.units().next()
    ///     .expect("Should have at least one compilation unit")
    ///     .expect("and it should parse ok");
    ///
    /// // Read the `.debug_abbrev` section and parse the
    /// // abbreviations for our compilation unit.
    ///
    /// # let abbrev_buf = [
    /// #     // Code
    /// #     0x01,
    /// #     // DW_TAG_subprogram
    /// #     0x2e,
    /// #     // DW_CHILDREN_no
    /// #     0x00,
    /// #     // Begin attributes
    /// #       // Attribute name = DW_AT_name
    /// #       0x03,
    /// #       // Attribute form = DW_FORM_string
    /// #       0x08,
    /// #     // End attributes
    /// #     0x00,
    /// #     0x00,
    /// #     // Null terminator
    /// #     0x00
    /// # ];
    /// # let read_debug_abbrev_section_somehow = || &abbrev_buf;
    /// let debug_abbrev = DebugAbbrev::new(read_debug_abbrev_section_somehow(), LittleEndian);
    /// let abbrevs = unit.abbreviations(&debug_abbrev).unwrap();
    ///
    /// // Get the first entry from that compilation unit.
    ///
    /// let mut cursor = unit.entries(&abbrevs);
    /// let (_, entry) = cursor.next_dfs()
    ///     .expect("Should parse next entry")
    ///     .expect("Should have at least one entry");
    ///
    /// // Finally, print the first entry's attributes.
    ///
    /// let mut attrs = entry.attrs();
    /// while let Some(attr) = attrs.next().unwrap() {
    ///     println!("Attribute name = {:?}", attr.name());
    ///     println!("Attribute value = {:?}", attr.value());
    /// }
    /// ```
    ///
    /// Can be [used with
    /// `FallibleIterator`](./index.html#using-with-fallibleiterator).
    pub fn attrs<'me>(&'me self) -> AttrsIter<'abbrev, 'me, 'unit, R> {
        AttrsIter {
            input: self.attrs_slice.clone(),
            attributes: self.abbrev.attributes(),
            entry: self,
        }
    }

    /// Find the first attribute in this entry which has the given name,
    /// and return it. Returns `Ok(None)` if no attribute is found.
    pub fn attr(&self, name: constants::DwAt) -> Result<Option<Attribute<R>>> {
        let mut attrs = self.attrs();
        while let Some(attr) = attrs.next()? {
            if attr.name() == name {
                return Ok(Some(attr));
            }
        }
        Ok(None)
    }

    /// Find the first attribute in this entry which has the given name,
    /// and return its raw value. Returns `Ok(None)` if no attribute is found.
    pub fn attr_value_raw(&self, name: constants::DwAt) -> Result<Option<AttributeValue<R>>> {
        self.attr(name)
            .map(|attr| attr.map(|attr| attr.raw_value()))
    }

    /// Find the first attribute in this entry which has the given name,
    /// and return its normalized value.  Returns `Ok(None)` if no
    /// attribute is found.
    pub fn attr_value(&self, name: constants::DwAt) -> Result<Option<AttributeValue<R>>> {
        self.attr(name).map(|attr| attr.map(|attr| attr.value()))
    }

    /// Return the input buffer after the last attribute.
    #[allow(inline_always)]
    #[inline(always)]
    fn after_attrs(&self) -> Result<R> {
        if let Some(attrs_len) = self.attrs_len.get() {
            let mut input = self.attrs_slice.clone();
            input.skip(attrs_len)?;
            Ok(input)
        } else {
            let mut attrs = self.attrs();
            while let Some(_) = attrs.next()? {}
            Ok(attrs.input)
        }
    }

    /// Use the `DW_AT_sibling` attribute to find the input buffer for the
    /// next sibling. Returns `None` if the attribute is missing or invalid.
    fn sibling(&self) -> Option<R> {
        let attr = self.attr_value(constants::DW_AT_sibling);
        if let Ok(Some(AttributeValue::UnitRef(offset))) = attr {
            if offset.0 > self.offset.0 {
                if let Ok(input) = self.unit.range_from(offset..) {
                    return Some(input);
                }
            }
        }
        None
    }

    /// Parse an entry. Returns `Ok(None)` for null entries.
    #[allow(inline_always)]
    #[inline(always)]
    fn parse(input: &mut R,
             unit: &'unit UnitHeader<R, R::Offset>,
             abbreviations: &'abbrev Abbreviations)
             -> Result<Option<Self>> {
        let offset = unit.header_size() + input.offset_from(&unit.entries_buf);
        let code = input.read_uleb128()?;
        if code == 0 {
            return Ok(None);
        };
        let abbrev = abbreviations.get(code).ok_or(Error::UnknownAbbreviation)?;
        Ok(Some(DebuggingInformationEntry {
                    offset: UnitOffset(offset),
                    attrs_slice: input.clone(),
                    attrs_len: Cell::new(None),
                    abbrev: abbrev,
                    unit: unit,
                }))
    }
}

/// The value of an attribute in a `DebuggingInformationEntry`.
//
// Set the discriminant size so that all variants use the same alignment
// for their data.  This gives better code generation in `parse_attribute`.
#[repr(u64)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttributeValue<R: Reader> {
    /// "Refers to some location in the address space of the described program."
    Addr(u64),

    /// A slice of an arbitrary number of bytes.
    Block(R),

    /// A one byte constant data value. How to interpret the byte depends on context.
    ///
    /// From section 7 of the standard: "Depending on context, it may be a
    /// signed integer, an unsigned integer, a floating-point constant, or
    /// anything else."
    Data1([u8; 1]),

    /// A two byte constant data value. How to interpret the bytes depends on context.
    ///
    /// From section 7 of the standard: "Depending on context, it may be a
    /// signed integer, an unsigned integer, a floating-point constant, or
    /// anything else."
    Data2(([u8; 2], R::Endian)),

    /// A four byte constant data value. How to interpret the bytes depends on context.
    ///
    /// From section 7 of the standard: "Depending on context, it may be a
    /// signed integer, an unsigned integer, a floating-point constant, or
    /// anything else."
    Data4(([u8; 4], R::Endian)),

    /// An eight byte constant data value. How to interpret the bytes depends on context.
    ///
    /// From section 7 of the standard: "Depending on context, it may be a
    /// signed integer, an unsigned integer, a floating-point constant, or
    /// anything else."
    Data8(([u8; 8], R::Endian)),

    /// A signed integer constant.
    Sdata(i64),

    /// An unsigned integer constant.
    Udata(u64),

    /// "The information bytes contain a DWARF expression (see Section 2.5) or
    /// location description (see Section 2.6)."
    Exprloc(Expression<R>),

    /// A boolean typically used to describe the presence or absence of another
    /// attribute.
    Flag(bool),

    /// An offset into another section. Which section this is an offset into
    /// depends on context.
    SecOffset(R::Offset),

    /// An offset into the current compilation unit.
    UnitRef(UnitOffset<R::Offset>),

    /// An offset into the current `.debug_info` section, but possibly a
    /// different compilation unit from the current one.
    DebugInfoRef(DebugInfoOffset<R::Offset>),

    /// An offset into the `.debug_line` section.
    DebugLineRef(DebugLineOffset<R::Offset>),

    /// An offset into the `.debug_loc` section.
    DebugLocRef(DebugLocOffset<R::Offset>),

    /// An offset into the `.debug_macinfo` section.
    DebugMacinfoRef(DebugMacinfoOffset<R::Offset>),

    /// An offset into the `.debug_ranges` section.
    DebugRangesRef(DebugRangesOffset<R::Offset>),

    /// A type signature.
    DebugTypesRef(DebugTypeSignature),

    /// An offset into the `.debug_str` section.
    DebugStrRef(DebugStrOffset<R::Offset>),

    /// A slice of bytes representing a string. Does not include a final null byte.
    /// Not guaranteed to be UTF-8 or anything like that.
    String(R),

    /// The value of a `DW_AT_encoding` attribute.
    Encoding(constants::DwAte),

    /// The value of a `DW_AT_decimal_sign` attribute.
    DecimalSign(constants::DwDs),

    /// The value of a `DW_AT_endianity` attribute.
    Endianity(constants::DwEnd),

    /// The value of a `DW_AT_accessibility` attribute.
    Accessibility(constants::DwAccess),

    /// The value of a `DW_AT_visibility` attribute.
    Visibility(constants::DwVis),

    /// The value of a `DW_AT_virtuality` attribute.
    Virtuality(constants::DwVirtuality),

    /// The value of a `DW_AT_language` attribute.
    Language(constants::DwLang),

    /// The value of a `DW_AT_address_class` attribute.
    AddressClass(constants::DwAddr),

    /// The value of a `DW_AT_identifier_case` attribute.
    IdentifierCase(constants::DwId),

    /// The value of a `DW_AT_calling_convention` attribute.
    CallingConvention(constants::DwCc),

    /// The value of a `DW_AT_inline` attribute.
    Inline(constants::DwInl),

    /// The value of a `DW_AT_ordering` attribute.
    Ordering(constants::DwOrd),

    /// An index into the filename entries from the line number information
    /// table for the compilation unit containing this value.
    FileIndex(u64),
}

/// An attribute in a `DebuggingInformationEntry`, consisting of a name and
/// associated value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Attribute<R: Reader> {
    name: constants::DwAt,
    value: AttributeValue<R>,
}

impl<R: Reader> Attribute<R> {
    /// Get this attribute's name.
    pub fn name(&self) -> constants::DwAt {
        self.name
    }

    /// Get this attribute's raw value.
    pub fn raw_value(&self) -> AttributeValue<R> {
        self.value.clone()
    }

    /// Get this attribute's normalized value.
    ///
    /// Attribute values can potentially be encoded in multiple equivalent forms,
    /// and may have special meaning depending on the attribute name.  This method
    /// converts the attribute value to a normalized form based on the attribute
    /// name.
    ///
    /// See "Figure 20. Attribute encodings" and "Figure 21. Attribute form encodings".
    #[allow(cyclomatic_complexity)]
    #[allow(match_same_arms)]
    pub fn value(&self) -> AttributeValue<R> {
        // Figure 20 shows the possible attribute classes for each name.
        // Figure 21 shows the possible attribute classes for each form.
        // For each attribute name, we need to match on the form, and
        // convert it to one of the classes that is allowed for both
        // the name and the form.
        //
        // The individual class conversions rarely vary for each name,
        // so for each class conversion we define a macro that matches
        // on the allowed forms for that class.
        //
        // For some classes, we don't need to do any conversion, so their
        // macro is empty.  In the future we may want to fill them in to
        // provide strict checking of the forms for each class.  For now,
        // they simply provide a way to document the allowed classes for
        // each name.
        macro_rules! address {
            () => ();
        }
        macro_rules! block {
            () => ();
        }
        macro_rules! constant {
            ($value:ident, $variant:ident) => (
                if let Some(value) = self.$value() {
                    return AttributeValue::$variant(value);
                });
            ($value:ident, $variant:ident, $constant:ident) => (
                if let Some(value) = self.$value() {
                    return AttributeValue::$variant(constants::$constant(value));
                });
        }
        macro_rules! exprloc {
            () => (
                if let Some(value) = self.exprloc_value() {
                    return AttributeValue::Exprloc(value);
                });
        }
        macro_rules! flag {
            () => ();
        }
        macro_rules! loclistptr {
            () => (
                if let Some(offset) = self.offset_value() {
                    return AttributeValue::DebugLocRef(DebugLocOffset(offset));
                });
        }
        macro_rules! lineptr {
            () => (
                if let Some(offset) = self.offset_value() {
                    return AttributeValue::DebugLineRef(DebugLineOffset(offset));
                });
        }
        macro_rules! macptr {
            () => (
                if let Some(offset) = self.offset_value() {
                    return AttributeValue::DebugMacinfoRef(DebugMacinfoOffset(offset));
                });
        }
        macro_rules! rangelistptr {
            () => (
                if let Some(offset) = self.offset_value() {
                    return AttributeValue::DebugRangesRef(DebugRangesOffset(offset));
                });
        }
        macro_rules! reference {
            () => ();
        }
        macro_rules! string {
            () => ();
        }

        // Perform the allowed class conversions for each attribute name.
        match self.name {
            constants::DW_AT_sibling => {
                reference!();
            }
            constants::DW_AT_location => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_name => {
                string!();
            }
            constants::DW_AT_ordering => {
                constant!(u8_value, Ordering, DwOrd);
            }
            constants::DW_AT_byte_size |
            constants::DW_AT_bit_offset |
            constants::DW_AT_bit_size => {
                constant!(udata_value, Udata);
                exprloc!();
                reference!();
            }
            constants::DW_AT_stmt_list => {
                lineptr!();
            }
            constants::DW_AT_low_pc => {
                address!();
            }
            constants::DW_AT_high_pc => {
                address!();
                constant!(udata_value, Udata);
            }
            constants::DW_AT_language => {
                constant!(u16_value, Language, DwLang);
            }
            constants::DW_AT_discr => {
                reference!();
            }
            constants::DW_AT_discr_value => {
                // constant: depends on type of DW_TAG_variant_part,
                // so caller must normalize.
            }
            constants::DW_AT_visibility => {
                constant!(u8_value, Visibility, DwVis);
            }
            constants::DW_AT_import => {
                reference!();
            }
            constants::DW_AT_string_length => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_common_reference => {
                reference!();
            }
            constants::DW_AT_comp_dir => {
                string!();
            }
            constants::DW_AT_const_value => {
                // TODO: constant: sign depends on DW_AT_type.
                block!();
                string!();
            }
            constants::DW_AT_containing_type => {
                reference!();
            }
            constants::DW_AT_default_value => {
                reference!();
            }
            constants::DW_AT_inline => {
                constant!(u8_value, Inline, DwInl);
            }
            constants::DW_AT_is_optional => {
                flag!();
            }
            constants::DW_AT_lower_bound => {
                // TODO: constant: sign depends on DW_AT_type.
                exprloc!();
                reference!();
            }
            constants::DW_AT_producer => {
                string!();
            }
            constants::DW_AT_prototyped => {
                flag!();
            }
            constants::DW_AT_return_addr => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_start_scope => {
                // TODO: constant
                rangelistptr!();
            }
            constants::DW_AT_bit_stride => {
                constant!(udata_value, Udata);
                exprloc!();
                reference!();
            }
            constants::DW_AT_upper_bound => {
                // TODO: constant: sign depends on DW_AT_type.
                exprloc!();
                reference!();
            }
            constants::DW_AT_abstract_origin => {
                reference!();
            }
            constants::DW_AT_accessibility => {
                constant!(u8_value, Accessibility, DwAccess);
            }
            constants::DW_AT_address_class => {
                constant!(udata_value, AddressClass, DwAddr);
            }
            constants::DW_AT_artificial => {
                flag!();
            }
            constants::DW_AT_base_types => {
                reference!();
            }
            constants::DW_AT_calling_convention => {
                constant!(u8_value, CallingConvention, DwCc);
            }
            constants::DW_AT_count => {
                // TODO: constant
                exprloc!();
                reference!();
            }
            constants::DW_AT_data_member_location => {
                // Constants must be handled before loclistptr so that DW_FORM_data4/8
                // are correctly interpreted for DWARF version 4+.
                constant!(udata_value, Udata);
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_decl_column => {
                constant!(udata_value, Udata);
            }
            constants::DW_AT_decl_file => {
                constant!(udata_value, FileIndex);
            }
            constants::DW_AT_decl_line => {
                constant!(udata_value, Udata);
            }
            constants::DW_AT_declaration => {
                flag!();
            }
            constants::DW_AT_discr_list => {
                block!();
            }
            constants::DW_AT_encoding => {
                constant!(u8_value, Encoding, DwAte);
            }
            constants::DW_AT_external => {
                flag!();
            }
            constants::DW_AT_frame_base => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_friend => {
                reference!();
            }
            constants::DW_AT_identifier_case => {
                constant!(u8_value, IdentifierCase, DwId);
            }
            constants::DW_AT_macro_info => {
                macptr!();
            }
            constants::DW_AT_namelist_item => {
                reference!();
            }
            constants::DW_AT_priority => {
                reference!();
            }
            constants::DW_AT_segment => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_specification => {
                reference!();
            }
            constants::DW_AT_static_link => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_type => {
                reference!();
            }
            constants::DW_AT_use_location => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_variable_parameter => {
                flag!();
            }
            constants::DW_AT_virtuality => {
                constant!(u8_value, Virtuality, DwVirtuality);
            }
            constants::DW_AT_vtable_elem_location => {
                exprloc!();
                loclistptr!();
            }
            constants::DW_AT_allocated => {
                // TODO: constant
                exprloc!();
                reference!();
            }
            constants::DW_AT_associated => {
                // TODO: constant
                exprloc!();
                reference!();
            }
            constants::DW_AT_data_location => {
                exprloc!();
            }
            constants::DW_AT_byte_stride => {
                constant!(udata_value, Udata);
                exprloc!();
                reference!();
            }
            constants::DW_AT_entry_pc => {
                address!();
            }
            constants::DW_AT_use_UTF8 => {
                flag!();
            }
            constants::DW_AT_extension => {
                reference!();
            }
            constants::DW_AT_ranges => {
                rangelistptr!();
            }
            constants::DW_AT_trampoline => {
                address!();
                flag!();
                reference!();
                string!();
            }
            constants::DW_AT_call_column => {
                constant!(udata_value, Udata);
            }
            constants::DW_AT_call_file => {
                constant!(udata_value, FileIndex);
            }
            constants::DW_AT_call_line => {
                constant!(udata_value, Udata);
            }
            constants::DW_AT_description => {
                string!();
            }
            constants::DW_AT_binary_scale => {
                // TODO: constant
            }
            constants::DW_AT_decimal_scale => {
                // TODO: constant
            }
            constants::DW_AT_small => {
                reference!();
            }
            constants::DW_AT_decimal_sign => {
                constant!(u8_value, DecimalSign, DwDs);
            }
            constants::DW_AT_digit_count => {
                // TODO: constant
            }
            constants::DW_AT_picture_string => {
                string!();
            }
            constants::DW_AT_mutable => {
                flag!();
            }
            constants::DW_AT_threads_scaled => {
                flag!();
            }
            constants::DW_AT_explicit => {
                flag!();
            }
            constants::DW_AT_object_pointer => {
                reference!();
            }
            constants::DW_AT_endianity => {
                constant!(u8_value, Endianity, DwEnd);
            }
            constants::DW_AT_elemental => {
                flag!();
            }
            constants::DW_AT_pure => {
                flag!();
            }
            constants::DW_AT_recursive => {
                flag!();
            }
            constants::DW_AT_signature => {
                reference!();
            }
            constants::DW_AT_main_subprogram => {
                flag!();
            }
            constants::DW_AT_data_bit_offset => {
                // TODO: constant
            }
            constants::DW_AT_const_expr => {
                flag!();
            }
            constants::DW_AT_enum_class => {
                flag!();
            }
            constants::DW_AT_linkage_name => {
                string!();
            }
            _ => {}
        }
        self.value.clone()
    }

    /// Try to convert this attribute's value to a u8.
    pub fn u8_value(&self) -> Option<u8> {
        if let Some(value) = self.udata_value() {
            if value <= u8::MAX as u64 {
                return Some(value as u8);
            }
        }
        None
    }

    /// Try to convert this attribute's value to a u16.
    pub fn u16_value(&self) -> Option<u16> {
        if let Some(value) = self.udata_value() {
            if value <= u16::MAX as u64 {
                return Some(value as u16);
            }
        }
        None
    }

    /// Try to convert this attribute's value to an unsigned integer.
    pub fn udata_value(&self) -> Option<u64> {
        Some(match self.value {
                 AttributeValue::Data1(ref data) => data[0] as u64,
                 AttributeValue::Data2((ref data, endian)) => endian.read_u16(data) as u64,
                 AttributeValue::Data4((ref data, endian)) => endian.read_u32(data) as u64,
                 AttributeValue::Data8((ref data, endian)) => endian.read_u64(data),
                 AttributeValue::Udata(data) => data,
                 _ => return None,
             })
    }

    /// Try to convert this attribute's value to a signed integer.
    pub fn sdata_value(&self) -> Option<i64> {
        Some(match self.value {
                 AttributeValue::Data1(ref data) => data[0] as i8 as i64,
                 AttributeValue::Data2((ref data, endian)) => endian.read_u16(data) as i16 as i64,
                 AttributeValue::Data4((ref data, endian)) => endian.read_u32(data) as i32 as i64,
                 AttributeValue::Data8((ref data, endian)) => endian.read_u64(data) as i64,
                 AttributeValue::Sdata(data) => data,
                 _ => return None,
             })
    }

    /// Try to convert this attribute's value to an offset.
    ///
    /// Offsets will be `Data` in DWARF version 2/3, and `SecOffset` otherwise.
    pub fn offset_value(&self) -> Option<R::Offset> {
        match self.value {
            AttributeValue::Data4((ref data, endian)) => {
                Some(R::Offset::from_u32(endian.read_u32(data)))
            }
            AttributeValue::Data8((ref data, endian)) => {
                R::Offset::from_u64(endian.read_u64(data)).ok()
            }
            AttributeValue::SecOffset(offset) => Some(offset),
            _ => None,
        }
    }

    /// Try to convert this attribute's value to an expression or location buffer.
    ///
    /// Expressions and locations may be `DW_FORM_block*` or `DW_FORM_exprloc`.
    /// The standard doesn't mention `DW_FORM_block*` as a possible form, but
    /// it is encountered in practice.
    fn exprloc_value(&self) -> Option<Expression<R>> {
        Some(match self.value {
                 AttributeValue::Block(ref data) => Expression(data.clone()),
                 AttributeValue::Exprloc(ref data) => data.clone(),
                 _ => return None,
             })
    }

    /// Try to return this attribute's value as a string slice.
    ///
    /// If this attribute's value is either an inline `DW_FORM_string` string,
    /// or a `DW_FORM_strp` reference to an offset into the `.debug_str`
    /// section, return the attribute's string value as `Some`. Other attribute
    /// value forms are returned as `None`.
    pub fn string_value(&self, debug_str: &DebugStr<R>) -> Option<R> {
        match self.value {
            AttributeValue::String(ref string) => Some(string.clone()),
            AttributeValue::DebugStrRef(offset) => debug_str.get_str(offset).ok(),
            _ => None,
        }
    }
}

fn length_u8_value<R: Reader>(input: &mut R) -> Result<R> {
    let len = input.read_u8().map(R::Offset::from_u8)?;
    input.split(len)
}

fn length_u16_value<R: Reader>(input: &mut R) -> Result<R> {
    let len = input.read_u16().map(R::Offset::from_u16)?;
    input.split(len)
}

fn length_u32_value<R: Reader>(input: &mut R) -> Result<R> {
    let len = input.read_u32().map(R::Offset::from_u32)?;
    input.split(len)
}

fn length_uleb128_value<R: Reader>(input: &mut R) -> Result<R> {
    let len = input.read_uleb128().and_then(R::Offset::from_u64)?;
    input.split(len)
}

fn parse_attribute<'unit, R: Reader>(input: &mut R,
                                     unit: &'unit UnitHeader<R, R::Offset>,
                                     spec: AttributeSpecification)
                                     -> Result<Attribute<R>> {
    let mut form = spec.form();
    loop {
        let value = match form {
            constants::DW_FORM_indirect => {
                let dynamic_form = input.read_uleb128()?;
                form = constants::DwForm(dynamic_form);
                continue;
            }
            constants::DW_FORM_addr => {
                let addr = input.read_address(unit.address_size())?;
                AttributeValue::Addr(addr)
            }
            constants::DW_FORM_block1 => {
                let block = length_u8_value(input)?;
                AttributeValue::Block(block)
            }
            constants::DW_FORM_block2 => {
                let block = length_u16_value(input)?;
                AttributeValue::Block(block)
            }
            constants::DW_FORM_block4 => {
                let block = length_u32_value(input)?;
                AttributeValue::Block(block)
            }
            constants::DW_FORM_block => {
                let block = length_uleb128_value(input)?;
                AttributeValue::Block(block)
            }
            constants::DW_FORM_data1 => {
                let data = input.read_u8_array()?;
                AttributeValue::Data1(data)
            }
            constants::DW_FORM_data2 => {
                let data = input.read_u8_array()?;
                AttributeValue::Data2((data, input.endian()))
            }
            constants::DW_FORM_data4 => {
                // DWARF version 2/3 may use DW_FORM_data4/8 for section offsets.
                // Generally we can defer interpretation of these until
                // `AttributeValue::value()`, but this is ambiguous for
                // `DW_AT_data_member_location`.
                if (unit.version() == 2 || unit.version() == 3) &&
                   spec.name() == constants::DW_AT_data_member_location {
                    let offset = input.read_u32().map(R::Offset::from_u32)?;
                    AttributeValue::SecOffset(offset)
                } else {
                    let data = input.read_u8_array()?;
                    AttributeValue::Data4((data, input.endian()))
                }
            }
            constants::DW_FORM_data8 => {
                // DWARF version 2/3 may use DW_FORM_data4/8 for section offsets.
                // Generally we can defer interpretation of these until
                // `AttributeValue::value()`, but this is ambiguous for
                // `DW_AT_data_member_location`.
                if (unit.version() == 2 || unit.version() == 3) &&
                   spec.name() == constants::DW_AT_data_member_location {
                    let offset = input.read_u64().and_then(R::Offset::from_u64)?;
                    AttributeValue::SecOffset(offset)
                } else {
                    let data = input.read_u8_array()?;
                    AttributeValue::Data8((data, input.endian()))
                }
            }
            constants::DW_FORM_udata => {
                let data = input.read_uleb128()?;
                AttributeValue::Udata(data)
            }
            constants::DW_FORM_sdata => {
                let data = input.read_sleb128()?;
                AttributeValue::Sdata(data)
            }
            constants::DW_FORM_exprloc => {
                let block = length_uleb128_value(input)?;
                AttributeValue::Exprloc(Expression(block))
            }
            constants::DW_FORM_flag => {
                let present = input.read_u8()?;
                AttributeValue::Flag(present != 0)
            }
            constants::DW_FORM_flag_present => {
                // FlagPresent is this weird compile time always true thing that
                // isn't actually present in the serialized DIEs, only in the abbreviation.
                AttributeValue::Flag(true)
            }
            constants::DW_FORM_sec_offset => {
                let offset = input.read_offset(unit.format())?;
                AttributeValue::SecOffset(offset)
            }
            constants::DW_FORM_ref1 => {
                let reference = input.read_u8().map(R::Offset::from_u8)?;
                AttributeValue::UnitRef(UnitOffset(reference))
            }
            constants::DW_FORM_ref2 => {
                let reference = input.read_u16().map(R::Offset::from_u16)?;
                AttributeValue::UnitRef(UnitOffset(reference))
            }
            constants::DW_FORM_ref4 => {
                let reference = input.read_u32().map(R::Offset::from_u32)?;
                AttributeValue::UnitRef(UnitOffset(reference))
            }
            constants::DW_FORM_ref8 => {
                let reference = input.read_u64().and_then(R::Offset::from_u64)?;
                AttributeValue::UnitRef(UnitOffset(reference))
            }
            constants::DW_FORM_ref_udata => {
                let reference = input.read_uleb128().and_then(R::Offset::from_u64)?;
                AttributeValue::UnitRef(UnitOffset(reference))
            }
            constants::DW_FORM_ref_addr => {
                // This is an offset, but DWARF version 2 specifies that DW_FORM_ref_addr
                // has the same size as an address on the target system.  This was changed
                // in DWARF version 3.
                let offset = if unit.version() == 2 {
                    input
                        .read_address(unit.address_size())
                        .and_then(R::Offset::from_u64)?
                } else {
                    input.read_offset(unit.format())?
                };
                AttributeValue::DebugInfoRef(DebugInfoOffset(offset))
            }
            constants::DW_FORM_ref_sig8 => {
                let signature = input.read_u64()?;
                AttributeValue::DebugTypesRef(DebugTypeSignature(signature))
            }
            constants::DW_FORM_string => {
                let string = input.read_null_terminated_slice()?;
                AttributeValue::String(string)
            }
            constants::DW_FORM_strp => {
                let offset = input.read_offset(unit.format())?;
                AttributeValue::DebugStrRef(DebugStrOffset(offset))
            }
            _ => {
                return Err(Error::UnknownForm);
            }
        };
        let attr = Attribute {
            name: spec.name(),
            value: value,
        };
        return Ok(attr);
    }
}

/// An iterator over a particular entry's attributes.
///
/// See [the documentation for
/// `DebuggingInformationEntry::attrs()`](./struct.DebuggingInformationEntry.html#method.attrs)
/// for details.
///
/// Can be [used with
/// `FallibleIterator`](./index.html#using-with-fallibleiterator).
#[derive(Clone, Copy, Debug)]
pub struct AttrsIter<'abbrev, 'entry, 'unit, R>
    where 'abbrev: 'entry,
          'unit: 'entry,
          R: Reader + 'entry + 'unit
{
    input: R,
    attributes: &'abbrev [AttributeSpecification],
    entry: &'entry DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>,
}

impl<'abbrev, 'entry, 'unit, R: Reader> AttrsIter<'abbrev, 'entry, 'unit, R> {
    /// Advance the iterator and return the next attribute.
    ///
    /// Returns `None` when iteration is finished. If an error
    /// occurs while parsing the next attribute, then this error
    /// is returned, and all subsequent calls return `None`.
    #[allow(inline_always)]
    #[inline(always)]
    pub fn next(&mut self) -> Result<Option<Attribute<R>>> {
        if self.attributes.is_empty() {
            // Now that we have parsed all of the attributes, we know where
            // either (1) this entry's children start, if the abbreviation says
            // this entry has children; or (2) where this entry's siblings
            // begin.
            if let Some(end) = self.entry.attrs_len.get() {
                debug_assert_eq!(end, self.input.offset_from(&self.entry.attrs_slice));
            } else {
                self.entry
                    .attrs_len
                    .set(Some(self.input.offset_from(&self.entry.attrs_slice)));
            }

            return Ok(None);
        }

        let attr = self.attributes[0];
        let rest_attr = &self.attributes[1..];
        match parse_attribute(&mut self.input, self.entry.unit, attr) {
            Ok(attr) => {
                self.attributes = rest_attr;
                Ok(Some(attr))
            }
            Err(e) => {
                self.input.empty();
                Err(e)
            }
        }
    }
}

impl<'abbrev, 'entry, 'unit, R: Reader> FallibleIterator for AttrsIter<'abbrev, 'entry, 'unit, R> {
    type Item = Attribute<R>;
    type Error = Error;

    fn next(&mut self) -> ::std::result::Result<Option<Self::Item>, Self::Error> {
        AttrsIter::next(self)
    }
}

/// A cursor into the Debugging Information Entries tree for a compilation unit.
///
/// The `EntriesCursor` can traverse the DIE tree in DFS order using `next_dfs()`,
/// or skip to the next sibling of the entry the cursor is currently pointing to
/// using `next_sibling()`.
///
/// It is also possible to traverse the DIE tree at a lower abstraction level
/// using `next_entry()`. This method does not skip over null entries, or provide
/// any indication of the current tree depth. In this case, you must use `current()`
/// to obtain the current entry, and `current().has_children()` to determine if
/// the entry following the current entry will be a sibling or child. `current()`
/// will return `None` if the current entry is a null entry, which signifies the
/// end of the current tree depth.
#[derive(Clone, Debug)]
pub struct EntriesCursor<'abbrev, 'unit, R>
    where R: Reader + 'unit
{
    input: R,
    unit: &'unit UnitHeader<R, R::Offset>,
    abbreviations: &'abbrev Abbreviations,
    cached_current: Option<DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>>,
    delta_depth: isize,
}

impl<'abbrev, 'unit, R: Reader> EntriesCursor<'abbrev, 'unit, R> {
    /// Get a reference to the entry that the cursor is currently pointing to.
    ///
    /// If the cursor is not pointing at an entry, or if the current entry is a
    /// null entry, then `None` is returned.
    #[inline]
    pub fn current(&self) -> Option<&DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>> {
        self.cached_current.as_ref()
    }

    /// Move the cursor to the next DIE in the tree.
    ///
    /// Returns `Some` if there is a next entry, even if this entry is null.
    /// If there is no next entry, then `None` is returned.
    pub fn next_entry(&mut self) -> Result<Option<()>> {
        if let Some(ref current) = self.cached_current {
            self.input = current.after_attrs()?;
        }

        if self.input.is_empty() {
            self.cached_current = None;
            self.delta_depth = 0;
            return Ok(None);
        }

        match DebuggingInformationEntry::parse(&mut self.input, self.unit, self.abbreviations) {
            Ok(Some(entry)) => {
                self.delta_depth = entry.has_children() as isize;
                self.cached_current = Some(entry);
                Ok(Some(()))
            }
            Ok(None) => {
                self.delta_depth = -1;
                self.cached_current = None;
                Ok(Some(()))
            }
            Err(e) => {
                self.input.empty();
                self.delta_depth = 0;
                self.cached_current = None;
                Err(e)
            }
        }
    }

    /// Move the cursor to the next DIE in the tree in DFS order.
    ///
    /// Upon successful movement of the cursor, return the delta traversal
    /// depth and the entry:
    ///
    ///   * If we moved down into the previous current entry's children, we get
    ///     `Some((1, entry))`.
    ///
    ///   * If we moved to the previous current entry's sibling, we get
    ///     `Some((0, entry))`.
    ///
    ///   * If the previous entry does not have any siblings and we move up to
    ///     its parent's next sibling, then we get `Some((-1, entry))`. Note that
    ///     if the parent doesn't have a next sibling, then it could go up to the
    ///     parent's parent's next sibling and return `Some((-2, entry))`, etc.
    ///
    /// If there is no next entry, then `None` is returned.
    ///
    /// Here is an example that finds the first entry in a compilation unit that
    /// does not have any children.
    ///
    /// ```
    /// # use gimli::{DebugAbbrev, DebugInfo, LittleEndian};
    /// # let info_buf = [
    /// #     // Comilation unit header
    /// #
    /// #     // 32-bit unit length = 25
    /// #     0x19, 0x00, 0x00, 0x00,
    /// #     // Version 4
    /// #     0x04, 0x00,
    /// #     // debug_abbrev_offset
    /// #     0x00, 0x00, 0x00, 0x00,
    /// #     // Address size
    /// #     0x04,
    /// #
    /// #     // DIEs
    /// #
    /// #     // Abbreviation code
    /// #     0x01,
    /// #     // Attribute of form DW_FORM_string = "foo\0"
    /// #     0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #       // Children
    /// #
    /// #       // Abbreviation code
    /// #       0x01,
    /// #       // Attribute of form DW_FORM_string = "foo\0"
    /// #       0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #         // Children
    /// #
    /// #         // Abbreviation code
    /// #         0x01,
    /// #         // Attribute of form DW_FORM_string = "foo\0"
    /// #         0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #           // Children
    /// #
    /// #           // End of children
    /// #           0x00,
    /// #
    /// #         // End of children
    /// #         0x00,
    /// #
    /// #       // End of children
    /// #       0x00,
    /// # ];
    /// # let debug_info = DebugInfo::new(&info_buf, LittleEndian);
    /// #
    /// # let abbrev_buf = [
    /// #     // Code
    /// #     0x01,
    /// #     // DW_TAG_subprogram
    /// #     0x2e,
    /// #     // DW_CHILDREN_yes
    /// #     0x01,
    /// #     // Begin attributes
    /// #       // Attribute name = DW_AT_name
    /// #       0x03,
    /// #       // Attribute form = DW_FORM_string
    /// #       0x08,
    /// #     // End attributes
    /// #     0x00,
    /// #     0x00,
    /// #     // Null terminator
    /// #     0x00
    /// # ];
    /// # let debug_abbrev = DebugAbbrev::new(&abbrev_buf, LittleEndian);
    /// #
    /// # let get_some_unit = || debug_info.units().next().unwrap().unwrap();
    ///
    /// let unit = get_some_unit();
    /// # let get_abbrevs_for_unit = |_| unit.abbreviations(&debug_abbrev).unwrap();
    /// let abbrevs = get_abbrevs_for_unit(&unit);
    ///
    /// let mut first_entry_with_no_children = None;
    /// let mut cursor = unit.entries(&abbrevs);
    ///
    /// // Move the cursor to the root.
    /// assert!(cursor.next_dfs().unwrap().is_some());
    ///
    /// // Keep looping while the cursor is moving deeper into the DIE tree.
    /// while let Some((delta_depth, current)) = cursor.next_dfs().expect("Should parse next dfs") {
    ///     // 0 means we moved to a sibling, a negative number means we went back
    ///     // up to a parent's sibling. In either case, bail out of the loop because
    ///     //  we aren't going deeper into the tree anymore.
    ///     if delta_depth <= 0 {
    ///         break;
    ///     }
    ///
    ///     first_entry_with_no_children = Some(current.clone());
    /// }
    ///
    /// println!("The first entry with no children is {:?}",
    ///          first_entry_with_no_children.unwrap());
    /// ```
    #[allow(type_complexity)]
    pub fn next_dfs
        (&mut self)
         -> Result<Option<(isize, &DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>)>> {
        let mut delta_depth = self.delta_depth;
        loop {
            // The next entry should be the one we want.
            if self.next_entry()?.is_some() {
                if let Some(ref entry) = self.cached_current {
                    return Ok(Some((delta_depth, entry)));
                }

                // next_entry() read a null entry.
                delta_depth += self.delta_depth;
            } else {
                return Ok(None);
            }
        }
    }

    /// Move the cursor to the next sibling DIE of the current one.
    ///
    /// Returns `Ok(Some(entry))` when the cursor has been moved to
    /// the next sibling, `Ok(None)` when there is no next sibling.
    ///
    /// The depth of the cursor is never changed if this method returns `Ok`.
    /// Once `Ok(None)` is returned, this method will continue to return
    /// `Ok(None)` until either `next_entry` or `next_dfs` is called.
    ///
    /// Here is an example that iterates over all of the direct children of the
    /// root entry:
    ///
    /// ```
    /// # use gimli::{DebugAbbrev, DebugInfo, LittleEndian};
    /// # let info_buf = [
    /// #     // Comilation unit header
    /// #
    /// #     // 32-bit unit length = 25
    /// #     0x19, 0x00, 0x00, 0x00,
    /// #     // Version 4
    /// #     0x04, 0x00,
    /// #     // debug_abbrev_offset
    /// #     0x00, 0x00, 0x00, 0x00,
    /// #     // Address size
    /// #     0x04,
    /// #
    /// #     // DIEs
    /// #
    /// #     // Abbreviation code
    /// #     0x01,
    /// #     // Attribute of form DW_FORM_string = "foo\0"
    /// #     0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #       // Children
    /// #
    /// #       // Abbreviation code
    /// #       0x01,
    /// #       // Attribute of form DW_FORM_string = "foo\0"
    /// #       0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #         // Children
    /// #
    /// #         // Abbreviation code
    /// #         0x01,
    /// #         // Attribute of form DW_FORM_string = "foo\0"
    /// #         0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #           // Children
    /// #
    /// #           // End of children
    /// #           0x00,
    /// #
    /// #         // End of children
    /// #         0x00,
    /// #
    /// #       // End of children
    /// #       0x00,
    /// # ];
    /// # let debug_info = DebugInfo::new(&info_buf, LittleEndian);
    /// #
    /// # let get_some_unit = || debug_info.units().next().unwrap().unwrap();
    ///
    /// # let abbrev_buf = [
    /// #     // Code
    /// #     0x01,
    /// #     // DW_TAG_subprogram
    /// #     0x2e,
    /// #     // DW_CHILDREN_yes
    /// #     0x01,
    /// #     // Begin attributes
    /// #       // Attribute name = DW_AT_name
    /// #       0x03,
    /// #       // Attribute form = DW_FORM_string
    /// #       0x08,
    /// #     // End attributes
    /// #     0x00,
    /// #     0x00,
    /// #     // Null terminator
    /// #     0x00
    /// # ];
    /// # let debug_abbrev = DebugAbbrev::new(&abbrev_buf, LittleEndian);
    /// #
    /// let unit = get_some_unit();
    /// # let get_abbrevs_for_unit = |_| unit.abbreviations(&debug_abbrev).unwrap();
    /// let abbrevs = get_abbrevs_for_unit(&unit);
    ///
    /// let mut cursor = unit.entries(&abbrevs);
    ///
    /// // Move the cursor to the root.
    /// assert!(cursor.next_dfs().unwrap().is_some());
    ///
    /// // Move the cursor to the root's first child.
    /// assert!(cursor.next_dfs().unwrap().is_some());
    ///
    /// // Iterate the root's children.
    /// loop {
    ///     {
    ///         let current = cursor.current().expect("Should be at an entry");
    ///         println!("{:?} is a child of the root", current);
    ///     }
    ///
    ///     if cursor.next_sibling().expect("Should parse next sibling").is_none() {
    ///         break;
    ///     }
    /// }
    /// ```
    pub fn next_sibling
        (&mut self)
         -> Result<Option<(&DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>)>> {
        if self.current().is_none() {
            // We're already at the null for the end of the sibling list.
            return Ok(None);
        }

        // Loop until we find an entry at the current level.
        let mut depth = 0;
        loop {
            // Use is_some() and unwrap() to keep borrow checker happy.
            if self.current().is_some() && self.current().unwrap().has_children() {
                if let Some(sibling_input) = self.current().unwrap().sibling() {
                    // Fast path: this entry has a DW_AT_sibling
                    // attribute pointing to its sibling, so jump
                    // to it (which keeps us at the same depth).
                    self.input = sibling_input;
                    self.cached_current = None;
                } else {
                    // This entry has children, so the next entry is
                    // down one level.
                    depth += 1;
                }
            }

            if self.next_entry()?.is_none() {
                // End of input.
                return Ok(None);
            }

            if depth == 0 {
                // Found an entry at the current level.
                return Ok(self.current());
            }

            if self.current().is_none() {
                // A null entry means the end of a child list, so we're
                // back up a level.
                depth -= 1;
            }
        }
    }
}

/// The state information for a tree view of the Debugging Information Entries.
///
/// The `EntriesTree` can be used to recursively iterate through the DIE
/// tree, following the parent/child relationships. The `EntriesTree` contains
/// shared state for all nodes in the tree, avoiding any duplicate parsing of
/// entries during the traversal.
///
/// ## Example Usage
/// ```rust,no_run
/// extern crate gimli;
///
/// # fn example() -> Result<(), gimli::Error> {
/// # let debug_info = gimli::DebugInfo::new(&[], gimli::LittleEndian);
/// # let get_some_unit = || debug_info.units().next().unwrap().unwrap();
/// let unit = get_some_unit();
/// # let debug_abbrev = gimli::DebugAbbrev::new(&[], gimli::LittleEndian);
/// # let get_abbrevs_for_unit = |_| unit.abbreviations(&debug_abbrev).unwrap();
/// let abbrevs = get_abbrevs_for_unit(&unit);
///
/// let mut tree = unit.entries_tree(&abbrevs, None)?;
/// let root = tree.root()?;
/// process_tree(root)?;
/// # unreachable!()
/// # }
///
/// fn process_tree<R>(mut node: gimli::EntriesTreeNode<R>) -> gimli::Result<()>
///     where R: gimli::Reader
/// {
///     {
///         // Examine the entry attributes.
///         let mut attrs = node.entry().attrs();
///         while let Some(attr) = attrs.next()? {
///         }
///     }
///     let mut children = node.children();
///     while let Some(child) = children.next()? {
///         // Recursively process a child.
///         process_tree(child);
///     }
///     Ok(())
/// }
/// ```
#[derive(Clone, Debug)]
pub struct EntriesTree<'abbrev, 'unit, R>
    where R: Reader + 'unit
{
    root: R,
    unit: &'unit UnitHeader<R, R::Offset>,
    abbreviations: &'abbrev Abbreviations,
    input: R,
    entry: Option<DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset>>,
    depth: isize,
}

impl<'abbrev, 'unit, R: Reader> EntriesTree<'abbrev, 'unit, R> {
    fn new(root: R,
           unit: &'unit UnitHeader<R, R::Offset>,
           abbreviations: &'abbrev Abbreviations)
           -> Self {
        let input = root.clone();
        EntriesTree {
            root,
            unit,
            abbreviations,
            input,
            entry: None,
            depth: 0,
        }
    }

    /// Returns the root node of the tree.
    pub fn root<'me>(&'me mut self) -> Result<EntriesTreeNode<'abbrev, 'unit, 'me, R>> {
        self.input = self.root.clone();
        self.entry =
            DebuggingInformationEntry::parse(&mut self.input, self.unit, self.abbreviations)?;
        if self.entry.is_none() {
            return Err(Error::UnexpectedNull);
        }
        self.depth = 0;
        Ok(EntriesTreeNode::new(self, 1))
    }

    /// Move the cursor to the next entry at the specified depth.
    ///
    /// Requires `depth <= self.depth + 1`.
    ///
    /// Returns `true` if successful.
    fn next(&mut self, depth: isize) -> Result<bool> {
        if self.depth < depth {
            debug_assert_eq!(self.depth + 1, depth);

            match self.entry {
                Some(ref entry) => {
                    if !entry.has_children() {
                        return Ok(false);
                    }
                    self.depth += 1;
                    self.input = entry.after_attrs()?;
                }
                None => return Ok(false),
            }

            if self.input.is_empty() {
                self.entry = None;
                return Ok(false);
            }

            return match DebuggingInformationEntry::parse(&mut self.input,
                                                          self.unit,
                                                          self.abbreviations) {
                Ok(entry) => {
                    self.entry = entry;
                    Ok(self.entry.is_some())
                }
                Err(e) => {
                    self.input.empty();
                    self.entry = None;
                    Err(e)
                }
            };
        }

        loop {
            match self.entry {
                Some(ref entry) => {
                    if entry.has_children() {
                        if let Some(sibling_input) = entry.sibling() {
                            // Fast path: this entry has a DW_AT_sibling
                            // attribute pointing to its sibling, so jump
                            // to it (which keeps us at the same depth).
                            self.input = sibling_input;
                        } else {
                            // This entry has children, so the next entry is
                            // down one level.
                            self.depth += 1;
                            self.input = entry.after_attrs()?;
                        }
                    } else {
                        // This entry has no children, so next entry is at same depth.
                        self.input = entry.after_attrs()?;
                    }
                }
                None => {
                    // This entry is a null, so next entry is up one level.
                    self.depth -= 1;
                }
            }

            if self.input.is_empty() {
                self.entry = None;
                return Ok(false);
            }

            match DebuggingInformationEntry::parse(&mut self.input, self.unit, self.abbreviations) {
                Ok(entry) => {
                    self.entry = entry;
                    if self.depth == depth {
                        return Ok(self.entry.is_some());
                    }
                }
                Err(e) => {
                    self.input.empty();
                    self.entry = None;
                    return Err(e);
                }
            }
        }
    }
}

/// A node in the Debugging Information Entry tree.
///
/// The root node of a tree can be obtained
/// via [`EntriesTree::root`](./struct.EntriesTree.html#method.root).
#[derive(Debug)]
pub struct EntriesTreeNode<'abbrev, 'unit, 'tree, R>
    where 'abbrev: 'tree,
          'unit: 'tree,
          R: Reader + 'unit
{
    tree: &'tree mut EntriesTree<'abbrev, 'unit, R>,
    depth: isize,
}

impl<'abbrev, 'unit, 'tree, R: Reader> EntriesTreeNode<'abbrev, 'unit, 'tree, R> {
    fn new(tree: &'tree mut EntriesTree<'abbrev, 'unit, R>,
           depth: isize)
           -> EntriesTreeNode<'abbrev, 'unit, 'tree, R> {
        debug_assert!(tree.entry.is_some());
        EntriesTreeNode {
            tree: tree,
            depth: depth,
        }
    }

    /// Returns the current entry in the tree.
    pub fn entry(&self) -> &DebuggingInformationEntry<'abbrev, 'unit, R, R::Offset> {
        // We never create a node without an entry.
        self.tree.entry.as_ref().unwrap()
    }

    /// Create an iterator for the children of the current entry.
    ///
    /// The current entry can no longer be accessed after creating the
    /// iterator.
    pub fn children(self) -> EntriesTreeIter<'abbrev, 'unit, 'tree, R> {
        EntriesTreeIter::new(self.tree, self.depth)
    }
}


/// An iterator that allows traversal of the children of an
/// `EntriesTreeNode`.
///
/// The items returned by this iterator are also `EntriesTreeNode`s,
/// which allow recursive traversal of grandchildren, etc.
#[derive(Debug)]
pub struct EntriesTreeIter<'abbrev, 'unit, 'tree, R>
    where 'abbrev: 'tree,
          'unit: 'tree,
          R: Reader + 'unit
{
    tree: &'tree mut EntriesTree<'abbrev, 'unit, R>,
    depth: isize,
    empty: bool,
}

impl<'abbrev, 'unit, 'tree, R: Reader> EntriesTreeIter<'abbrev, 'unit, 'tree, R> {
    fn new(tree: &'tree mut EntriesTree<'abbrev, 'unit, R>,
           depth: isize)
           -> EntriesTreeIter<'abbrev, 'unit, 'tree, R> {
        EntriesTreeIter {
            tree: tree,
            depth: depth,
            empty: false,
        }
    }

    /// Returns an `EntriesTreeNode` for the next child entry.
    ///
    /// Returns `None` if there are no more children.
    pub fn next<'me>(&'me mut self) -> Result<Option<EntriesTreeNode<'abbrev, 'unit, 'me, R>>> {
        if self.empty {
            Ok(None)
        } else if self.tree.next(self.depth)? {
            Ok(Some(EntriesTreeNode::new(self.tree, self.depth + 1)))
        } else {
            self.empty = true;
            Ok(None)
        }
    }
}

/// Parse a type unit header's unique type signature. Callers should handle
/// unique-ness checking.
fn parse_type_signature<R: Reader>(input: &mut R) -> Result<DebugTypeSignature> {
    input.read_u64().map(DebugTypeSignature)
}

/// Parse a type unit header's type offset.
fn parse_type_offset<R: Reader>(input: &mut R, format: Format) -> Result<UnitOffset<R::Offset>> {
    input.read_offset(format).map(UnitOffset)
}

/// The `DebugTypes` struct represents the DWARF type information
/// found in the `.debug_types` section.
#[derive(Debug, Clone, Copy)]
pub struct DebugTypes<R: Reader> {
    debug_types_section: R,
}

impl<'input, Endian> DebugTypes<EndianBuf<'input, Endian>>
    where Endian: Endianity
{
    /// Construct a new `DebugTypes` instance from the data in the `.debug_types`
    /// section.
    ///
    /// It is the caller's responsibility to read the `.debug_types` section and
    /// present it as a `&[u8]` slice. That means using some ELF loader on
    /// Linux, a Mach-O loader on OSX, etc.
    ///
    /// ```
    /// use gimli::{DebugTypes, LittleEndian};
    ///
    /// # let buf = [0x00, 0x01, 0x02, 0x03];
    /// # let read_debug_types_section_somehow = || &buf;
    /// let debug_types = DebugTypes::new(read_debug_types_section_somehow(), LittleEndian);
    /// ```
    pub fn new(debug_types_section: &'input [u8], endian: Endian) -> Self {
        Self::from(EndianBuf::new(debug_types_section, endian))
    }
}

impl<R: Reader> Section<R> for DebugTypes<R> {
    fn section_name() -> &'static str {
        ".debug_types"
    }
}

impl<R: Reader> From<R> for DebugTypes<R> {
    fn from(debug_types_section: R) -> Self {
        DebugTypes { debug_types_section }
    }
}


impl<R: Reader> DebugTypes<R> {
    /// Iterate the type-units in this `.debug_types` section.
    ///
    /// ```
    /// use gimli::{DebugTypes, LittleEndian};
    ///
    /// # let buf = [];
    /// # let read_debug_types_section_somehow = || &buf;
    /// let debug_types = DebugTypes::new(read_debug_types_section_somehow(), LittleEndian);
    ///
    /// let mut iter = debug_types.units();
    /// while let Some(unit) = iter.next().unwrap() {
    ///     println!("unit's length is {}", unit.unit_length());
    /// }
    /// ```
    ///
    /// Can be [used with
    /// `FallibleIterator`](./index.html#using-with-fallibleiterator).
    pub fn units(&self) -> TypeUnitHeadersIter<R> {
        TypeUnitHeadersIter {
            input: self.debug_types_section.clone(),
            offset: DebugTypesOffset(R::Offset::from_u8(0)),
        }
    }
}

/// An iterator over the type-units of this `.debug_types` section.
///
/// See the [documentation on
/// `DebugTypes::units`](./struct.DebugTypes.html#method.units) for
/// more detail.
#[derive(Clone, Debug)]
pub struct TypeUnitHeadersIter<R: Reader> {
    input: R,
    offset: DebugTypesOffset<R::Offset>,
}

impl<R: Reader> TypeUnitHeadersIter<R> {
    /// Advance the iterator to the next type unit header.
    pub fn next(&mut self) -> Result<Option<TypeUnitHeader<R, R::Offset>>> {
        if self.input.is_empty() {
            Ok(None)
        } else {
            let len = self.input.len();
            match parse_type_unit_header(&mut self.input, self.offset) {
                Ok(header) => {
                    self.offset.0 += len - self.input.len();
                    Ok(Some(header))
                }
                Err(e) => {
                    self.input.empty();
                    Err(e)
                }
            }
        }
    }
}

impl<R: Reader> FallibleIterator for TypeUnitHeadersIter<R> {
    type Item = TypeUnitHeader<R, R::Offset>;
    type Error = Error;

    fn next(&mut self) -> ::std::result::Result<Option<Self::Item>, Self::Error> {
        TypeUnitHeadersIter::next(self)
    }
}

/// The header of a type unit's debugging information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeUnitHeader<R, Offset = usize>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    header: UnitHeader<R, Offset>,
    offset: DebugTypesOffset<Offset>,
    type_signature: DebugTypeSignature,
    type_offset: UnitOffset<Offset>,
}

impl<R, Offset> TypeUnitHeader<R, Offset>
    where R: Reader<Offset = Offset>,
          Offset: ReaderOffset
{
    /// Construct a new `TypeUnitHeader`.
    fn new(header: UnitHeader<R, R::Offset>,
           offset: DebugTypesOffset<R::Offset>,
           type_signature: DebugTypeSignature,
           type_offset: UnitOffset<R::Offset>)
           -> Self {
        TypeUnitHeader {
            header: header,
            offset: offset,
            type_signature: type_signature,
            type_offset: type_offset,
        }
    }

    /// Return the serialized size of the type-unit header for the given
    /// DWARF format.
    pub fn size_of_header(format: Format) -> usize {
        let unit_header_size = UnitHeader::<R, _>::size_of_header(format);
        let type_signature_size = 8;
        let type_offset_size = match format {
            Format::Dwarf32 => 4,
            Format::Dwarf64 => 8,
        };
        unit_header_size + type_signature_size + type_offset_size
    }

    /// Get the offset of this compilation unit within the .debug_info section.
    pub fn offset(&self) -> DebugTypesOffset<R::Offset> {
        self.offset
    }

    /// Get the length of the debugging info for this type-unit.
    pub fn unit_length(&self) -> R::Offset {
        self.header.unit_length
    }

    /// Get the length of the debugging info for this type-unit,
    /// including the byte length of the encoded length itself.
    pub fn length_including_self(&self) -> R::Offset {
        self.header.length_including_self()
    }

    /// Get the DWARF version of the debugging info for this type-unit.
    pub fn version(&self) -> u16 {
        self.header.version
    }

    /// The offset into the `.debug_abbrev` section for this type-unit's
    /// debugging information entries.
    pub fn debug_abbrev_offset(&self) -> DebugAbbrevOffset<R::Offset> {
        self.header.debug_abbrev_offset
    }

    /// The size of addresses (in bytes) in this type-unit.
    pub fn address_size(&self) -> u8 {
        self.header.address_size
    }

    /// Whether this type unit is encoded in 64- or 32-bit DWARF.
    pub fn format(&self) -> Format {
        self.header.format
    }

    /// The serialized size of the header for this type-unit.
    pub fn header_size(&self) -> R::Offset {
        self.header.header_size()
    }

    /// Get the unique type signature for this type unit.
    pub fn type_signature(&self) -> DebugTypeSignature {
        self.type_signature
    }

    /// Get the offset within this type unit where the type is defined.
    pub fn type_offset(&self) -> UnitOffset<R::Offset> {
        self.type_offset
    }

    /// Navigate this type unit's `DebuggingInformationEntry`s.
    pub fn entries<'me, 'abbrev>(&'me self,
                                 abbreviations: &'abbrev Abbreviations)
                                 -> EntriesCursor<'abbrev, 'me, R> {
        self.header.entries(abbreviations)
    }

    /// Navigate this type unit's `DebuggingInformationEntry`s
    /// starting at the given offset.
    pub fn entries_at_offset<'me, 'abbrev>(&'me self,
                                           abbreviations: &'abbrev Abbreviations,
                                           offset: UnitOffset<R::Offset>)
                                           -> Result<EntriesCursor<'abbrev, 'me, R>> {
        self.header.entries_at_offset(abbreviations, offset)
    }

    /// Navigate this type unit's `DebuggingInformationEntry`s as a tree
    /// starting at the given offset.
    pub fn entries_tree<'me, 'abbrev>(&'me self,
                                      abbreviations: &'abbrev Abbreviations,
                                      offset: Option<UnitOffset<R::Offset>>)
                                      -> Result<EntriesTree<'abbrev, 'me, R>> {
        self.header.entries_tree(abbreviations, offset)
    }

    /// Parse this type unit's abbreviations.
    ///
    /// ```
    /// use gimli::DebugAbbrev;
    /// # use gimli::{DebugTypes, LittleEndian};
    /// # let types_buf = [
    /// #     // Type unit header
    /// #
    /// #     // 32-bit unit length = 37
    /// #     0x25, 0x00, 0x00, 0x00,
    /// #     // Version 4
    /// #     0x04, 0x00,
    /// #     // debug_abbrev_offset
    /// #     0x00, 0x00, 0x00, 0x00,
    /// #     // Address size
    /// #     0x04,
    /// #     // Type signature
    /// #     0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
    /// #     // Type offset
    /// #     0x01, 0x02, 0x03, 0x04,
    /// #
    /// #     // DIEs
    /// #
    /// #     // Abbreviation code
    /// #     0x01,
    /// #     // Attribute of form DW_FORM_string = "foo\0"
    /// #     0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #       // Children
    /// #
    /// #       // Abbreviation code
    /// #       0x01,
    /// #       // Attribute of form DW_FORM_string = "foo\0"
    /// #       0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #         // Children
    /// #
    /// #         // Abbreviation code
    /// #         0x01,
    /// #         // Attribute of form DW_FORM_string = "foo\0"
    /// #         0x66, 0x6f, 0x6f, 0x00,
    /// #
    /// #           // Children
    /// #
    /// #           // End of children
    /// #           0x00,
    /// #
    /// #         // End of children
    /// #         0x00,
    /// #
    /// #       // End of children
    /// #       0x00,
    /// # ];
    /// # let debug_types = DebugTypes::new(&types_buf, LittleEndian);
    /// #
    /// # let abbrev_buf = [
    /// #     // Code
    /// #     0x01,
    /// #     // DW_TAG_subprogram
    /// #     0x2e,
    /// #     // DW_CHILDREN_yes
    /// #     0x01,
    /// #     // Begin attributes
    /// #       // Attribute name = DW_AT_name
    /// #       0x03,
    /// #       // Attribute form = DW_FORM_string
    /// #       0x08,
    /// #     // End attributes
    /// #     0x00,
    /// #     0x00,
    /// #     // Null terminator
    /// #     0x00
    /// # ];
    /// #
    /// # let get_some_type_unit = || debug_types.units().next().unwrap().unwrap();
    ///
    /// let unit = get_some_type_unit();
    ///
    /// # let read_debug_abbrev_section_somehow = || &abbrev_buf;
    /// let debug_abbrev = DebugAbbrev::new(read_debug_abbrev_section_somehow(), LittleEndian);
    /// let abbrevs_for_unit = unit.abbreviations(&debug_abbrev).unwrap();
    /// ```
    pub fn abbreviations(&self, debug_abbrev: &DebugAbbrev<R>) -> Result<Abbreviations> {
        self.header.abbreviations(debug_abbrev)
    }
}

/// Parse a type unit header.
fn parse_type_unit_header<R: Reader>(input: &mut R,
                                     offset: DebugTypesOffset<R::Offset>)
                                     -> Result<TypeUnitHeader<R, R::Offset>> {
    let mut header = parse_unit_header(input)?;
    let format = header.format();
    let signature = parse_type_signature(&mut header.entries_buf)?;
    let type_offset = parse_type_offset(&mut header.entries_buf, format)?;
    Ok(TypeUnitHeader::new(header, offset, signature, type_offset))
}

#[cfg(test)]
mod tests {
    extern crate test_assembler;

    use super::*;
    use super::{parse_version, parse_debug_abbrev_offset, parse_type_offset, parse_unit_header,
                parse_type_unit_header, parse_attribute};
    use abbrev::{DebugAbbrev, DebugAbbrevOffset, Abbreviation, AttributeSpecification};
    use abbrev::tests::AbbrevSectionMethods;
    use constants;
    use constants::*;
    use endianity::{EndianBuf, Endianity, LittleEndian};
    use leb128;
    use loc::DebugLocOffset;
    use parser::{Error, Format, Result};
    use self::test_assembler::{Endian, Label, LabelMaker, Section};
    use str::DebugStrOffset;
    use std;
    use std::cell::Cell;
    use test_util::GimliSectionMethods;

    // Mixin methods for `Section` to help define binary test data.

    trait UnitSectionMethods {
        fn comp_unit<'input, E>(self,
                                unit: &mut CompilationUnitHeader<EndianBuf<'input, E>>)
                                -> Self
            where E: Endianity;
        fn type_unit<'input, E>(self, unit: &mut TypeUnitHeader<EndianBuf<'input, E>>) -> Self
            where E: Endianity;
        fn unit<'input, E>(self,
                           unit: &mut UnitHeader<EndianBuf<'input, E>>,
                           extra_header: &[u8])
                           -> Self
            where E: Endianity;
        fn die<F>(self, code: u64, attr: F) -> Self where F: Fn(Section) -> Section;
        fn die_null(self) -> Self;
        fn attr_string(self, s: &str) -> Self;
        fn attr_ref1(self, o: u8) -> Self;
        fn offset(self, offset: usize, format: Format) -> Self;
    }

    impl UnitSectionMethods for Section {
        fn comp_unit<'input, E>(self,
                                unit: &mut CompilationUnitHeader<EndianBuf<'input, E>>)
                                -> Self
            where E: Endianity
        {
            unit.offset = DebugInfoOffset(self.size() as usize);
            self.unit(&mut unit.header, &[])
        }

        fn type_unit<'input, E>(self, unit: &mut TypeUnitHeader<EndianBuf<'input, E>>) -> Self
            where E: Endianity
        {
            unit.offset = DebugTypesOffset(self.size() as usize);
            let section = Section::with_endian(Endian::Little)
                .L64(unit.type_signature.0)
                .offset(unit.type_offset.0, unit.header.format);
            let extra_header = section.get_contents().unwrap();
            self.unit(&mut unit.header, &extra_header)
        }

        fn unit<'input, E>(self,
                           unit: &mut UnitHeader<EndianBuf<'input, E>>,
                           extra_header: &[u8])
                           -> Self
            where E: Endianity
        {
            let length = Label::new();
            let start = Label::new();
            let end = Label::new();

            let section = match unit.format {
                Format::Dwarf32 => self.L32(&length),
                Format::Dwarf64 => self.L32(0xffffffff).L64(&length),
            };

            let section = section
                .mark(&start)
                .L16(unit.version)
                .offset(unit.debug_abbrev_offset.0, unit.format)
                .D8(unit.address_size)
                .append_bytes(extra_header)
                .append_bytes(unit.entries_buf.into())
                .mark(&end);

            unit.unit_length = (&end - &start) as usize;
            length.set_const(unit.unit_length as u64);

            section
        }

        fn die<F>(self, code: u64, attr: F) -> Self
            where F: Fn(Section) -> Section
        {
            let section = self.uleb(code);
            attr(section)
        }

        fn die_null(self) -> Self {
            self.D8(0)
        }

        fn attr_string(self, attr: &str) -> Self {
            self.append_bytes(attr.as_bytes()).D8(0)
        }

        fn attr_ref1(self, attr: u8) -> Self {
            self.D8(attr)
        }

        fn offset(self, offset: usize, format: Format) -> Self {
            match format {
                Format::Dwarf32 => self.L32(offset as u32),
                Format::Dwarf64 => self.L64(offset as u64),
            }
        }
    }

    #[test]
    fn test_parse_debug_abbrev_offset_32() {
        let section = Section::with_endian(Endian::Little).L32(0x04030201);
        let buf = section.get_contents().unwrap();
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_abbrev_offset(buf, Format::Dwarf32) {
            Ok(val) => assert_eq!(val, DebugAbbrevOffset(0x04030201)),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_parse_debug_abbrev_offset_32_incomplete() {
        let buf = [0x01, 0x02];
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_abbrev_offset(buf, Format::Dwarf32) {
            Err(Error::UnexpectedEof) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_debug_abbrev_offset_64() {
        let section = Section::with_endian(Endian::Little).L64(0x0807060504030201);
        let buf = section.get_contents().unwrap();
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_abbrev_offset(buf, Format::Dwarf64) {
            Ok(val) => assert_eq!(val, DebugAbbrevOffset(0x0807060504030201)),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_parse_debug_abbrev_offset_64_incomplete() {
        let buf = [0x01, 0x02];
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_abbrev_offset(buf, Format::Dwarf64) {
            Err(Error::UnexpectedEof) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_parse_debug_info_offset_32() {
        let section = Section::with_endian(Endian::Little).L32(0x04030201);
        let buf = section.get_contents().unwrap();
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_info_offset(buf, Format::Dwarf32) {
            Ok(val) => assert_eq!(val, DebugInfoOffset(0x04030201)),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_parse_debug_info_offset_32_incomplete() {
        let buf = [0x01, 0x02];
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_info_offset(buf, Format::Dwarf32) {
            Err(Error::UnexpectedEof) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_debug_info_offset_64() {
        let section = Section::with_endian(Endian::Little).L64(0x0807060504030201);
        let buf = section.get_contents().unwrap();
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_info_offset(buf, Format::Dwarf64) {
            Ok(val) => assert_eq!(val, DebugInfoOffset(0x0807060504030201)),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_parse_debug_info_offset_64_incomplete() {
        let buf = [0x01, 0x02];
        let buf = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_debug_info_offset(buf, Format::Dwarf64) {
            Err(Error::UnexpectedEof) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_units() {
        let expected_rest = &[1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut unit64 = CompilationUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0x0102030405060708),
                address_size: 8,
                format: Format::Dwarf64,
                entries_buf: EndianBuf::new(expected_rest, LittleEndian),
            },
            offset: DebugInfoOffset(0),
        };
        let mut unit32 = CompilationUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0x08070605),
                address_size: 4,
                format: Format::Dwarf32,
                entries_buf: EndianBuf::new(expected_rest, LittleEndian),
            },
            offset: DebugInfoOffset(0),
        };
        let section = Section::with_endian(Endian::Little)
            .comp_unit(&mut unit64)
            .comp_unit(&mut unit32);
        let buf = section.get_contents().unwrap();

        let debug_info = DebugInfo::new(&buf, LittleEndian);
        let mut units = debug_info.units();

        assert_eq!(units.next(), Ok(Some(unit64)));
        assert_eq!(units.next(), Ok(Some(unit32)));
        assert_eq!(units.next(), Ok(None));
    }

    #[test]
    fn test_unit_version_ok() {
        // Version 4 and two extra bytes
        let buf = [0x04, 0x00, 0xff, 0xff];
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_version(rest) {
            Ok(val) => {
                assert_eq!(val, 4);
                assert_eq!(*rest, EndianBuf::new(&[0xff, 0xff], LittleEndian));
            }
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_unit_version_unknown_version() {
        let buf = [0xab, 0xcd];
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_version(rest) {
            Err(Error::UnknownVersion) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };

        let buf = [0x1, 0x0];
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_version(rest) {
            Err(Error::UnknownVersion) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_unit_version_incomplete() {
        let buf = [0x04];
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_version(rest) {
            Err(Error::UnexpectedEof) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    fn test_parse_unit_header_32_ok() {
        let expected_rest = &[1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut expected_unit = UnitHeader {
            unit_length: 0,
            version: 4,
            debug_abbrev_offset: DebugAbbrevOffset(0x08070605),
            address_size: 4,
            format: Format::Dwarf32,
            entries_buf: EndianBuf::new(expected_rest, LittleEndian),
        };
        let section = Section::with_endian(Endian::Little)
            .unit(&mut expected_unit, &[])
            .append_bytes(expected_rest);
        let buf = section.get_contents().unwrap();
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        assert_eq!(parse_unit_header(rest), Ok(expected_unit));
        assert_eq!(*rest, EndianBuf::new(expected_rest, LittleEndian));
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_unit_header_64_ok() {
        let expected_rest = &[1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut expected_unit = UnitHeader {
            unit_length: 0,
            version: 4,
            debug_abbrev_offset: DebugAbbrevOffset(0x0102030405060708),
            address_size: 8,
            format: Format::Dwarf64,
            entries_buf: EndianBuf::new(expected_rest, LittleEndian),
        };
        let section = Section::with_endian(Endian::Little)
            .unit(&mut expected_unit, &[])
            .append_bytes(expected_rest);
        let buf = section.get_contents().unwrap();
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        assert_eq!(parse_unit_header(rest), Ok(expected_unit));
        assert_eq!(*rest, EndianBuf::new(expected_rest, LittleEndian));
    }

    #[test]
    fn test_parse_type_offset_32_ok() {
        let buf = [0x12, 0x34, 0x56, 0x78, 0x00];
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_type_offset(rest, Format::Dwarf32) {
            Ok(offset) => {
                assert_eq!(rest.len(), 1);
                assert_eq!(UnitOffset(0x78563412), offset);
            }
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        }
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_type_offset_64_ok() {
        let buf = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xff, 0x00];
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_type_offset(rest, Format::Dwarf64) {
            Ok(offset) => {
                assert_eq!(rest.len(), 1);
                assert_eq!(UnitOffset(0xffdebc9a78563412), offset);
            }
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        }
    }

    #[test]
    fn test_parse_type_offset_incomplete() {
        // Need at least 4 bytes.
        let buf = [0xff, 0xff, 0xff];
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        match parse_type_offset(rest, Format::Dwarf32) {
            Err(Error::UnexpectedEof) => assert!(true),
            otherwise => panic!("Unexpected result: {:?}", otherwise),
        };
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_type_unit_header_64_ok() {
        let expected_rest = &[1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut expected_unit = TypeUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0x08070605),
                address_size: 8,
                format: Format::Dwarf64,
                entries_buf: EndianBuf::new(expected_rest, LittleEndian),
            },
            offset: DebugTypesOffset(0),
            type_signature: DebugTypeSignature(0xdeadbeefdeadbeef),
            type_offset: UnitOffset(0x7856341278563412),
        };
        let section = Section::with_endian(Endian::Little)
            .type_unit(&mut expected_unit)
            .append_bytes(expected_rest);
        let buf = section.get_contents().unwrap();
        let rest = &mut EndianBuf::new(&buf, LittleEndian);

        assert_eq!(parse_type_unit_header(rest, DebugTypesOffset(0)),
                   Ok(expected_unit));
        assert_eq!(*rest, EndianBuf::new(expected_rest, LittleEndian));
    }

    fn section_contents<F>(f: F) -> Vec<u8>
        where F: Fn(Section) -> Section
    {
        f(Section::with_endian(Endian::Little))
            .get_contents()
            .unwrap()
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_attribute_value() {
        let mut unit = test_parse_attribute_unit_default();
        let endian = unit.entries_buf.endian();

        let block_data = &[1, 2, 3, 4];
        let buf = section_contents(|s| s.uleb(block_data.len() as u64).append_bytes(block_data));
        let block = EndianBuf::new(&buf, endian);

        let buf = section_contents(|s| s.L32(0x01020304));
        let data4 = EndianBuf::new(&buf, endian);

        let buf = section_contents(|s| s.L64(0x0102030405060708));
        let data8 = EndianBuf::new(&buf, endian);

        let tests = [(2,
                      constants::DW_AT_data_member_location,
                      constants::DW_FORM_block,
                      block,
                      AttributeValue::Block(EndianBuf::new(block_data, endian)),
                      AttributeValue::Exprloc(Expression(EndianBuf::new(block_data, endian)))),
                     (2,
                      constants::DW_AT_data_member_location,
                      constants::DW_FORM_data4,
                      data4,
                      AttributeValue::SecOffset(0x01020304),
                      AttributeValue::DebugLocRef(DebugLocOffset(0x01020304))),
                     (4,
                      constants::DW_AT_data_member_location,
                      constants::DW_FORM_data4,
                      data4,
                      AttributeValue::Data4(([4, 3, 2, 1], endian)),
                      AttributeValue::Udata(0x01020304)),
                     (2,
                      constants::DW_AT_data_member_location,
                      constants::DW_FORM_data8,
                      data8,
                      AttributeValue::SecOffset(0x0102030405060708),
                      AttributeValue::DebugLocRef(DebugLocOffset(0x0102030405060708))),
                     (4,
                      constants::DW_AT_data_member_location,
                      constants::DW_FORM_data8,
                      data8,
                      AttributeValue::Data8(([8, 7, 6, 5, 4, 3, 2, 1], endian)),
                      AttributeValue::Udata(0x0102030405060708))];

        for test in tests.iter() {
            let (version, name, form, mut input, expect_raw, expect_value) = *test;
            unit.version = version;
            let spec = AttributeSpecification::new(name, form);
            let attribute = parse_attribute(&mut input, &unit, spec)
                .expect("Should parse attribute");
            assert_eq!(attribute.raw_value(), expect_raw);
            assert_eq!(attribute.value(), expect_value);
        }
    }

    #[test]
    fn test_attribute_udata_sdata_value() {
        let endian = LittleEndian;
        let tests: &[(AttributeValue<EndianBuf<LittleEndian>>, _, _)] =
            &[(AttributeValue::Data1([1]), Some(1), Some(1)),
              (AttributeValue::Data1([255]), Some(std::u8::MAX as u64), Some(-1)),
              (AttributeValue::Data2(([1, 0], endian)), Some(1), Some(1)),
              (AttributeValue::Data2(([255; 2], endian)), Some(std::u16::MAX as u64), Some(-1)),
              (AttributeValue::Data4(([1, 0, 0, 0], endian)), Some(1), Some(1)),
              (AttributeValue::Data4(([255; 4], endian)), Some(std::u32::MAX as u64), Some(-1)),
              (AttributeValue::Data8(([1, 0, 0, 0, 0, 0, 0, 0], endian)), Some(1), Some(1)),
              (AttributeValue::Data8(([255; 8], endian)), Some(std::u64::MAX), Some(-1)),
              (AttributeValue::Sdata(1), None, Some(1)),
              (AttributeValue::Udata(1), Some(1), None)];
        for test in tests.iter() {
            let (value, expect_udata, expect_sdata) = *test;
            let attribute = Attribute {
                name: DW_AT_data_member_location,
                value: value,
            };
            assert_eq!(attribute.udata_value(), expect_udata);
            assert_eq!(attribute.sdata_value(), expect_sdata);
        }
    }

    fn test_parse_attribute_unit<Endian>(address_size: u8,
                                         format: Format,
                                         endian: Endian)
                                         -> UnitHeader<EndianBuf<'static, Endian>>
        where Endian: Endianity
    {
        UnitHeader::new(7,
                        4,
                        DebugAbbrevOffset(0x08070605),
                        address_size,
                        format,
                        EndianBuf::new(&[], endian))
    }

    fn test_parse_attribute_unit_default() -> UnitHeader<EndianBuf<'static, LittleEndian>> {
        test_parse_attribute_unit(4, Format::Dwarf32, LittleEndian)
    }

    fn test_parse_attribute<'input, Endian>(buf: &'input [u8],
                                            len: usize,
                                            unit: &UnitHeader<EndianBuf<'input, Endian>>,
                                            form: constants::DwForm,
                                            value: AttributeValue<EndianBuf<'input, Endian>>)
        where Endian: Endianity
    {
        let spec = AttributeSpecification::new(constants::DW_AT_low_pc, form);

        let expect = Attribute {
            name: constants::DW_AT_low_pc,
            value: value,
        };

        let rest = &mut EndianBuf::new(buf, Endian::default());
        match parse_attribute(rest, unit, spec) {
            Ok(attr) => {
                assert_eq!(attr, expect);
                assert_eq!(*rest, EndianBuf::new(&buf[len..], Endian::default()));
            }
            otherwise => {
                println!("Unexpected parse result = {:#?}", otherwise);
                assert!(false);
            }
        };
    }

    #[test]
    fn test_parse_attribute_addr() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let unit = test_parse_attribute_unit(4, Format::Dwarf32, LittleEndian);
        let form = constants::DW_FORM_addr;
        let value = AttributeValue::Addr(0x04030201);
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_addr8() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let unit = test_parse_attribute_unit(8, Format::Dwarf32, LittleEndian);
        let form = constants::DW_FORM_addr;
        let value = AttributeValue::Addr(0x0807060504030201);
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_block1() {
        // Length of data (3), three bytes of data, two bytes of left over input.
        let buf = [0x03, 0x09, 0x09, 0x09, 0x00, 0x00];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_block1;
        let value = AttributeValue::Block(EndianBuf::new(&buf[1..4], LittleEndian));
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_block2() {
        // Two byte length of data (2), two bytes of data, two bytes of left over input.
        let buf = [0x02, 0x00, 0x09, 0x09, 0x00, 0x00];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_block2;
        let value = AttributeValue::Block(EndianBuf::new(&buf[2..4], LittleEndian));
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_block4() {
        // Four byte length of data (2), two bytes of data, no left over input.
        let buf = [0x02, 0x00, 0x00, 0x00, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_block4;
        let value = AttributeValue::Block(EndianBuf::new(&buf[4..], LittleEndian));
        test_parse_attribute(&buf, 6, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_block() {
        // LEB length of data (2, one byte), two bytes of data, no left over input.
        let buf = [0x02, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_block;
        let value = AttributeValue::Block(EndianBuf::new(&buf[1..], LittleEndian));
        test_parse_attribute(&buf, 3, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_data1() {
        let buf = [0x03];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_data1;
        let value = AttributeValue::Data1([0x03]);
        test_parse_attribute(&buf, 1, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_data2() {
        let buf = [0x02, 0x01, 0x0];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_data2;
        let value = AttributeValue::Data2(([0x02, 0x01], LittleEndian));
        test_parse_attribute(&buf, 2, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_data4() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_data4;
        let value = AttributeValue::Data4(([0x01, 0x02, 0x03, 0x04], LittleEndian));
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_data8() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_data8;
        let value = AttributeValue::Data8(([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
                                           LittleEndian));
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_udata() {
        let mut buf = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        let bytes_written = {
            let mut writable = &mut buf[..];
            leb128::write::unsigned(&mut writable, 4097).expect("should write ok")
        };

        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_udata;
        let value = AttributeValue::Udata(4097);
        test_parse_attribute(&buf, bytes_written, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_sdata() {
        let mut buf = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        let bytes_written = {
            let mut writable = &mut buf[..];
            leb128::write::signed(&mut writable, -4097).expect("should write ok")
        };

        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_sdata;
        let value = AttributeValue::Sdata(-4097);
        test_parse_attribute(&buf, bytes_written, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_exprloc() {
        // LEB length of data (2, one byte), two bytes of data, one byte left over input.
        let buf = [0x02, 0x99, 0x99, 0x11];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_exprloc;
        let value = AttributeValue::Exprloc(Expression(EndianBuf::new(&buf[1..3], LittleEndian)));
        test_parse_attribute(&buf, 3, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_flag_true() {
        let buf = [0x42];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_flag;
        let value = AttributeValue::Flag(true);
        test_parse_attribute(&buf, 1, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_flag_false() {
        let buf = [0x00];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_flag;
        let value = AttributeValue::Flag(false);
        test_parse_attribute(&buf, 1, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_flag_present() {
        let buf = [0x01, 0x02, 0x03, 0x04];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_flag_present;
        let value = AttributeValue::Flag(true);
        // DW_FORM_flag_present does not consume any bytes of the input stream.
        test_parse_attribute(&buf, 0, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_sec_offset_32() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x10];
        let unit = test_parse_attribute_unit(4, Format::Dwarf32, LittleEndian);
        let form = constants::DW_FORM_sec_offset;
        let value = AttributeValue::SecOffset(0x04030201);
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_attribute_sec_offset_64() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x10];
        let unit = test_parse_attribute_unit(4, Format::Dwarf64, LittleEndian);
        let form = constants::DW_FORM_sec_offset;
        let value = AttributeValue::SecOffset(0x0807060504030201);
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_ref1() {
        let buf = [0x03];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_ref1;
        let value = AttributeValue::UnitRef(UnitOffset(3));
        test_parse_attribute(&buf, 1, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_ref2() {
        let buf = [0x02, 0x01, 0x0];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_ref2;
        let value = AttributeValue::UnitRef(UnitOffset(258));
        test_parse_attribute(&buf, 2, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_ref4() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_ref4;
        let value = AttributeValue::UnitRef(UnitOffset(67305985));
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_attribute_ref8() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_ref8;
        let value = AttributeValue::UnitRef(UnitOffset(578437695752307201));
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_refudata() {
        let mut buf = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        let bytes_written = {
            let mut writable = &mut buf[..];
            leb128::write::unsigned(&mut writable, 4097).expect("should write ok")
        };

        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_ref_udata;
        let value = AttributeValue::UnitRef(UnitOffset(4097));
        test_parse_attribute(&buf, bytes_written, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_refaddr_32() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let unit = test_parse_attribute_unit(4, Format::Dwarf32, LittleEndian);
        let form = constants::DW_FORM_ref_addr;
        let value = AttributeValue::DebugInfoRef(DebugInfoOffset(67305985));
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_attribute_refaddr_64() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let unit = test_parse_attribute_unit(4, Format::Dwarf64, LittleEndian);
        let form = constants::DW_FORM_ref_addr;
        let value = AttributeValue::DebugInfoRef(DebugInfoOffset(578437695752307201));
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_refaddr_version2() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let mut unit = test_parse_attribute_unit(4, Format::Dwarf32, LittleEndian);
        unit.version = 2;
        let form = constants::DW_FORM_ref_addr;
        let value = AttributeValue::DebugInfoRef(DebugInfoOffset(0x04030201));
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_attribute_refaddr8_version2() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let mut unit = test_parse_attribute_unit(8, Format::Dwarf32, LittleEndian);
        unit.version = 2;
        let form = constants::DW_FORM_ref_addr;
        let value = AttributeValue::DebugInfoRef(DebugInfoOffset(0x0807060504030201));
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_refsig8() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_ref_sig8;
        let value = AttributeValue::DebugTypesRef(DebugTypeSignature(578437695752307201));
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_string() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x0, 0x99, 0x99];
        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_string;
        let value = AttributeValue::String(EndianBuf::new(&buf[..5], LittleEndian));
        test_parse_attribute(&buf, 6, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_strp_32() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let unit = test_parse_attribute_unit(4, Format::Dwarf32, LittleEndian);
        let form = constants::DW_FORM_strp;
        let value = AttributeValue::DebugStrRef(DebugStrOffset(67305985));
        test_parse_attribute(&buf, 4, &unit, form, value);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_parse_attribute_strp_64() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x99, 0x99];
        let unit = test_parse_attribute_unit(4, Format::Dwarf64, LittleEndian);
        let form = constants::DW_FORM_strp;
        let value = AttributeValue::DebugStrRef(DebugStrOffset(578437695752307201));
        test_parse_attribute(&buf, 8, &unit, form, value);
    }

    #[test]
    fn test_parse_attribute_indirect() {
        let mut buf = [0; 100];

        let bytes_written = {
            let mut writable = &mut buf[..];
            leb128::write::unsigned(&mut writable, constants::DW_FORM_udata.0)
                .expect("should write udata") +
            leb128::write::unsigned(&mut writable, 9999999).expect("should write value")
        };

        let unit = test_parse_attribute_unit_default();
        let form = constants::DW_FORM_indirect;
        let value = AttributeValue::Udata(9999999);
        test_parse_attribute(&buf, bytes_written, &unit, form, value);
    }

    #[test]
    fn test_attrs_iter() {
        let unit = UnitHeader::new(7,
                                   4,
                                   DebugAbbrevOffset(0x08070605),
                                   4,
                                   Format::Dwarf32,
                                   EndianBuf::new(&[], LittleEndian));

        let abbrev =
            Abbreviation::new(42,
                              constants::DW_TAG_subprogram,
                              constants::DW_CHILDREN_yes,
                              vec![AttributeSpecification::new(constants::DW_AT_name,
                                                               constants::DW_FORM_string),
                                   AttributeSpecification::new(constants::DW_AT_low_pc,
                                                               constants::DW_FORM_addr),
                                   AttributeSpecification::new(constants::DW_AT_high_pc,
                                                               constants::DW_FORM_addr)]);

        // "foo", 42, 1337, 4 dangling bytes of 0xaa where children would be
        let buf = [0x66, 0x6f, 0x6f, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x39, 0x05, 0x00, 0x00, 0xaa,
                   0xaa, 0xaa, 0xaa];

        let entry = DebuggingInformationEntry {
            offset: UnitOffset(0),
            attrs_slice: EndianBuf::new(&buf, LittleEndian),
            attrs_len: Cell::new(None),
            abbrev: &abbrev,
            unit: &unit,
        };

        let mut attrs = AttrsIter {
            input: EndianBuf::new(&buf, LittleEndian),
            attributes: abbrev.attributes(),
            entry: &entry,
        };

        match attrs.next() {
            Ok(Some(attr)) => {
                assert_eq!(attr,
                           Attribute {
                               name: constants::DW_AT_name,
                               value: AttributeValue::String(EndianBuf::new(b"foo", LittleEndian)),
                           });
            }
            otherwise => {
                println!("Unexpected parse result = {:#?}", otherwise);
                assert!(false);
            }
        }

        assert!(entry.attrs_len.get().is_none());

        match attrs.next() {
            Ok(Some(attr)) => {
                assert_eq!(attr,
                           Attribute {
                               name: constants::DW_AT_low_pc,
                               value: AttributeValue::Addr(0x2a),
                           });
            }
            otherwise => {
                println!("Unexpected parse result = {:#?}", otherwise);
                assert!(false);
            }
        }

        assert!(entry.attrs_len.get().is_none());

        match attrs.next() {
            Ok(Some(attr)) => {
                assert_eq!(attr,
                           Attribute {
                               name: constants::DW_AT_high_pc,
                               value: AttributeValue::Addr(0x539),
                           });
            }
            otherwise => {
                println!("Unexpected parse result = {:#?}", otherwise);
                assert!(false);
            }
        }

        assert!(entry.attrs_len.get().is_none());

        assert!(attrs.next().expect("should parse next").is_none());
        assert!(entry.attrs_len.get().is_some());
        assert_eq!(entry.attrs_len.get().expect("should have entry.attrs_len"),
                   buf.len() - 4)
    }

    #[test]
    fn test_attrs_iter_incomplete() {
        let unit = UnitHeader::new(7,
                                   4,
                                   DebugAbbrevOffset(0x08070605),
                                   4,
                                   Format::Dwarf32,
                                   EndianBuf::new(&[], LittleEndian));

        let abbrev =
            Abbreviation::new(42,
                              constants::DW_TAG_subprogram,
                              constants::DW_CHILDREN_yes,
                              vec![AttributeSpecification::new(constants::DW_AT_name,
                                                               constants::DW_FORM_string),
                                   AttributeSpecification::new(constants::DW_AT_low_pc,
                                                               constants::DW_FORM_addr),
                                   AttributeSpecification::new(constants::DW_AT_high_pc,
                                                               constants::DW_FORM_addr)]);

        // "foo"
        let buf = [0x66, 0x6f, 0x6f, 0x00];

        let entry = DebuggingInformationEntry {
            offset: UnitOffset(0),
            attrs_slice: EndianBuf::new(&buf, LittleEndian),
            attrs_len: Cell::new(None),
            abbrev: &abbrev,
            unit: &unit,
        };

        let mut attrs = AttrsIter {
            input: EndianBuf::new(&buf, LittleEndian),
            attributes: abbrev.attributes(),
            entry: &entry,
        };

        match attrs.next() {
            Ok(Some(attr)) => {
                assert_eq!(attr,
                           Attribute {
                               name: constants::DW_AT_name,
                               value: AttributeValue::String(EndianBuf::new(b"foo", LittleEndian)),
                           });
            }
            otherwise => {
                println!("Unexpected parse result = {:#?}", otherwise);
                assert!(false);
            }
        }

        assert!(entry.attrs_len.get().is_none());

        // Return error for incomplete attribute.
        assert!(attrs.next().is_err());
        assert!(entry.attrs_len.get().is_none());

        // Return error for all subsequent calls.
        assert!(attrs.next().is_err());
        assert!(attrs.next().is_err());
        assert!(attrs.next().is_err());
        assert!(attrs.next().is_err());
        assert!(entry.attrs_len.get().is_none());
    }

    fn assert_entry_name<Endian>(entry: &DebuggingInformationEntry<EndianBuf<Endian>>, name: &str)
        where Endian: Endianity
    {
        let value = entry
            .attr_value(constants::DW_AT_name)
            .expect("Should have parsed the name attribute")
            .expect("Should have found the name attribute");

        assert_eq!(value,
                   AttributeValue::String(EndianBuf::new(name.as_bytes(), Endian::default())));
    }

    fn assert_current_name<Endian>(cursor: &EntriesCursor<EndianBuf<Endian>>, name: &str)
        where Endian: Endianity
    {
        let entry = cursor.current().expect("Should have an entry result");
        assert_entry_name(entry, name);
    }

    fn assert_next_entry<Endian>(cursor: &mut EntriesCursor<EndianBuf<Endian>>, name: &str)
        where Endian: Endianity
    {
        cursor
            .next_entry()
            .expect("Should parse next entry")
            .expect("Should have an entry");
        assert_current_name(cursor, name);
    }

    fn assert_next_entry_null<Endian>(cursor: &mut EntriesCursor<EndianBuf<Endian>>)
        where Endian: Endianity
    {
        cursor
            .next_entry()
            .expect("Should parse next entry")
            .expect("Should have an entry");
        assert!(cursor.current().is_none());
    }

    fn assert_next_dfs<Endian>(cursor: &mut EntriesCursor<EndianBuf<Endian>>,
                               name: &str,
                               depth: isize)
        where Endian: Endianity
    {
        {
            let (val, entry) = cursor
                .next_dfs()
                .expect("Should parse next dfs")
                .expect("Should not be done with traversal");
            assert_eq!(val, depth);
            assert_entry_name(entry, name);
        }
        assert_current_name(cursor, name);
    }

    fn assert_next_sibling<Endian>(cursor: &mut EntriesCursor<EndianBuf<Endian>>, name: &str)
        where Endian: Endianity
    {
        {
            let entry = cursor
                .next_sibling()
                .expect("Should parse next sibling")
                .expect("Should not be done with traversal");
            assert_entry_name(entry, name);
        }
        assert_current_name(cursor, name);
    }

    fn assert_valid_sibling_ptr<Endian>(cursor: &EntriesCursor<EndianBuf<Endian>>)
        where Endian: Endianity
    {
        let sibling_ptr = cursor
            .current()
            .expect("Should have current entry")
            .attr_value(constants::DW_AT_sibling);
        match sibling_ptr {
            Ok(Some(AttributeValue::UnitRef(offset))) => {
                cursor
                    .unit
                    .range_from(offset..)
                    .expect("Sibling offset should be valid");
            }
            _ => panic!("Invalid sibling pointer {:?}", sibling_ptr),
        }
    }

    fn entries_cursor_tests_abbrev_buf() -> Vec<u8> {
        let section = Section::with_endian(Endian::Little)
            .abbrev(1, DW_TAG_subprogram, DW_CHILDREN_yes)
            .abbrev_attr(DW_AT_name, DW_FORM_string)
            .abbrev_attr_null()
            .abbrev_null();
        section.get_contents().unwrap()
    }

    #[cfg_attr(rustfmt, rustfmt_skip)]
    fn entries_cursor_tests_debug_info_buf() -> Vec<u8> {
        let section = Section::with_endian(Endian::Little)
            .die(1, |s| s.attr_string("001"))
                .die(1, |s| s.attr_string("002"))
                    .die(1, |s| s.attr_string("003"))
                        .die_null()
                    .die_null()
                .die(1, |s| s.attr_string("004"))
                    .die(1, |s| s.attr_string("005"))
                        .die_null()
                    .die(1, |s| s.attr_string("006"))
                        .die_null()
                    .die_null()
                .die(1, |s| s.attr_string("007"))
                    .die(1, |s| s.attr_string("008"))
                        .die(1, |s| s.attr_string("009"))
                            .die_null()
                        .die_null()
                    .die_null()
                .die(1, |s| s.attr_string("010"))
                    .die_null()
                .die_null();
        let entries_buf = section.get_contents().unwrap();

        let mut unit = CompilationUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0),
                address_size: 4,
                format: Format::Dwarf32,
                entries_buf: EndianBuf::new(&entries_buf, LittleEndian),
            },
            offset: DebugInfoOffset(0),
        };
        let section = Section::with_endian(Endian::Little).comp_unit(&mut unit);
        section.get_contents().unwrap()
    }

    #[test]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    fn test_cursor_next_entry_incomplete() {
        let section = Section::with_endian(Endian::Little)
            .die(1, |s| s.attr_string("001"))
                .die(1, |s| s.attr_string("002"))
                    .die(1, |s| s);
        let entries_buf = section.get_contents().unwrap();

        let mut unit = CompilationUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0),
                address_size: 4,
                format: Format::Dwarf32,
                entries_buf: EndianBuf::new(&entries_buf, LittleEndian),
            },
            offset: DebugInfoOffset(0),
        };
        let section = Section::with_endian(Endian::Little).comp_unit(&mut unit);
        let info_buf = &section.get_contents().unwrap();
        let debug_info = DebugInfo::new(info_buf, LittleEndian);

        let unit = debug_info.units().next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrevs_buf = &entries_cursor_tests_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(abbrevs_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries(&abbrevs);

        assert_next_entry(&mut cursor, "001");
        assert_next_entry(&mut cursor, "002");

        {
            // Entry code is present, but none of the attributes.
            cursor.next_entry()
                .expect("Should parse next entry")
                .expect("Should have an entry");
            let entry = cursor.current().expect("Should have an entry result");
            assert!(entry.attrs().next().is_err());
        }

        assert!(cursor.next_entry().is_err());
        assert!(cursor.next_entry().is_err());
    }

    #[test]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    fn test_cursor_next_entry() {
        let info_buf = &entries_cursor_tests_debug_info_buf();
        let debug_info = DebugInfo::new(info_buf, LittleEndian);

        let unit = debug_info.units().next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrevs_buf = &entries_cursor_tests_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(abbrevs_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries(&abbrevs);

        assert_next_entry(&mut cursor, "001");
        assert_next_entry(&mut cursor, "002");
        assert_next_entry(&mut cursor, "003");
        assert_next_entry_null(&mut cursor);
        assert_next_entry_null(&mut cursor);
        assert_next_entry(&mut cursor, "004");
        assert_next_entry(&mut cursor, "005");
        assert_next_entry_null(&mut cursor);
        assert_next_entry(&mut cursor, "006");
        assert_next_entry_null(&mut cursor);
        assert_next_entry_null(&mut cursor);
        assert_next_entry(&mut cursor, "007");
        assert_next_entry(&mut cursor, "008");
        assert_next_entry(&mut cursor, "009");
        assert_next_entry_null(&mut cursor);
        assert_next_entry_null(&mut cursor);
        assert_next_entry_null(&mut cursor);
        assert_next_entry(&mut cursor, "010");
        assert_next_entry_null(&mut cursor);
        assert_next_entry_null(&mut cursor);

        assert!(cursor.next_entry().expect("Should parse next entry").is_none());
        assert!(cursor.current().is_none());
    }

    #[test]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    fn test_cursor_next_dfs() {
        let info_buf = &entries_cursor_tests_debug_info_buf();
        let debug_info = DebugInfo::new(info_buf, LittleEndian);

        let unit = debug_info.units().next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrevs_buf = &entries_cursor_tests_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(abbrevs_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries(&abbrevs);

        assert_next_dfs(&mut cursor, "001", 0);
        assert_next_dfs(&mut cursor, "002", 1);
        assert_next_dfs(&mut cursor, "003", 1);
        assert_next_dfs(&mut cursor, "004", -1);
        assert_next_dfs(&mut cursor, "005", 1);
        assert_next_dfs(&mut cursor, "006", 0);
        assert_next_dfs(&mut cursor, "007", -1);
        assert_next_dfs(&mut cursor, "008", 1);
        assert_next_dfs(&mut cursor, "009", 1);
        assert_next_dfs(&mut cursor, "010", -2);

        assert!(cursor.next_dfs().expect("Should parse next dfs").is_none());
        assert!(cursor.current().is_none());
    }

    #[test]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    fn test_cursor_next_sibling_no_sibling_ptr() {
        let info_buf = &entries_cursor_tests_debug_info_buf();
        let debug_info = DebugInfo::new(info_buf, LittleEndian);

        let unit = debug_info.units().next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrevs_buf = &entries_cursor_tests_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(abbrevs_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries(&abbrevs);

        assert_next_dfs(&mut cursor, "001", 0);

        // Down to the first child of the root entry.

        assert_next_dfs(&mut cursor, "002", 1);

        // Now iterate all children of the root via `next_sibling`.

        assert_next_sibling(&mut cursor, "004");
        assert_next_sibling(&mut cursor, "007");
        assert_next_sibling(&mut cursor, "010");

        // There should be no more siblings.

        assert!(cursor.next_sibling().expect("Should parse next sibling").is_none());
        assert!(cursor.current().is_none());
    }

    #[test]
    fn test_cursor_next_sibling_continuation() {
        let info_buf = &entries_cursor_tests_debug_info_buf();
        let debug_info = DebugInfo::new(info_buf, LittleEndian);

        let unit = debug_info
            .units()
            .next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrevs_buf = &entries_cursor_tests_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(abbrevs_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries(&abbrevs);

        assert_next_dfs(&mut cursor, "001", 0);

        // Down to the first child of the root entry.

        assert_next_dfs(&mut cursor, "002", 1);

        // Get the next sibling, then iterate its children

        assert_next_sibling(&mut cursor, "004");
        assert_next_dfs(&mut cursor, "005", 1);
        assert_next_sibling(&mut cursor, "006");
        assert!(cursor
                    .next_sibling()
                    .expect("Should parse next sibling")
                    .is_none());
        assert!(cursor
                    .next_sibling()
                    .expect("Should parse next sibling")
                    .is_none());
        assert!(cursor
                    .next_sibling()
                    .expect("Should parse next sibling")
                    .is_none());
        assert!(cursor
                    .next_sibling()
                    .expect("Should parse next sibling")
                    .is_none());

        // And we should be able to continue with the children of the root entry.

        assert_next_dfs(&mut cursor, "007", -1);
        assert_next_sibling(&mut cursor, "010");

        // There should be no more siblings.

        assert!(cursor
                    .next_sibling()
                    .expect("Should parse next sibling")
                    .is_none());
        assert!(cursor.current().is_none());
    }

    fn entries_cursor_sibling_abbrev_buf() -> Vec<u8> {
        let section = Section::with_endian(Endian::Little)
            .abbrev(1, DW_TAG_subprogram, DW_CHILDREN_yes)
            .abbrev_attr(DW_AT_name, DW_FORM_string)
            .abbrev_attr(DW_AT_sibling, DW_FORM_ref1)
            .abbrev_attr_null()
            .abbrev(2, DW_TAG_subprogram, DW_CHILDREN_yes)
            .abbrev_attr(DW_AT_name, DW_FORM_string)
            .abbrev_attr_null()
            .abbrev_null();
        section.get_contents().unwrap()
    }

    fn entries_cursor_sibling_entries_buf(header_size: usize) -> Vec<u8> {
        let start = Label::new();
        let sibling004_ref = Label::new();
        let sibling004 = Label::new();
        let sibling009_ref = Label::new();
        let sibling009 = Label::new();

        let section = Section::with_endian(Endian::Little)
            .mark(&start)
            .die(2, |s| s.attr_string("001"))
                // Valid sibling attribute.
                .die(1, |s| s.attr_string("002").D8(&sibling004_ref))
                    // Invalid code to ensure the sibling attribute was used.
                    .die(10, |s| s.attr_string("003"))
                        .die_null()
                    .die_null()
                .mark(&sibling004)
                // Invalid sibling attribute.
                .die(1, |s| s.attr_string("004").attr_ref1(255))
                    .die(2, |s| s.attr_string("005"))
                        .die_null()
                    .die_null()
                // Sibling attribute in child only.
                .die(2, |s| s.attr_string("006"))
                    // Valid sibling attribute.
                    .die(1, |s| s.attr_string("007").D8(&sibling009_ref))
                        // Invalid code to ensure the sibling attribute was used.
                        .die(10, |s| s.attr_string("008"))
                            .die_null()
                        .die_null()
                    .mark(&sibling009)
                    .die(2, |s| s.attr_string("009"))
                        .die_null()
                    .die_null()
                // No sibling attribute.
                .die(2, |s| s.attr_string("010"))
                    .die(2, |s| s.attr_string("011"))
                        .die_null()
                    .die_null()
                .die_null();

        let offset = header_size as u64 + (&sibling004 - &start) as u64;
        sibling004_ref.set_const(offset);

        let offset = header_size as u64 + (&sibling009 - &start) as u64;
        sibling009_ref.set_const(offset);

        section.get_contents().unwrap()
    }

    fn test_cursor_next_sibling_with_ptr(cursor: &mut EntriesCursor<EndianBuf<LittleEndian>>) {
        assert_next_dfs(cursor, "001", 0);

        // Down to the first child of the root.

        assert_next_dfs(cursor, "002", 1);

        // Now iterate all children of the root via `next_sibling`.

        assert_valid_sibling_ptr(&cursor);
        assert_next_sibling(cursor, "004");
        assert_next_sibling(cursor, "006");
        assert_next_sibling(cursor, "010");

        // There should be no more siblings.

        assert!(cursor
                    .next_sibling()
                    .expect("Should parse next sibling")
                    .is_none());
        assert!(cursor.current().is_none());
    }

    #[test]
    fn test_debug_info_next_sibling_with_ptr() {
        let format = Format::Dwarf32;
        let header_size =
            CompilationUnitHeader::<EndianBuf<LittleEndian>, _>::size_of_header(format);
        let entries_buf = entries_cursor_sibling_entries_buf(header_size);

        let mut unit = CompilationUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0),
                address_size: 4,
                format: format,
                entries_buf: EndianBuf::new(&entries_buf, LittleEndian),
            },
            offset: DebugInfoOffset(0),
        };
        let section = Section::with_endian(Endian::Little).comp_unit(&mut unit);
        let info_buf = section.get_contents().unwrap();
        let debug_info = DebugInfo::new(&info_buf, LittleEndian);

        let unit = debug_info
            .units()
            .next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrev_buf = entries_cursor_sibling_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(&abbrev_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries(&abbrevs);
        test_cursor_next_sibling_with_ptr(&mut cursor);
    }

    #[test]
    fn test_debug_types_next_sibling_with_ptr() {
        let format = Format::Dwarf32;
        let header_size = TypeUnitHeader::<EndianBuf<LittleEndian>, _>::size_of_header(format);
        let entries_buf = entries_cursor_sibling_entries_buf(header_size);

        let mut unit = TypeUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0),
                address_size: 4,
                format: format,
                entries_buf: EndianBuf::new(&entries_buf, LittleEndian),
            },
            type_signature: DebugTypeSignature(0),
            type_offset: UnitOffset(0),
            offset: DebugTypesOffset(0),
        };
        let section = Section::with_endian(Endian::Little).type_unit(&mut unit);
        let info_buf = section.get_contents().unwrap();
        let debug_types = DebugTypes::new(&info_buf, LittleEndian);

        let unit = debug_types
            .units()
            .next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrev_buf = entries_cursor_sibling_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(&abbrev_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries(&abbrevs);
        test_cursor_next_sibling_with_ptr(&mut cursor);
    }

    #[test]
    fn test_entries_at_offset() {
        let info_buf = &entries_cursor_tests_debug_info_buf();
        let debug_info = DebugInfo::new(info_buf, LittleEndian);

        let unit = debug_info
            .units()
            .next()
            .expect("should have a unit result")
            .expect("and it should be ok");

        let abbrevs_buf = &entries_cursor_tests_abbrev_buf();
        let debug_abbrev = DebugAbbrev::new(abbrevs_buf, LittleEndian);

        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");

        let mut cursor = unit.entries_at_offset(&abbrevs, UnitOffset(unit.header_size()))
            .unwrap();
        assert_next_entry(&mut cursor, "001");

        let cursor = unit.entries_at_offset(&abbrevs, UnitOffset(0));
        match cursor {
            Err(Error::OffsetOutOfBounds) => {}
            otherwise => {
                println!("Unexpected result = {:#?}", otherwise);
                assert!(false);
            }
        }
    }

    #[cfg_attr(rustfmt, rustfmt_skip)]
    fn entries_tree_tests_debug_abbrevs_buf() -> Vec<u8> {
        Section::with_endian(Endian::Little)
            .abbrev(1, DW_TAG_subprogram, DW_CHILDREN_yes)
                .abbrev_attr(DW_AT_name, DW_FORM_string)
                .abbrev_attr_null()
            .abbrev(2, DW_TAG_subprogram, DW_CHILDREN_no)
                .abbrev_attr(DW_AT_name, DW_FORM_string)
                .abbrev_attr_null()
            .abbrev_null()
            .get_contents()
            .unwrap()
    }

    #[cfg_attr(rustfmt, rustfmt_skip)]
    fn entries_tree_tests_debug_info_buf(header_size: usize) -> (Vec<u8>, UnitOffset) {
        let start = Label::new();
        let entry2 = Label::new();
        let section = Section::with_endian(Endian::Little)
            .mark(&start)
            .die(1, |s| s.attr_string("root"))
                .die(1, |s| s.attr_string("1"))
                    .die(1, |s| s.attr_string("1a"))
                        .die_null()
                    .die(2, |s| s.attr_string("1b"))
                    .die_null()
                .mark(&entry2)
                .die(1, |s| s.attr_string("2"))
                    .die(1, |s| s.attr_string("2a"))
                        .die(1, |s| s.attr_string("2a1"))
                            .die_null()
                        .die_null()
                    .die(1, |s| s.attr_string("2b"))
                        .die(2, |s| s.attr_string("2b1"))
                        .die_null()
                    .die_null()
                .die(1, |s| s.attr_string("3"))
                    .die(1, |s| s.attr_string("3a"))
                        .die(2, |s| s.attr_string("3a1"))
                        .die(2, |s| s.attr_string("3a2"))
                        .die_null()
                    .die(2, |s| s.attr_string("3b"))
                    .die_null()
                .die(2, |s| s.attr_string("final"))
                .die_null()
            .get_contents()
            .unwrap();
        let entry2 = UnitOffset(header_size + (&entry2 - &start) as usize);
        (section, entry2)
    }

    #[test]
    fn test_entries_tree() {
        fn assert_entry<'input, 'abbrev, 'unit, 'tree, Endian>
            (node: Result<Option<EntriesTreeNode<'abbrev,
                                                 'unit,
                                                 'tree,
                                                 EndianBuf<'input, Endian>>>>,
             name: &str)
             -> EntriesTreeIter<'abbrev, 'unit, 'tree, EndianBuf<'input, Endian>>
            where Endian: Endianity
        {
            let node = node.expect("Should parse entry")
                .expect("Should have entry");
            assert_entry_name(node.entry(), name);
            node.children()
        }

        fn assert_null<E: Endianity>(node: Result<Option<EntriesTreeNode<EndianBuf<E>>>>) {
            match node {
                Ok(None) => {}
                otherwise => {
                    println!("Unexpected parse result = {:#?}", otherwise);
                    assert!(false);
                }
            }
        }

        let abbrevs_buf = entries_tree_tests_debug_abbrevs_buf();
        let debug_abbrev = DebugAbbrev::new(&abbrevs_buf, LittleEndian);

        let format = Format::Dwarf32;
        let header_size =
            CompilationUnitHeader::<EndianBuf<LittleEndian>, _>::size_of_header(format);
        let (entries_buf, entry2) = entries_tree_tests_debug_info_buf(header_size);
        let mut unit = CompilationUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0),
                address_size: 4,
                format: format,
                entries_buf: EndianBuf::new(&entries_buf, LittleEndian),
            },
            offset: DebugInfoOffset(0),
        };
        let info_buf = Section::with_endian(Endian::Little)
            .comp_unit(&mut unit)
            .get_contents()
            .unwrap();
        let debug_info = DebugInfo::new(&info_buf, LittleEndian);

        let unit = debug_info
            .units()
            .next()
            .expect("Should parse unit")
            .expect("and it should be some");
        let abbrevs = unit.abbreviations(&debug_abbrev)
            .expect("Should parse abbreviations");
        let mut tree = unit.entries_tree(&abbrevs, None)
            .expect("Should have entries tree");

        // Test we can restart iteration of the tree.
        {
            let mut iter = assert_entry(tree.root().map(Some), "root");
            assert_entry(iter.next(), "1");
        }
        {
            let mut iter = assert_entry(tree.root().map(Some), "root");
            assert_entry(iter.next(), "1");
        }

        let mut iter = assert_entry(tree.root().map(Some), "root");
        {
            // Test iteration with children.
            let mut iter = assert_entry(iter.next(), "1");
            {
                // Test iteration with children flag, but no children.
                let mut iter = assert_entry(iter.next(), "1a");
                assert_null(iter.next());
                assert_null(iter.next());
            }
            {
                // Test iteration without children flag.
                let mut iter = assert_entry(iter.next(), "1b");
                assert_null(iter.next());
                assert_null(iter.next());
            }
            assert_null(iter.next());
            assert_null(iter.next());
        }
        {
            // Test skipping over children.
            let mut iter = assert_entry(iter.next(), "2");
            assert_entry(iter.next(), "2a");
            assert_entry(iter.next(), "2b");
            assert_null(iter.next());
        }
        {
            // Test skipping after partial iteration.
            let mut iter = assert_entry(iter.next(), "3");
            {
                let mut iter = assert_entry(iter.next(), "3a");
                assert_entry(iter.next(), "3a1");
                // Parent iter should be able to skip over "3a2".
            }
            assert_entry(iter.next(), "3b");
            assert_null(iter.next());
        }
        assert_entry(iter.next(), "final");
        assert_null(iter.next());

        // Test starting at an offset.
        let mut tree = unit.entries_tree(&abbrevs, Some(entry2))
            .expect("Should have entries tree");
        let mut iter = assert_entry(tree.root().map(Some), "2");
        assert_entry(iter.next(), "2a");
        assert_entry(iter.next(), "2b");
        assert_null(iter.next());
    }

    #[test]
    fn test_debug_info_offset() {
        let padding = &[0; 10];
        let entries = &[0; 20];
        let mut unit = CompilationUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0),
                address_size: 4,
                format: Format::Dwarf32,
                entries_buf: EndianBuf::new(entries, LittleEndian),
            },
            offset: DebugInfoOffset(0),
        };
        Section::with_endian(Endian::Little)
            .append_bytes(padding)
            .comp_unit(&mut unit);
        let offset = padding.len();
        let header_length =
            CompilationUnitHeader::<EndianBuf<LittleEndian>, _>::size_of_header(unit.format());
        let length = unit.length_including_self();
        assert_eq!(DebugInfoOffset(0).to_unit_offset(&unit), None);
        assert_eq!(DebugInfoOffset(offset - 1).to_unit_offset(&unit), None);
        assert_eq!(DebugInfoOffset(offset).to_unit_offset(&unit), None);
        assert_eq!(DebugInfoOffset(offset + header_length - 1).to_unit_offset(&unit),
                   None);
        assert_eq!(DebugInfoOffset(offset + header_length).to_unit_offset(&unit),
                   Some(UnitOffset(header_length)));
        assert_eq!(DebugInfoOffset(offset + length - 1).to_unit_offset(&unit),
                   Some(UnitOffset(length - 1)));
        assert_eq!(DebugInfoOffset(offset + length).to_unit_offset(&unit), None);
        assert_eq!(UnitOffset(header_length).to_debug_info_offset(&unit),
                   DebugInfoOffset(offset + header_length));
        assert_eq!(UnitOffset(length - 1).to_debug_info_offset(&unit),
                   DebugInfoOffset(offset + length - 1));
    }

    #[test]
    fn test_debug_types_offset() {
        let padding = &[0; 10];
        let entries = &[0; 20];
        let mut unit = TypeUnitHeader {
            header: UnitHeader {
                unit_length: 0,
                version: 4,
                debug_abbrev_offset: DebugAbbrevOffset(0),
                address_size: 4,
                format: Format::Dwarf32,
                entries_buf: EndianBuf::new(entries, LittleEndian),
            },
            type_signature: DebugTypeSignature(0),
            type_offset: UnitOffset(0),
            offset: DebugTypesOffset(0),
        };
        Section::with_endian(Endian::Little)
            .append_bytes(padding)
            .type_unit(&mut unit);
        let offset = padding.len();
        let header_length =
            TypeUnitHeader::<EndianBuf<LittleEndian>, _>::size_of_header(unit.format());
        let length = unit.length_including_self();
        assert_eq!(DebugTypesOffset(0).to_unit_offset(&unit), None);
        assert_eq!(DebugTypesOffset(offset - 1).to_unit_offset(&unit), None);
        assert_eq!(DebugTypesOffset(offset).to_unit_offset(&unit), None);
        assert_eq!(DebugTypesOffset(offset + header_length - 1).to_unit_offset(&unit),
                   None);
        assert_eq!(DebugTypesOffset(offset + header_length).to_unit_offset(&unit),
                   Some(UnitOffset(header_length)));
        assert_eq!(DebugTypesOffset(offset + length - 1).to_unit_offset(&unit),
                   Some(UnitOffset(length - 1)));
        assert_eq!(DebugTypesOffset(offset + length).to_unit_offset(&unit),
                   None);
        assert_eq!(UnitOffset(header_length).to_debug_types_offset(&unit),
                   DebugTypesOffset(offset + header_length));
        assert_eq!(UnitOffset(length - 1).to_debug_types_offset(&unit),
                   DebugTypesOffset(offset + length - 1));
    }
}
