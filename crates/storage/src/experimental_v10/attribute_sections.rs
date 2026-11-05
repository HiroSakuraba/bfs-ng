//! F4-on-v10 bridge: independent checksum-protected schema, row and
//! posting metadata roots, carrying the original F4 binary semantics.
//! The sections are rebuilt on changes; incremental posting CoW is later.
use super::{BlockPointer, Item, MAX_LEVEL, Page, TreeId, read_page};
use crate::fs::Catalog;
use crate::volume::ROOT_PAYLOAD_LAST;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};
use std::collections::BTreeSet;

const CHUNK: usize = 3800;
const MAX_BYTES: usize = 256 * 1024;
const MAX_PAGES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttributeRoots {
    pub rows: BlockPointer,
    pub postings: BlockPointer,
    pub schema: BlockPointer,
}
pub(crate) struct ReadAttributes {
    pub catalog: Catalog,
    pub pages: BTreeSet<u64>,
}
pub(crate) struct StagedAttributes {
    pub roots: AttributeRoots,
    pub pages: Vec<(u64, [u8; BLOCK_SIZE])>,
    pub used: usize,
}
struct Walker<'a, D: BlockDevice> {
    device: &'a mut D,
    id: u128,
    generation: u64,
    tree: TreeId,
    seen: BTreeSet<u64>,
    chunks: Vec<Vec<u8>>,
}
impl<D: BlockDevice> Walker<'_, D> {
    fn visit(&mut self, pointer: BlockPointer) -> Result<(Vec<u8>, Vec<u8>)> {
        if self.seen.len() >= MAX_PAGES || !self.seen.insert(pointer.address) {
            return Err(Error::CorruptTree);
        }
        let page = read_page(self.device, pointer, self.tree, self.id, self.generation)?;
        if page.items.is_empty() {
            return Err(Error::CorruptTree);
        }
        if page.level == 0 {
            if page.items.len() != 1 || page.items[0].key.len() != 8 {
                return Err(Error::CorruptTree);
            }
            let item = &page.items[0];
            let index =
                u64::from_be_bytes(item.key[..8].try_into().map_err(|_| Error::CorruptTree)?);
            if index != self.chunks.len() as u64
                || item.value.is_empty()
                || item.value.len() > CHUNK
                || self.chunks.len() >= MAX_BYTES.div_ceil(CHUNK)
                || (index > 0 && self.chunks.last().is_some_and(|p| p.len() != CHUNK))
            {
                return Err(Error::CorruptTree);
            }
            self.chunks.push(item.value.clone());
            return Ok((item.key.clone(), item.key.clone()));
        }
        let mut first = None;
        let mut last: Option<Vec<u8>> = None;
        for item in &page.items {
            if item.key.len() != 8 {
                return Err(Error::CorruptTree);
            }
            let child = BlockPointer::decode_required(&item.value)?;
            let (min, max) = self.visit(child)?;
            if max != item.key || last.as_ref().is_some_and(|prior| prior >= &min) {
                return Err(Error::CorruptTree);
            }
            if first.is_none() {
                first = Some(min);
            }
            last = Some(max);
        }
        Ok((
            first.ok_or(Error::CorruptTree)?,
            last.ok_or(Error::CorruptTree)?,
        ))
    }
}
fn read_section<D: BlockDevice>(
    device: &mut D,
    pointer: BlockPointer,
    tree: TreeId,
    id: u128,
    generation: u64,
) -> Result<(Vec<u8>, BTreeSet<u64>)> {
    let mut walker = Walker {
        device,
        id,
        generation,
        tree,
        seen: BTreeSet::new(),
        chunks: Vec::new(),
    };
    walker.visit(pointer)?;
    let mut bytes = Vec::new();
    for chunk in &walker.chunks {
        bytes.extend_from_slice(chunk);
    }
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(Error::CorruptTree);
    }
    Ok((bytes, walker.seen))
}
/// Reassemble the three independent physical trees, independently verify
/// posting normalizations and reject noncanonical F4 contents.
pub(crate) fn read_attribute_roots<D: BlockDevice>(
    device: &mut D,
    roots: Option<AttributeRoots>,
    id: u128,
    generation: u64,
) -> Result<Option<ReadAttributes>> {
    let Some(roots) = roots else {
        return Ok(None);
    };
    let (schema, schema_pages) =
        read_section(device, roots.schema, TreeId::Schema, id, generation)?;
    let (rows, row_pages) =
        read_section(device, roots.rows, TreeId::AttributeRows, id, generation)?;
    let (postings, posting_pages) = read_section(
        device,
        roots.postings,
        TreeId::AttributePostings,
        id,
        generation,
    )?;
    if !schema_pages.is_disjoint(&row_pages)
        || !schema_pages.is_disjoint(&posting_pages)
        || !row_pages.is_disjoint(&posting_pages)
    {
        return Err(Error::CorruptTree);
    }
    let parts = [schema, rows, postings];
    let catalog = Catalog::decode_sections(&parts)?;
    if catalog.encode_sections()? != parts {
        return Err(Error::CorruptTree);
    }
    let mut pages = schema_pages;
    pages.extend(row_pages);
    pages.extend(posting_pages);
    Ok(Some(ReadAttributes { catalog, pages }))
}
struct Builder<'a> {
    id: u128,
    generation: u64,
    slots: &'a [u64],
    staged: Vec<(u64, [u8; BLOCK_SIZE])>,
}
struct Child {
    max_key: Vec<u8>,
    ptr: BlockPointer,
}
impl Builder<'_> {
    fn page(&mut self, tree: TreeId, level: u8, items: Vec<Item>) -> Result<Child> {
        let address = *self.slots.get(self.staged.len()).ok_or(Error::NoSpace)?;
        let p = Page {
            tree,
            level,
            address,
            birth: self.generation,
            volume_id: self.id,
            items,
        };
        let bytes = p.encode()?;
        let ptr = p.pointer()?;
        let max_key = p.items.last().ok_or(Error::CorruptTree)?.key.clone();
        self.staged.push((address, bytes));
        Ok(Child { max_key, ptr })
    }
    fn section(&mut self, tree: TreeId, raw: &[u8]) -> Result<BlockPointer> {
        if raw.is_empty() || raw.len() > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        let mut children = Vec::new();
        for (index, chunk) in raw.chunks(CHUNK).enumerate() {
            children.push(self.page(
                tree,
                0,
                vec![Item {
                    key: (index as u64).to_be_bytes().to_vec(),
                    value: chunk.to_vec(),
                }],
            )?);
        }
        let mut level = 0;
        while children.len() > 1 {
            if level >= MAX_LEVEL {
                return Err(Error::TooLarge);
            }
            let mut parents = Vec::new();
            for siblings in children.chunks(90) {
                let items = siblings
                    .iter()
                    .map(|child| Item {
                        key: child.max_key.clone(),
                        value: child.ptr.encode().to_vec(),
                    })
                    .collect();
                parents.push(self.page(tree, level + 1, items)?);
            }
            children = parents;
            level += 1;
        }
        Ok(children.pop().ok_or(Error::CorruptTree)?.ptr)
    }
}
/// Pure stage of the three F4 catalog sections. Reservation addresses are
/// authorized only by the integrated writer's fresh two-F0 ownership audit.
pub(crate) fn stage_attribute_roots(
    catalog: &Catalog,
    reservations: &[u64],
    id: u128,
    generation: u64,
    total_blocks: u64,
) -> Result<StagedAttributes> {
    if generation == 0
        || id == 0
        || reservations
            .iter()
            .any(|&n| n <= ROOT_PAYLOAD_LAST || n >= total_blocks)
        || reservations.windows(2).any(|w| w[0] <= w[1])
    {
        return Err(Error::Geometry);
    }
    let parts = catalog.encode_sections()?;
    let mut builder = Builder {
        id,
        generation,
        slots: reservations,
        staged: Vec::new(),
    };
    let schema = builder.section(TreeId::Schema, &parts[0])?;
    let rows = builder.section(TreeId::AttributeRows, &parts[1])?;
    let postings = builder.section(TreeId::AttributePostings, &parts[2])?;
    let used = builder.staged.len();
    Ok(StagedAttributes {
        roots: AttributeRoots {
            rows,
            postings,
            schema,
        },
        pages: builder.staged,
        used,
    })
}
