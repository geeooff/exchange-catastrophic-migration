//! A property context read from a node's bytes ([MS-PST] 2.3.1 to 2.3.3): for
//! the messages `outlook-pst` 1.2 refuses, those without a sub-node tree (no
//! recipient and no attachment), which the format allows. Values come out as
//! `outlook-pst` gives them, so an item reads the same either way.

use std::{collections::BTreeMap, io};

use crate::pim::Value;

/// A property as its context keeps it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Raw {
    /// Its value.
    Value(Value),
    /// Its value is in the sub-node of this id, not in the context.
    SubNode(u32),
}

/// The most data a block of a Unicode PST holds.
const BLOCK_SIZE: usize = 8176;
const HEAP_SIGNATURE: u8 = 0xEC;
const PROPERTY_CONTEXT: u8 = 0xBC;
const TREE_ON_HEAP: u8 = 0xB5;

const INTEGER16: u16 = 0x0002;
const INTEGER32: u16 = 0x0003;
const FLOATING32: u16 = 0x0004;
const FLOATING64: u16 = 0x0005;
const CURRENCY: u16 = 0x0006;
const FLOATING_TIME: u16 = 0x0007;
const ERROR_CODE: u16 = 0x000A;
const BOOLEAN: u16 = 0x000B;
const OBJECT: u16 = 0x000D;
const INTEGER64: u16 = 0x0014;
const STRING8: u16 = 0x001E;
const UNICODE: u16 = 0x001F;
const TIME: u16 = 0x0040;
const GUID: u16 = 0x0048;
const BINARY: u16 = 0x0102;
const NULL: u16 = 0x0001;
const MULTIPLE: u16 = 0x1000;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

fn u16_at(data: &[u8], at: usize) -> io::Result<u16> {
    data.get(at..at + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| invalid("a heap ends before one of its fields"))
}

fn u32_at(data: &[u8], at: usize) -> io::Result<u32> {
    data.get(at..at + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| invalid("a heap ends before one of its fields"))
}

/// The heap of a node ([MS-PST] 2.3.1): its blocks, each ending with its page
/// map.
struct Heap<'a> {
    blocks: Vec<&'a [u8]>,
}

impl<'a> Heap<'a> {
    /// Splits a node's data into its blocks: in a data tree, every block but
    /// the last is full ([MS-PST] 2.2.2.8.3.2). Each must hold its page map,
    /// whose offset starts every header, and allocations in order below it.
    fn new(data: &'a [u8]) -> io::Result<Self> {
        let blocks: Vec<&[u8]> = data.chunks(BLOCK_SIZE).collect();
        for block in &blocks {
            let map = usize::from(u16_at(block, 0)?);
            let count = usize::from(u16_at(block, map)?);
            let mut last = 0;
            for i in 0..=count {
                let offset = usize::from(u16_at(block, map + 4 + 2 * i)?);
                if offset < last || offset > map {
                    return Err(invalid("a heap block with allocations out of order"));
                }
                last = offset;
            }
        }
        Ok(Self { blocks })
    }

    /// The allocation a heap id names: 5 bits of type (0), 11 of index from 1,
    /// 16 of block.
    fn get(&self, id: u32) -> io::Result<&'a [u8]> {
        if id & 0x1F != 0 {
            return Err(invalid("not a heap id"));
        }
        let index = usize::from(u16::try_from((id >> 5) & 0x7FF).unwrap_or_default());
        let block = usize::from(u16::try_from(id >> 16).unwrap_or_default());
        let block = self
            .blocks
            .get(block)
            .ok_or_else(|| invalid("a heap id names a block the heap does not have"))?;
        let map = usize::from(u16_at(block, 0)?);
        let count = usize::from(u16_at(block, map)?);
        if index == 0 || index > count {
            return Err(invalid(
                "a heap id names an allocation its block does not have",
            ));
        }
        let start = usize::from(u16_at(block, map + 4 + 2 * (index - 1))?);
        let end = usize::from(u16_at(block, map + 4 + 2 * index)?);
        block
            .get(start..end)
            .ok_or_else(|| invalid("a heap allocation is outside its block"))
    }
}

/// The 8-byte records of a tree-on-heap's leaves ([MS-PST] 2.3.2), below the
/// allocation `id`, `levels` levels of 6-byte index records above them.
fn records(heap: &Heap, id: u32, levels: u8, out: &mut Vec<[u8; 8]>) -> io::Result<()> {
    if id == 0 {
        return Ok(());
    }
    let data = heap.get(id)?;
    if levels == 0 {
        out.extend_from_slice(data.as_chunks::<8>().0);
        return Ok(());
    }
    for record in data.as_chunks::<6>().0 {
        records(heap, u32_at(record, 2)?, levels - 1, out)?;
    }
    Ok(())
}

/// The properties of a property context ([MS-PST] 2.3.3), from the data of its
/// node.
pub(crate) fn property_context(data: &[u8]) -> io::Result<BTreeMap<u16, Raw>> {
    let heap = Heap::new(data)?;
    // HNHDR: ibHnpm, bSig, bClientSig, hidUserRoot, rgbFillLevel.
    let first = heap
        .blocks
        .first()
        .filter(|block| block.len() >= 12)
        .ok_or_else(|| invalid("a node without a heap"))?;
    if first[2] != HEAP_SIGNATURE || first[3] != PROPERTY_CONTEXT {
        return Err(invalid("not a property context"));
    }
    // BTHHEADER: bType, cbKey, cbEnt, bIdxLevels, hidRoot.
    let header = heap.get(u32_at(first, 4)?)?;
    if header.len() < 8 || header[0] != TREE_ON_HEAP || header[1] != 2 || header[2] != 6 {
        return Err(invalid("not the tree of a property context"));
    }
    let mut leaves = Vec::new();
    records(&heap, u32_at(header, 4)?, header[3], &mut leaves)?;
    let mut properties = BTreeMap::new();
    for record in leaves {
        // wPropId, wPropType, dwValueHnid.
        let id = u16_at(&record, 0)?;
        let prop_type = u16_at(&record, 2)?;
        let value = u32_at(&record, 4)?;
        properties.insert(id, property(&heap, prop_type, value)?);
    }
    Ok(properties)
}

/// One property: its value in the record itself, in the heap, or in a sub-node.
fn property(heap: &Heap, prop_type: u16, raw: u32) -> io::Result<Raw> {
    let bytes = raw.to_le_bytes();
    let small = match prop_type {
        BOOLEAN => Some(Value::Bool(bytes[0] != 0)),
        INTEGER16 => Some(Value::Integer(i64::from(i16::from_le_bytes([
            bytes[0], bytes[1],
        ])))),
        INTEGER32 => Some(Value::Integer(i64::from(i32::from_le_bytes(bytes)))),
        NULL | FLOATING32 | ERROR_CODE => Some(Value::Other),
        _ => None,
    };
    if let Some(value) = small {
        return Ok(Raw::Value(value));
    }
    // Values of a fixed size above 4 bytes are always in the heap; the others
    // are in a sub-node when the id is not a heap's.
    let fixed = matches!(
        prop_type,
        FLOATING64 | CURRENCY | FLOATING_TIME | INTEGER64 | TIME | GUID | OBJECT
    );
    if !fixed && raw & 0x1F != 0 {
        return Ok(Raw::SubNode(raw));
    }
    if raw == 0 {
        return Ok(Raw::Value(Value::Other));
    }
    Ok(Raw::Value(value(prop_type, heap.get(raw)?)?))
}

fn integer64(data: &[u8]) -> io::Result<i64> {
    data.get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or_else(|| invalid("a 64-bit value shorter than 8 bytes"))
}

/// UTF-16 up to the first NUL.
fn unicode(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Bytes up to the first NUL, each the character of the same number.
fn string8(data: &[u8]) -> String {
    data.iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| char::from(*byte))
        .collect()
}

/// The items of a multiple-valued property of variable size: their count, the
/// offset of each, then the items.
fn items(data: &[u8]) -> io::Result<Vec<&[u8]>> {
    let count = usize::try_from(u32_at(data, 0)?).unwrap_or(usize::MAX);
    if count > data.len() / 4 {
        return Err(invalid("a multiple value with more items than bytes"));
    }
    let mut offsets = Vec::with_capacity(count + 1);
    for i in 0..count {
        offsets.push(usize::try_from(u32_at(data, 4 + 4 * i)?).unwrap_or(usize::MAX));
    }
    offsets.push(data.len());
    offsets
        .windows(2)
        .map(|pair| {
            data.get(pair[0]..pair[1])
                .ok_or_else(|| invalid("a multiple value with an item outside it"))
        })
        .collect()
}

/// A value kept in the heap.
fn value(prop_type: u16, data: &[u8]) -> io::Result<Value> {
    Ok(match prop_type {
        INTEGER64 => Value::Integer(integer64(data)?),
        TIME => Value::Time(integer64(data)?),
        UNICODE => Value::Text(unicode(data)),
        STRING8 => Value::Text(string8(data)),
        BINARY => Value::Binary(data.to_vec()),
        t if t == MULTIPLE | INTEGER16 => Value::Integers(
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|n| i64::from(i16::from_le_bytes(*n)))
                .collect(),
        ),
        t if t == MULTIPLE | INTEGER32 => Value::Integers(
            data.as_chunks::<4>()
                .0
                .iter()
                .map(|n| i64::from(i32::from_le_bytes(*n)))
                .collect(),
        ),
        t if t == MULTIPLE | INTEGER64 => Value::Integers(
            data.as_chunks::<8>()
                .0
                .iter()
                .map(|n| i64::from_le_bytes(*n))
                .collect(),
        ),
        t if t == MULTIPLE | UNICODE => {
            Value::Texts(items(data)?.into_iter().map(unicode).collect())
        }
        t if t == MULTIPLE | STRING8 => {
            Value::Texts(items(data)?.into_iter().map(string8).collect())
        }
        _ => Value::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap block: `header`, the allocations, then the page map.
    fn block(header: &[u8], allocations: &[Vec<u8>]) -> Vec<u8> {
        let mut out = header.to_vec();
        let mut offsets = vec![out.len()];
        for allocation in allocations {
            out.extend_from_slice(allocation);
            offsets.push(out.len());
        }
        let map = u16::try_from(out.len()).unwrap();
        out[..2].copy_from_slice(&map.to_le_bytes());
        out.extend_from_slice(&u16::try_from(allocations.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        for offset in offsets {
            out.extend_from_slice(&u16::try_from(offset).unwrap().to_le_bytes());
        }
        out
    }

    /// The heap id of the allocation `index` (from 1) of the block `block`.
    fn hid(block: u32, index: u32) -> u32 {
        (block << 16) | (index << 5)
    }

    fn record(id: u16, prop_type: u16, value: u32) -> Vec<u8> {
        let mut out = id.to_le_bytes().to_vec();
        out.extend_from_slice(&prop_type.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// The first block's header: a property context whose tree header is the
    /// first allocation.
    fn first_header() -> Vec<u8> {
        let mut header = vec![0, 0, HEAP_SIGNATURE, PROPERTY_CONTEXT];
        header.extend_from_slice(&hid(0, 1).to_le_bytes());
        header.extend_from_slice(&[0; 4]);
        header
    }

    fn tree_header(levels: u8, root: u32) -> Vec<u8> {
        let mut header = vec![TREE_ON_HEAP, 2, 6, levels];
        header.extend_from_slice(&root.to_le_bytes());
        header
    }

    #[test]
    fn reads_every_kind_of_value_of_a_one_block_context() {
        let mut texts = 2u32.to_le_bytes().to_vec();
        texts.extend_from_slice(&12u32.to_le_bytes());
        texts.extend_from_slice(&20u32.to_le_bytes());
        texts.extend_from_slice(&utf16("Amis"));
        texts.extend_from_slice(&utf16("Été"));
        let leaves = [
            record(0x0017, INTEGER32, 1),
            record(0x001A, UNICODE, hid(0, 3)),
            record(0x0037, UNICODE, 0),
            record(0x0E08, INTEGER32, 0xFFFF_FFFE),
            record(0x1000, UNICODE, 0x0000_8025),
            record(0x3007, TIME, hid(0, 4)),
            record(0x3A4D, INTEGER16, 0xFFFF),
            record(0x8001, BOOLEAN, 1),
            record(0x8002, MULTIPLE | UNICODE, hid(0, 5)),
            record(0x8003, BINARY, hid(0, 6)),
            record(0x8004, STRING8, hid(0, 7)),
            record(0x8005, MULTIPLE | INTEGER32, hid(0, 8)),
        ]
        .concat();
        let data = block(
            &first_header(),
            &[
                tree_header(0, hid(0, 2)),
                leaves,
                utf16("IPM.Task"),
                129_000_000_000_000_000i64.to_le_bytes().to_vec(),
                texts,
                vec![1, 2, 3],
                vec![b'c', 0xE9, 0, b'x'],
                [7i32.to_le_bytes(), (-7i32).to_le_bytes()].concat(),
            ],
        );
        let properties = property_context(&data).unwrap();
        let value = |id: u16| match &properties[&id] {
            Raw::Value(value) => value.clone(),
            Raw::SubNode(_) => panic!("{id:04x} is in a sub-node"),
        };
        assert_eq!(value(0x0017), Value::Integer(1));
        assert_eq!(value(0x001A), Value::Text("IPM.Task".into()));
        assert_eq!(value(0x0037), Value::Other);
        assert_eq!(value(0x0E08), Value::Integer(-2));
        assert_eq!(properties[&0x1000], Raw::SubNode(0x8025));
        assert_eq!(value(0x3007), Value::Time(129_000_000_000_000_000));
        assert_eq!(value(0x3A4D), Value::Integer(-1));
        assert_eq!(value(0x8001), Value::Bool(true));
        assert_eq!(
            value(0x8002),
            Value::Texts(vec!["Amis".into(), "Été".into()])
        );
        assert_eq!(value(0x8003), Value::Binary(vec![1, 2, 3]));
        assert_eq!(value(0x8004), Value::Text("cé".into()));
        assert_eq!(value(0x8005), Value::Integers(vec![7, -7]));
        assert_eq!(properties.len(), 12);
    }

    #[test]
    fn follows_index_levels_and_later_blocks() {
        // The tree's root is an index of two leaves; the second leaf and a
        // value are in the second block, whose header is only the map's offset.
        let index = [
            [
                0x0017u16.to_le_bytes().to_vec(),
                hid(0, 3).to_le_bytes().to_vec(),
            ]
            .concat(),
            [
                0x0037u16.to_le_bytes().to_vec(),
                hid(1, 1).to_le_bytes().to_vec(),
            ]
            .concat(),
        ]
        .concat();
        let mut data = block(
            &first_header(),
            &[
                tree_header(1, hid(0, 2)),
                index,
                record(0x0017, INTEGER32, 2),
            ],
        );
        // Every block but the last is full.
        data.resize(BLOCK_SIZE, 0);
        data.extend_from_slice(&block(
            &[0, 0],
            &[record(0x0037, UNICODE, hid(1, 2)), utf16("Vider le garage")],
        ));
        let properties = property_context(&data).unwrap();
        assert_eq!(properties[&0x0017], Raw::Value(Value::Integer(2)));
        assert_eq!(
            properties[&0x0037],
            Raw::Value(Value::Text("Vider le garage".into()))
        );
    }

    #[test]
    fn refuses_what_is_not_a_property_context() {
        assert!(property_context(&[]).is_err());
        // A table context (client signature 0x7C).
        let mut header = first_header();
        header[3] = 0x7C;
        let table = block(&header, &[tree_header(0, 0)]);
        assert!(property_context(&table).is_err());
        // A heap id past the block's allocations.
        let short = block(&first_header(), &[tree_header(0, hid(0, 9))]);
        assert!(property_context(&short).is_err());
        // A block cut before its page map.
        let whole = block(&first_header(), &[tree_header(0, 0)]);
        assert!(property_context(&whole[..whole.len() - 3]).is_err());
        assert!(property_context(&whole).unwrap().is_empty());
    }
}
