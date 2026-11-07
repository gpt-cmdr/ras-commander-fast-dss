//! Hand-built files for the reader's limits: HDF5 version 0 structures and classic headers, laid out
//! byte by byte from the HDF5 File Format Specification and the NetCDF classic format, because
//! h5py and netCDF4 will not write most of them.
#![allow(dead_code)]

pub const UNDEFINED: u64 = u64::MAX;

/// An HDF5 file with 8-byte offsets and lengths, growing from the end of its superblock.
pub struct Raw {
    pub bytes: Vec<u8>,
}

impl Raw {
    pub fn new() -> Self {
        Raw { bytes: vec![0; 96] }
    }

    /// Put bytes at the next 8-byte boundary and return their address.
    pub fn place(&mut self, data: &[u8]) -> u64 {
        self.bytes.resize(self.bytes.len().next_multiple_of(8), 0);
        let at = self.bytes.len() as u64;
        self.bytes.extend_from_slice(data);
        at
    }

    pub fn patch(&mut self, at: u64, value: u64) {
        self.bytes[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn heap(&mut self, names: &[u8]) -> u64 {
        let segment = self.place(names);
        let mut heap = b"HEAP\0\0\0\0".to_vec();
        heap.extend_from_slice(&(names.len() as u64).to_le_bytes());
        heap.extend_from_slice(&UNDEFINED.to_le_bytes());
        heap.extend_from_slice(&segment.to_le_bytes());
        self.place(&heap)
    }

    /// A group holding `entries`, by name and address, with `extra` messages. Returns the object
    /// header's address and the file offset of each entry's address, for a link to something not
    /// placed yet.
    pub fn group(&mut self, entries: &[(&[u8], u64)], extra: &[Vec<u8>]) -> (u64, Vec<u64>) {
        let mut names = vec![0u8; 8];
        let mut offsets = Vec::new();
        for (name, _) in entries {
            offsets.push(names.len() as u64);
            names.extend_from_slice(name);
            names.push(0);
            names.resize(names.len().next_multiple_of(8), 0);
        }
        let heap = self.heap(&names);
        let mut node = b"SNOD\x01\0".to_vec();
        node.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (i, (_, address)) in entries.iter().enumerate() {
            node.extend_from_slice(&offsets[i].to_le_bytes());
            node.extend_from_slice(&address.to_le_bytes());
            node.extend_from_slice(&[0; 24]);
        }
        let node_at = self.place(&node);
        let patches = (0..entries.len() as u64)
            .map(|i| node_at + 8 + i * 40 + 8)
            .collect();
        let tree = self.tree(0, node_at);
        let mut messages = vec![symbol_table(tree, heap)];
        messages.extend_from_slice(extra);
        (self.place(&header(&messages)), patches)
    }

    /// A group B-tree node with one child.
    pub fn tree(&mut self, level: u8, child: u64) -> u64 {
        let mut tree = b"TREE\0".to_vec();
        tree.push(level);
        tree.extend_from_slice(&1u16.to_le_bytes());
        tree.extend_from_slice(&UNDEFINED.to_le_bytes());
        tree.extend_from_slice(&UNDEFINED.to_le_bytes());
        tree.extend_from_slice(&0u64.to_le_bytes());
        tree.extend_from_slice(&child.to_le_bytes());
        tree.extend_from_slice(&0u64.to_le_bytes());
        self.place(&tree)
    }

    pub fn finish(mut self, root: u64) -> Vec<u8> {
        let eof = self.bytes.len() as u64;
        let mut s = b"\x89HDF\r\n\x1a\n".to_vec();
        s.extend_from_slice(&[0, 0, 0, 0, 0, 8, 8, 0]);
        s.extend_from_slice(&4u16.to_le_bytes());
        s.extend_from_slice(&16u16.to_le_bytes());
        s.extend_from_slice(&0u32.to_le_bytes());
        s.extend_from_slice(&0u64.to_le_bytes());
        s.extend_from_slice(&UNDEFINED.to_le_bytes());
        s.extend_from_slice(&eof.to_le_bytes());
        s.extend_from_slice(&UNDEFINED.to_le_bytes());
        s.extend_from_slice(&0u64.to_le_bytes());
        s.extend_from_slice(&root.to_le_bytes());
        s.extend_from_slice(&[0; 24]);
        self.bytes[..96].copy_from_slice(&s);
        self.bytes
    }
}

fn symbol_table(tree: u64, heap: u64) -> Vec<u8> {
    let mut table = Vec::new();
    table.extend_from_slice(&tree.to_le_bytes());
    table.extend_from_slice(&heap.to_le_bytes());
    message(0x11, 0, &table)
}

/// A version 1 object header message, padded to 8 bytes.
pub fn message(kind: u16, flags: u8, payload: &[u8]) -> Vec<u8> {
    let padded = payload.len().next_multiple_of(8);
    let mut m = Vec::with_capacity(8 + padded);
    m.extend_from_slice(&kind.to_le_bytes());
    m.extend_from_slice(&(padded as u16).to_le_bytes());
    m.extend_from_slice(&[flags, 0, 0, 0]);
    m.extend_from_slice(payload);
    m.resize(8 + padded, 0);
    m
}

/// A version 1 object header.
pub fn header(messages: &[Vec<u8>]) -> Vec<u8> {
    let size: usize = messages.iter().map(Vec::len).sum();
    let mut h = vec![1, 0];
    h.extend_from_slice(&(messages.len() as u16).to_le_bytes());
    h.extend_from_slice(&1u32.to_le_bytes());
    h.extend_from_slice(&(size as u32).to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    for m in messages {
        h.extend_from_slice(m);
    }
    h
}

pub fn int32_type() -> Vec<u8> {
    let mut t = vec![0x10, 0x08, 0, 0];
    t.extend_from_slice(&4u32.to_le_bytes());
    t.extend_from_slice(&0u16.to_le_bytes());
    t.extend_from_slice(&32u16.to_le_bytes());
    t
}

/// `levels` of variable-length sequences of a 32-bit integer.
pub fn nested_type(levels: usize) -> Vec<u8> {
    let mut t = Vec::new();
    for _ in 0..levels {
        t.extend_from_slice(&[0x19, 0, 0, 0]);
        t.extend_from_slice(&16u32.to_le_bytes());
    }
    t.extend_from_slice(&int32_type());
    t
}

/// A variable-length string type.
pub fn vlen_string_type() -> Vec<u8> {
    let mut t = vec![0x19, 0x01, 0, 0];
    t.extend_from_slice(&16u32.to_le_bytes());
    t.extend_from_slice(&[0x10, 0, 0, 0]);
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&0u16.to_le_bytes());
    t.extend_from_slice(&8u16.to_le_bytes());
    t
}

/// A version 1 attribute message's payload.
pub fn attribute(name: &str, datatype: &[u8], dims: &[u64], data: &[u8]) -> Vec<u8> {
    let mut space = vec![1, dims.len() as u8, 0, 0, 0, 0, 0, 0];
    for d in dims {
        space.extend_from_slice(&d.to_le_bytes());
    }
    let mut a = vec![1, 0];
    a.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes());
    a.extend_from_slice(&(datatype.len() as u16).to_le_bytes());
    a.extend_from_slice(&(space.len() as u16).to_le_bytes());
    a.extend_from_slice(name.as_bytes());
    a.push(0);
    a.resize(a.len().next_multiple_of(8), 0);
    a.extend_from_slice(datatype);
    a.resize(a.len().next_multiple_of(8), 0);
    a.extend_from_slice(&space);
    a.extend_from_slice(data);
    a
}

/// A global heap collection with one object.
pub fn global_heap(object: &[u8]) -> Vec<u8> {
    let padded = object.len().next_multiple_of(8);
    let mut g = b"GCOL\x01\0\0\0".to_vec();
    g.extend_from_slice(&((16 + 16 + padded + 16) as u64).to_le_bytes());
    g.extend_from_slice(&1u16.to_le_bytes());
    g.extend_from_slice(&1u16.to_le_bytes());
    g.extend_from_slice(&[0; 4]);
    g.extend_from_slice(&(object.len() as u64).to_le_bytes());
    g.extend_from_slice(object);
    g.resize(32 + padded, 0);
    g.extend_from_slice(&[0; 16]);
    g
}

/// The root is reached from itself.
pub fn group_cycle() -> Vec<u8> {
    let mut raw = Raw::new();
    let (root, patches) = raw.group(&[(b"loop", 0)], &[]);
    raw.patch(patches[0], root);
    raw.finish(root)
}

/// Each group links to the next twice: 2^levels paths from a small file.
pub fn group_doubled(levels: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let (mut at, _) = raw.group(&[], &[]);
    for _ in 0..levels {
        at = raw.group(&[(b"a", at), (b"b", at)], &[]).0;
    }
    raw.finish(at)
}

/// `levels` groups below the root, one in the next.
pub fn group_chain(levels: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let (mut at, _) = raw.group(&[], &[]);
    for _ in 0..levels {
        at = raw.group(&[(b"g", at)], &[]).0;
    }
    raw.finish(at)
}

/// A B-tree `levels` deep over one empty symbol table node.
pub fn tree_chain(levels: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let heap = raw.heap(&[0; 8]);
    let node = raw.place(b"SNOD\x01\0\0\0");
    let mut at = raw.tree(0, node);
    for level in 1..levels {
        at = raw.tree(level.min(255) as u8, at);
    }
    let root = raw.place(&header(&[symbol_table(at, heap)]));
    raw.finish(root)
}

/// `count` objects whose headers all continue into one block of attributes.
pub fn shared_continuation(count: usize, block_bytes: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let one = message(0x0C, 0, &attribute("a", &int32_type(), &[], &[1, 2, 3, 4]));
    let mut block = Vec::new();
    while block.len() < block_bytes {
        block.extend_from_slice(&one);
    }
    let block_at = raw.place(&block);
    let mut payload = Vec::new();
    payload.extend_from_slice(&block_at.to_le_bytes());
    payload.extend_from_slice(&(block.len() as u64).to_le_bytes());
    let object = header(&[message(0x10, 0, &payload)]);
    let names: Vec<String> = (0..count).map(|i| format!("o{i}")).collect();
    let mut entries: Vec<(&[u8], u64)> = Vec::new();
    for name in &names {
        entries.push((name.as_bytes(), raw.place(&object)));
    }
    let (root, _) = raw.group(&entries, &[]);
    raw.finish(root)
}

/// One attribute with `count` references to one heap object of `object_bytes`.
pub fn repeated_heap_object(count: usize, object_bytes: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let heap = raw.place(&global_heap(&vec![b'x'; object_bytes]));
    let mut data = Vec::new();
    for _ in 0..count {
        data.extend_from_slice(&(object_bytes as u32).to_le_bytes());
        data.extend_from_slice(&heap.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
    }
    let a = attribute("history", &vlen_string_type(), &[count as u64], &data);
    let (root, _) = raw.group(&[], &[message(0x0C, 0, &a)]);
    raw.finish(root)
}

/// `count` entries sharing one name of `name_bytes`, each linking to its own object.
pub fn repeated_name(count: usize, name_bytes: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let object = header(&[message(
        0x0C,
        0,
        &attribute("a", &int32_type(), &[], &[1, 2, 3, 4]),
    )]);
    let mut node = b"SNOD\x01\0".to_vec();
    node.extend_from_slice(&(count as u16).to_le_bytes());
    for _ in 0..count {
        node.extend_from_slice(&0u64.to_le_bytes());
        node.extend_from_slice(&raw.place(&object).to_le_bytes());
        node.extend_from_slice(&[0; 24]);
    }
    let mut names = vec![b'n'; name_bytes];
    names.push(0);
    names.resize(names.len().next_multiple_of(8), 0);
    let heap = raw.heap(&names);
    let node = raw.place(&node);
    let tree = raw.tree(0, node);
    let root = raw.place(&header(&[symbol_table(tree, heap)]));
    raw.finish(root)
}

/// A dataset with `count` shared fill value references to one large header.
pub fn repeated_shared_message(count: usize, target_bytes: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let one = message(0x0C, 0, &attribute("a", &int32_type(), &[], &[1, 2, 3, 4]));
    let mut messages = vec![message(0x05, 0, &[2, 1, 0, 0])];
    let mut size = 0;
    while size < target_bytes {
        messages.push(one.clone());
        size += one.len();
    }
    let target = raw.place(&header(&messages));
    let mut reference = vec![1, 0, 0, 0, 0, 0, 0, 0];
    reference.extend_from_slice(&target.to_le_bytes());
    let mut all = vec![
        dataspace_message(&[4]),
        message(0x03, 0, &int32_type()),
        contiguous_layout_message(UNDEFINED, 0),
    ];
    all.extend((0..count).map(|_| message(0x05, 0x02, &reference)));
    let object = raw.place(&header(&all));
    let root = raw.place(&header(&[link_message("data", object)]));
    raw.finish(root)
}

/// A root attribute of `levels` nested sequence types.
pub fn nested_attribute(levels: usize) -> Vec<u8> {
    let mut raw = Raw::new();
    let a = attribute("deep", &nested_type(levels), &[], &[0; 16]);
    let (root, _) = raw.group(&[], &[message(0x0C, 0, &a)]);
    raw.finish(root)
}

fn classic_name(out: &mut Vec<u8>, name: &[u8]) {
    out.extend_from_slice(&(name.len() as u32).to_be_bytes());
    out.extend_from_slice(name);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// CDF-2 declaring `count` dimensions, zero-padded so the count fits the bytes that follow.
pub fn classic_dimension_count(count: u32) -> Vec<u8> {
    let mut d = b"CDF\x02\0\0\0\0".to_vec();
    d.extend_from_slice(&10u32.to_be_bytes());
    d.extend_from_slice(&count.to_be_bytes());
    d.resize(16 + count as usize * 16, 0);
    d
}

/// CDF-1 with one dimension of a long name and one variable using it `rank` times.
pub fn classic_repeated_dimension(name_bytes: usize, rank: u32) -> Vec<u8> {
    let mut d = b"CDF\x01\0\0\0\0".to_vec();
    d.extend_from_slice(&10u32.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    classic_name(&mut d, &vec![b'd'; name_bytes]);
    d.extend_from_slice(&1u32.to_be_bytes());
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&11u32.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    classic_name(&mut d, b"v");
    d.extend_from_slice(&rank.to_be_bytes());
    d.resize(d.len() + rank as usize * 4, 0);
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&5u32.to_be_bytes());
    d.extend_from_slice(&4u32.to_be_bytes());
    d.extend_from_slice(&0u32.to_be_bytes());
    d
}

fn classic_attribute(out: &mut Vec<u8>, name: &[u8], text: &[u8]) {
    classic_name(out, name);
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());
    out.extend_from_slice(text);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// One variable's header. Returns the offset of its `begin` field.
fn classic_variable(
    d: &mut Vec<u8>,
    name: &[u8],
    dims: &[u32],
    attributes: &[(&[u8], &[u8])],
    code: u32,
    size: u32,
) -> usize {
    classic_name(d, name);
    d.extend_from_slice(&(dims.len() as u32).to_be_bytes());
    for id in dims {
        d.extend_from_slice(&id.to_be_bytes());
    }
    d.extend_from_slice(&12u32.to_be_bytes());
    d.extend_from_slice(&(attributes.len() as u32).to_be_bytes());
    for (key, value) in attributes {
        classic_attribute(d, key, value);
    }
    d.extend_from_slice(&code.to_be_bytes());
    d.extend_from_slice(&size.to_be_bytes());
    let begin = d.len();
    d.extend_from_slice(&0u32.to_be_bytes());
    begin
}

/// CDF-1 with a rain grid on lat, lon and a time axis, for unit strings and values.
pub fn classic_time_file(units: &[u8], calendar: Option<&[u8]>, times: &[f64]) -> Vec<u8> {
    let n = times.len() as u32;
    let mut d = b"CDF\x01\0\0\0\0".to_vec();
    d.extend_from_slice(&10u32.to_be_bytes());
    d.extend_from_slice(&3u32.to_be_bytes());
    for (name, len) in [(&b"time"[..], n), (b"lat", 2), (b"lon", 2)] {
        classic_name(&mut d, name);
        d.extend_from_slice(&len.to_be_bytes());
    }
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&11u32.to_be_bytes());
    d.extend_from_slice(&4u32.to_be_bytes());
    let mut time_attributes: Vec<(&[u8], &[u8])> = vec![(b"units", units)];
    if let Some(c) = calendar {
        time_attributes.push((b"calendar", c));
    }
    let cells = n * 16;
    let begins = [
        classic_variable(&mut d, b"time", &[0], &time_attributes, 6, n * 8),
        classic_variable(&mut d, b"lat", &[1], &[(b"units", b"degrees_north")], 6, 16),
        classic_variable(&mut d, b"lon", &[2], &[(b"units", b"degrees_east")], 6, 16),
        classic_variable(&mut d, b"rain", &[0, 1, 2], &[(b"units", b"mm")], 5, cells),
    ];
    let start = |d: &mut Vec<u8>, i: usize| {
        let at = d.len() as u32;
        d[begins[i]..begins[i] + 4].copy_from_slice(&at.to_be_bytes());
    };
    start(&mut d, 0);
    for t in times {
        d.extend_from_slice(&t.to_be_bytes());
    }
    start(&mut d, 1);
    for v in [40.0f64, 41.0] {
        d.extend_from_slice(&v.to_be_bytes());
    }
    start(&mut d, 2);
    for v in [-100.0f64, -99.0] {
        d.extend_from_slice(&v.to_be_bytes());
    }
    start(&mut d, 3);
    d.resize(d.len() + cells as usize, 0);
    d
}

/// A Shared message in another object header, version 2. `declared` is its envelope's type.
pub fn shared_message(declared: u16, target: u64) -> Vec<u8> {
    let mut payload = vec![2, 2];
    payload.extend_from_slice(&target.to_le_bytes());
    message(declared, 2, &payload)
}

/// A hard link message.
pub fn link_message(name: &str, target: u64) -> Vec<u8> {
    let mut payload = vec![1, 0, name.len() as u8];
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(&target.to_le_bytes());
    message(6, 0, &payload)
}

/// The root group's only message is Shared, declared as `declared`, and its target's first real
/// message is a link back to the root. 176 bytes.
pub fn shared_hides_cycle(declared: u16) -> Vec<u8> {
    let mut raw = Raw::new();
    let root = raw.bytes.len() as u64;
    let own_size = header(&[shared_message(declared, 0)]).len() as u64;
    let target = root + own_size;
    raw.place(&header(&[shared_message(declared, target)]));
    raw.place(&header(&[link_message("loop", root)]));
    raw.finish(root)
}

/// The same with the target holding `hidden`, as the only message of the root group.
pub fn shared_hides(declared: u16, hidden: Vec<u8>) -> Vec<u8> {
    let mut raw = Raw::new();
    let target = raw.place(&header(&[hidden]));
    let root = raw.place(&header(&[shared_message(declared, target)]));
    raw.finish(root)
}

/// A symbol table message for a group holding a link to the root, hidden behind a Shared message.
pub fn shared_hides_symbol_table(declared: u16) -> Vec<u8> {
    let mut raw = Raw::new();
    let heap = raw.heap(b"\0\0\0\0\0\0\0\0loop\0\0\0\0");
    let mut node = b"SNOD\x01\0".to_vec();
    node.extend_from_slice(&1u16.to_le_bytes());
    node.extend_from_slice(&8u64.to_le_bytes());
    let root_patch = node.len() as u64;
    node.extend_from_slice(&UNDEFINED.to_le_bytes());
    node.extend_from_slice(&[0; 24]);
    let node_at = raw.place(&node);
    let tree = raw.tree(0, node_at);
    let target = raw.place(&header(&[symbol_table(tree, heap)]));
    let root = raw.place(&header(&[shared_message(declared, target)]));
    raw.patch(node_at + root_patch, root);
    raw.finish(root)
}

/// A simple dataspace message.
pub fn dataspace_message(dims: &[u64]) -> Vec<u8> {
    let mut payload = vec![1, dims.len() as u8, 0, 0, 0, 0, 0, 0];
    for d in dims {
        payload.extend_from_slice(&d.to_le_bytes());
    }
    message(1, 0, &payload)
}

pub fn datatype_message() -> Vec<u8> {
    message(3, 0, &int32_type())
}

pub fn contiguous_layout_message(address: u64, size: u64) -> Vec<u8> {
    let mut payload = vec![3, 1];
    payload.extend_from_slice(&address.to_le_bytes());
    payload.extend_from_slice(&size.to_le_bytes());
    message(8, 0, &payload)
}

/// A chunked layout, version 3. `dims` includes the element's size, as HDF5 writes it.
pub fn chunked_layout_message(address: u64, dims: &[u32]) -> Vec<u8> {
    let mut payload = vec![3, 2, dims.len() as u8];
    payload.extend_from_slice(&address.to_le_bytes());
    for d in dims {
        payload.extend_from_slice(&d.to_le_bytes());
    }
    message(8, 0, &payload)
}

/// A filter pipeline message, version 2, of filters with no parameters' worth of meaning.
pub fn filters_message(ids: &[u16]) -> Vec<u8> {
    let mut payload = vec![2, ids.len() as u8];
    for id in ids {
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
    }
    message(0x0B, 0, &payload)
}

/// A root group linking to one object of these messages.
pub fn object_with(messages: Vec<Vec<u8>>) -> Vec<u8> {
    let mut raw = Raw::new();
    let object = raw.place(&header(&messages));
    let root = raw.place(&header(&[link_message("data", object)]));
    raw.finish(root)
}

/// A root group linking to a dataset that has `messages` and a message behind a Shared one.
pub fn dataset_hiding(messages: Vec<Vec<u8>>, declared: u16, hidden: Vec<u8>) -> Vec<u8> {
    let mut raw = Raw::new();
    let target = raw.place(&header(&[hidden]));
    let mut all = messages;
    all.push(shared_message(declared, target));
    let object = raw.place(&header(&all));
    let root = raw.place(&header(&[link_message("data", object)]));
    raw.finish(root)
}

/// A compound datatype, version 3: `fields` of (name, offset, member type).
pub fn compound_type(size: u32, fields: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
    let count = fields.len() as u32;
    let mut t = vec![0x36, count as u8, (count >> 8) as u8, (count >> 16) as u8];
    t.extend_from_slice(&size.to_le_bytes());
    let width = match size {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    for (name, offset, member) in fields {
        t.extend_from_slice(name.as_bytes());
        t.push(0);
        t.extend_from_slice(&offset.to_le_bytes()[..width]);
        t.extend_from_slice(member);
    }
    t
}

pub fn int8_type() -> Vec<u8> {
    let mut t = vec![0x10, 0x08, 0, 0];
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&0u16.to_le_bytes());
    t.extend_from_slice(&8u16.to_le_bytes());
    t
}

/// An array datatype, version 3.
pub fn array_type(total: u32, dims: &[u32], base: &[u8]) -> Vec<u8> {
    let mut t = vec![0x3A, 0, 0, 0];
    t.extend_from_slice(&total.to_le_bytes());
    t.push(dims.len() as u8);
    for d in dims {
        t.extend_from_slice(&d.to_le_bytes());
    }
    t.extend_from_slice(base);
    t
}

/// A root group with one attribute of this type, `elements` of it and `data_bytes` of data.
pub fn group_attribute(datatype: &[u8], elements: u64, data_bytes: usize) -> Vec<u8> {
    let data = vec![0u8; data_bytes];
    let a = attribute("zz", datatype, &[elements], &data);
    let mut raw = Raw::new();
    let root = raw.place(&header(&[message(0x0C, 0, &a)]));
    raw.finish(root)
}

/// One byte of compound type with `fields` members all over it, `elements` of it. The reader builds
/// fields x elements values.
pub fn overlapping_compound(fields: usize, elements: u64) -> Vec<u8> {
    let member = int8_type();
    let list: Vec<(&str, u32, Vec<u8>)> = (0..fields).map(|_| ("", 0, member.clone())).collect();
    group_attribute(&compound_type(1, &list), elements, elements as usize)
}

/// The same with no two members overlapping: `fields` one-byte members, each of its own byte.
pub fn plain_compound(fields: usize, elements: u64) -> Vec<u8> {
    let member = int8_type();
    let names: Vec<String> = (0..fields).map(|i| format!("f{i}")).collect();
    let list: Vec<(&str, u32, Vec<u8>)> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i as u32, member.clone()))
        .collect();
    group_attribute(
        &compound_type(fields as u32, &list),
        elements,
        fields * elements as usize,
    )
}

/// `count` copies of [`plain_compound`]'s attribute on the root group.
pub fn plain_compounds(count: usize, fields: usize, elements: u64) -> Vec<u8> {
    let member = int8_type();
    let names: Vec<String> = (0..fields).map(|i| format!("f{i}")).collect();
    let list: Vec<(&str, u32, Vec<u8>)> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i as u32, member.clone()))
        .collect();
    let datatype = compound_type(fields as u32, &list);
    let data = vec![0u8; fields * elements as usize];
    let messages: Vec<Vec<u8>> = (0..count)
        .map(|i| {
            let a = attribute(&format!("a{i}"), &datatype, &[elements], &data);
            message(0x0C, 0, &a)
        })
        .collect();
    let mut raw = Raw::new();
    let root = raw.place(&header(&messages));
    raw.finish(root)
}

/// `chunk_index_source.nc4` with its chunk B-tree listing its one chunk `count` times.
pub fn duplicate_chunk_entries(source: &[u8], count: usize) -> Vec<u8> {
    let mut b = source.to_vec();
    let at = b
        .windows(5)
        .position(|w| w == b"TREE\x01")
        .expect("a chunk B-tree");
    let reference = (at as u64).to_le_bytes();
    let layout = b
        .windows(8)
        .position(|w| w == reference)
        .expect("a layout naming it");
    // Header 24 bytes, then key 0 (32 bytes for rank 2) and the chunk's address.
    let first = b[at + 24..at + 24 + 32 + 8].to_vec();
    let key = first[..32].to_vec();
    let mut leaves = Vec::new();
    let mut left = count;
    while left > 0 {
        let n = left.min(60_000);
        left -= n;
        leaves.push(b.len() as u64);
        b.extend_from_slice(b"TREE\x01\0");
        b.extend_from_slice(&(n as u16).to_le_bytes());
        b.extend_from_slice(&[0xFF; 16]);
        for _ in 0..n {
            b.extend_from_slice(&first);
        }
        b.extend_from_slice(&key);
    }
    let root = b.len() as u64;
    b.extend_from_slice(b"TREE\x01\x01");
    b.extend_from_slice(&(leaves.len() as u16).to_le_bytes());
    b.extend_from_slice(&[0xFF; 16]);
    for leaf in &leaves {
        b.extend_from_slice(&key);
        b.extend_from_slice(&leaf.to_le_bytes());
    }
    b.extend_from_slice(&key);
    b[layout..layout + 8].copy_from_slice(&root.to_le_bytes());
    let eof = b.len() as u64;
    b[40..48].copy_from_slice(&eof.to_le_bytes());
    b
}

/// A latest-format file with a version 2 B-tree chunk index, with that index listing its one chunk
/// `count` times: a root of depth 1 over leaves of up to 60,000 records.
pub fn duplicate_chunk_entries_v2(source: &[u8], count: usize) -> Vec<u8> {
    use hdf5_reader::checksum::jenkins_lookup3;
    let mut b = source.to_vec();
    let bthd = b
        .windows(4)
        .position(|w| w == b"BTHD")
        .expect("a B-tree header");
    let kind = b[bthd + 5];
    let record_size = u16::from_le_bytes([b[bthd + 10], b[bthd + 11]]) as usize;
    let root = u64::from_le_bytes(b[bthd + 16..bthd + 24].try_into().unwrap()) as usize;
    assert_eq!(&b[root..root + 4], b"BTLF");
    let record = b[root + 6..root + 6 + record_size].to_vec();
    let node_size = 10 + record_size * 60_000;
    let child_width = 2;
    let node = |signature: &[u8; 4], body: &[u8]| -> Vec<u8> {
        let mut n = signature.to_vec();
        n.extend_from_slice(&[0, kind]);
        n.extend_from_slice(body);
        let sum = jenkins_lookup3(&n);
        n.extend_from_slice(&sum.to_le_bytes());
        n.resize(node_size, 0);
        n
    };
    let mut leaves = Vec::new();
    let mut left = count;
    while left > 0 {
        let n = left.min(60_000);
        left -= n;
        let body: Vec<u8> = record
            .iter()
            .copied()
            .cycle()
            .take(n * record_size)
            .collect();
        leaves.push((b.len() as u64, n as u16));
        b.extend_from_slice(&node(b"BTLF", &body));
    }
    // n records between n + 1 children. Their records are the chunk's too.
    let mut body = Vec::new();
    for _ in 1..leaves.len() {
        body.extend_from_slice(&record);
    }
    for (address, n) in &leaves {
        body.extend_from_slice(&address.to_le_bytes());
        body.extend_from_slice(&n.to_le_bytes()[..child_width]);
    }
    let root_at = b.len() as u64;
    b.extend_from_slice(&node(b"BTIN", &body));
    b[bthd + 6..bthd + 10].copy_from_slice(&(node_size as u32).to_le_bytes());
    b[bthd + 12..bthd + 14].copy_from_slice(&1u16.to_le_bytes());
    b[bthd + 16..bthd + 24].copy_from_slice(&root_at.to_le_bytes());
    b[bthd + 24..bthd + 26].copy_from_slice(&((leaves.len() - 1) as u16).to_le_bytes());
    b[bthd + 26..bthd + 34].copy_from_slice(&(count as u64).to_le_bytes());
    let sum = jenkins_lookup3(&b[bthd..bthd + 34]);
    b[bthd + 34..bthd + 38].copy_from_slice(&sum.to_le_bytes());
    b
}
