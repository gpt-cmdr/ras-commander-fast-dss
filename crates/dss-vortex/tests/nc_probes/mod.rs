//! Reproducers of crafted files, built at test time so that nothing large is committed. Each once ended the
//! process under a 128 MiB limit, or took it nearly there. Same shapes as
//! files made with h5py and by hand.
#![allow(dead_code)]

const UNDEFINED: [u8; 8] = [0xFF; 8];

fn superblock(root: u64, eof: u64) -> Vec<u8> {
    let mut s = b"\x89HDF\r\n\x1a\n".to_vec();
    s.extend_from_slice(&[0, 0, 0, 0, 0, 8, 8, 0]);
    s.extend_from_slice(&4u16.to_le_bytes());
    s.extend_from_slice(&16u16.to_le_bytes());
    s.extend_from_slice(&0u32.to_le_bytes());
    s.extend_from_slice(&0u64.to_le_bytes());
    s.extend_from_slice(&UNDEFINED);
    s.extend_from_slice(&eof.to_le_bytes());
    s.extend_from_slice(&UNDEFINED);
    s.extend_from_slice(&0u64.to_le_bytes());
    s.extend_from_slice(&root.to_le_bytes());
    s.extend_from_slice(&[0; 24]);
    assert_eq!(s.len(), 96);
    s
}

fn pad8(mut b: Vec<u8>) -> Vec<u8> {
    b.resize(b.len().div_ceil(8) * 8, 0);
    b
}

fn message(kind: u16, body: Vec<u8>) -> Vec<u8> {
    let body = pad8(body);
    let mut m = kind.to_le_bytes().to_vec();
    m.extend_from_slice(&(body.len() as u16).to_le_bytes());
    m.extend_from_slice(&[0; 4]);
    m.extend_from_slice(&body);
    m
}

fn header(messages: &[Vec<u8>]) -> Vec<u8> {
    let body = messages.concat();
    let mut h = vec![1, 0];
    h.extend_from_slice(&(messages.len() as u16).to_le_bytes());
    h.extend_from_slice(&1u32.to_le_bytes());
    h.extend_from_slice(&(body.len() as u32).to_le_bytes());
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&body);
    h
}

/// A root header of `bytes` of Nil messages. 8 bytes in the file make one parsed message.
pub fn nil_header(bytes: usize) -> Vec<u8> {
    let n = bytes / 8 * 8;
    let mut h = vec![1, 0, 0, 0];
    h.extend_from_slice(&1u32.to_le_bytes());
    h.extend_from_slice(&(n as u32).to_le_bytes());
    h.extend_from_slice(&[0; 4]);
    h.resize(16 + n, 0);
    let mut file = superblock(96, 96 + h.len() as u64);
    file.extend_from_slice(&h);
    file
}

/// A string attribute with `refs` references to a heap object of `big` bytes, in a collection that
/// lists index 1 twice, the second empty: a walk that keeps the last length sees none of it.
pub fn repeated_heap_object(big: usize, refs: usize) -> Vec<u8> {
    let mut objects = Vec::new();
    for data in [vec![b'A'; big], Vec::new()] {
        objects.extend_from_slice(&1u16.to_le_bytes());
        objects.extend_from_slice(&1u16.to_le_bytes());
        objects.extend_from_slice(&0u32.to_le_bytes());
        objects.extend_from_slice(&(data.len() as u64).to_le_bytes());
        objects.extend_from_slice(&pad8(data));
    }
    let mut collection = b"GCOL\x01\0\0\0".to_vec();
    collection.extend_from_slice(&(16 + objects.len() as u64 + 16).to_le_bytes());
    collection.extend_from_slice(&objects);
    collection.extend_from_slice(&[0; 16]);
    let vlen_string: Vec<u8> = [
        vec![0x19, 1, 0, 0],
        16u32.to_le_bytes().to_vec(),
        vec![0x13, 0, 0, 0],
        1u32.to_le_bytes().to_vec(),
    ]
    .concat();
    let root = |heap: u64| {
        let mut space = vec![1, 1, 0, 0, 0, 0, 0, 0];
        space.extend_from_slice(&(refs as u64).to_le_bytes());
        let name = b"a\0".to_vec();
        let mut attribute = vec![1, 0];
        for length in [name.len(), vlen_string.len(), space.len()] {
            attribute.extend_from_slice(&(length as u16).to_le_bytes());
        }
        attribute.extend_from_slice(&pad8(name));
        attribute.extend_from_slice(&pad8(vlen_string.clone()));
        attribute.extend_from_slice(&pad8(space));
        for _ in 0..refs {
            attribute.extend_from_slice(&(big as u32).to_le_bytes());
            attribute.extend_from_slice(&heap.to_le_bytes());
            attribute.extend_from_slice(&1u32.to_le_bytes());
        }
        header(&[message(0x0C, attribute)])
    };
    let size = root(0).len() as u64;
    let body = root(96 + size);
    let mut file = superblock(96, 96 + size + collection.len() as u64);
    file.extend_from_slice(&body);
    file.extend_from_slice(&collection);
    file
}

/// `(chunk address, layout's offset, end)` of a one-chunk file with a version 1 B-tree.
fn chunk_tree(source: &[u8]) -> (u64, u32, usize) {
    let at = source
        .windows(5)
        .position(|w| w == b"TREE\x01")
        .expect("a chunk B-tree");
    let reference = (at as u64).to_le_bytes();
    let layout = source
        .windows(8)
        .position(|w| w == reference)
        .expect("a layout naming it");
    // Header 24 bytes; key 0 starts with the chunk's stored size, and its child follows at 32.
    let size = u32::from_le_bytes(source[at + 24..at + 28].try_into().unwrap());
    let child = u64::from_le_bytes(source[at + 56..at + 64].try_into().unwrap());
    (child, size, layout)
}

fn key(size: u32) -> Vec<u8> {
    let mut k = size.to_le_bytes().to_vec();
    k.extend_from_slice(&[0; 28]);
    k
}

fn finish(mut file: Vec<u8>, layout: usize, root: u64) -> Vec<u8> {
    file[layout..layout + 8].copy_from_slice(&root.to_le_bytes());
    let eof = file.len() as u64;
    file[40..48].copy_from_slice(&eof.to_le_bytes());
    file
}

/// A chain of `levels` internal nodes, each of 65,535 keys with one real child. They all stay alive
/// until the leaf is reached, and no entry is ever listed.
pub fn tree_chain(source: &[u8], levels: u8) -> Vec<u8> {
    let (chunk, size, layout) = chunk_tree(source);
    let k = key(size);
    let mut file = source.to_vec();
    let mut child = file.len() as u64;
    file.extend_from_slice(b"TREE\x01\0");
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&[0xFF; 16]);
    file.extend_from_slice(&k);
    file.extend_from_slice(&chunk.to_le_bytes());
    file.extend_from_slice(&k);
    for level in 1..=levels {
        let at = file.len() as u64;
        file.extend_from_slice(b"TREE\x01");
        file.push(level);
        file.extend_from_slice(&65535u16.to_le_bytes());
        file.extend_from_slice(&[0xFF; 16]);
        file.extend_from_slice(&k);
        file.extend_from_slice(&child.to_le_bytes());
        for _ in 1..65535 {
            file.extend_from_slice(&k);
            file.extend_from_slice(&UNDEFINED);
        }
        file.extend_from_slice(&k);
        child = at;
    }
    finish(file, layout, child)
}

/// A chunk index of group-type nodes, whose entries no chunk test applies to, so all are kept.
pub fn type0_index(source: &[u8], leaves: usize) -> Vec<u8> {
    let (chunk, _, layout) = chunk_tree(source);
    let mut file = source.to_vec();
    let k = [0u8; 8];
    let mut addresses = Vec::new();
    for _ in 0..leaves {
        addresses.push(file.len() as u64);
        file.extend_from_slice(b"TREE\0\0");
        file.extend_from_slice(&65535u16.to_le_bytes());
        file.extend_from_slice(&[0xFF; 16]);
        for _ in 0..65535 {
            file.extend_from_slice(&k);
            file.extend_from_slice(&chunk.to_le_bytes());
        }
        file.extend_from_slice(&k);
    }
    let root = file.len() as u64;
    file.extend_from_slice(b"TREE\0\x01");
    file.extend_from_slice(&(leaves as u16).to_le_bytes());
    file.extend_from_slice(&[0xFF; 16]);
    for address in addresses {
        file.extend_from_slice(&k);
        file.extend_from_slice(&address.to_le_bytes());
    }
    file.extend_from_slice(&k);
    finish(file, layout, root)
}

/// The one chunk's key says the rest of the file, `bytes` of it, is its stored size.
pub fn oversized_chunk(source: &[u8], bytes: usize) -> Vec<u8> {
    let (chunk, _, layout) = chunk_tree(source);
    let mut file = source.to_vec();
    file.resize(file.len() + bytes, 0);
    let root = file.len() as u64;
    let k = key(bytes as u32);
    file.extend_from_slice(b"TREE\x01\0");
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&[0xFF; 16]);
    file.extend_from_slice(&k);
    file.extend_from_slice(&chunk.to_le_bytes());
    file.extend_from_slice(&k);
    finish(file, layout, root)
}

/// A symbol-table group whose B-tree leaves list one symbol node 65,535 times each.
pub fn group_leaves(leaves: usize) -> Vec<u8> {
    let k = [0u8; 8];
    let mut body = vec![0u8; 16];
    let base = 96u64;
    let heap = base + body.len() as u64;
    body.extend_from_slice(b"HEAP\0\0\0\0");
    for value in [16u64, 16, base] {
        body.extend_from_slice(&value.to_le_bytes());
    }
    let symbols = base + body.len() as u64;
    body.extend_from_slice(b"SNOD\x01\0\0\0");
    body.extend_from_slice(&[0; 8]);
    let mut addresses = Vec::new();
    for _ in 0..leaves {
        addresses.push(base + body.len() as u64);
        body.extend_from_slice(b"TREE\0\0");
        body.extend_from_slice(&65535u16.to_le_bytes());
        body.extend_from_slice(&[0xFF; 16]);
        for _ in 0..65535 {
            body.extend_from_slice(&k);
            body.extend_from_slice(&symbols.to_le_bytes());
        }
        body.extend_from_slice(&k);
    }
    let tree = base + body.len() as u64;
    body.extend_from_slice(b"TREE\0\x01");
    body.extend_from_slice(&(leaves as u16).to_le_bytes());
    body.extend_from_slice(&[0xFF; 16]);
    for address in addresses {
        body.extend_from_slice(&k);
        body.extend_from_slice(&address.to_le_bytes());
    }
    body.extend_from_slice(&k);
    let root = base + body.len() as u64;
    let mut table = tree.to_le_bytes().to_vec();
    table.extend_from_slice(&heap.to_le_bytes());
    body.extend_from_slice(&header(&[message(0x11, table)]));
    let mut file = superblock(root, 96 + body.len() as u64);
    file.extend_from_slice(&body);
    file
}

/// A fractal heap whose root direct block is `bytes` long, with its checksum flag cleared. The block
/// is read fallibly and then copied.
pub fn big_heap_block(source: &[u8], bytes: usize) -> Vec<u8> {
    use hdf5_reader::checksum::jenkins_lookup3;
    let mut b = source.to_vec();
    let heap = b.windows(4).position(|w| w == b"FRHP").expect("a heap");
    let start = u64::from_le_bytes(b[heap + 112..heap + 120].try_into().unwrap()) as usize;
    let root = u64::from_le_bytes(b[heap + 132..heap + 140].try_into().unwrap()) as usize;
    let mut block = b[root..root + start].to_vec();
    block.resize(bytes, 0);
    let eof = b.len() as u64;
    b[heap + 9] &= !2;
    b[heap + 112..heap + 120].copy_from_slice(&(bytes as u64).to_le_bytes());
    b[heap + 120..heap + 128].copy_from_slice(&(bytes as u64).to_le_bytes());
    b[heap + 132..heap + 140].copy_from_slice(&eof.to_le_bytes());
    let sum = jenkins_lookup3(&b[heap..heap + 142]);
    b[heap + 142..heap + 146].copy_from_slice(&sum.to_le_bytes());
    let end = eof + bytes as u64;
    b[28..36].copy_from_slice(&end.to_le_bytes());
    let sum = jenkins_lookup3(&b[..44]);
    b[44..48].copy_from_slice(&sum.to_le_bytes());
    b.extend_from_slice(&block);
    b
}

/// A version 2 chunk index of `count` copies of a record, under one internal root. `pad` adds bytes
/// after each record, so the trailing bytes a count would read are not the offsets. `kind` replaces
/// the tree's type with one no chunk index has, and `one_byte` the record with a single byte.
pub fn v2_records(
    source: &[u8],
    count: usize,
    pad: usize,
    kind: Option<u8>,
    one_byte: bool,
) -> Vec<u8> {
    use hdf5_reader::checksum::jenkins_lookup3;
    let mut b = source.to_vec();
    let bthd = b.windows(4).position(|w| w == b"BTHD").expect("a B-tree");
    let old_kind = b[bthd + 5];
    let width = u16::from_le_bytes([b[bthd + 10], b[bthd + 11]]) as usize;
    let root = u64::from_le_bytes(b[bthd + 16..bthd + 24].try_into().unwrap()) as usize;
    let mut record = b[root + 6..root + 6 + width].to_vec();
    record.resize(width + pad, 0xFF);
    if one_byte {
        record = vec![0];
    }
    let kind = kind.unwrap_or(old_kind);
    const PER_LEAF: usize = 60_000;
    let leaf_size = 10 + record.len() * PER_LEAF;
    let node = |signature: &[u8; 4], body: &[u8], size: usize| -> Vec<u8> {
        let mut n = signature.to_vec();
        n.extend_from_slice(&[0, kind]);
        n.extend_from_slice(body);
        let sum = jenkins_lookup3(&n);
        n.extend_from_slice(&sum.to_le_bytes());
        assert!(n.len() <= size, "{} over {size}", n.len());
        n.resize(size, 0);
        n
    };
    let mut leaves: Vec<(u64, u16)> = Vec::new();
    let mut left = count;
    while left > 0 {
        let n = left.min(PER_LEAF);
        left -= n;
        leaves.push((b.len() as u64, n as u16));
        b.extend_from_slice(&node(b"BTLF", &record.repeat(n), leaf_size));
    }
    let mut body = record.repeat(leaves.len() - 1);
    for (address, n) in &leaves {
        body.extend_from_slice(&address.to_le_bytes());
        body.extend_from_slice(&n.to_le_bytes());
    }
    let size = leaf_size.max(10 + body.len());
    let at = b.len() as u64;
    b.extend_from_slice(&node(b"BTIN", &body, size));
    b[bthd + 5] = kind;
    b[bthd + 6..bthd + 10].copy_from_slice(&(size as u32).to_le_bytes());
    b[bthd + 10..bthd + 12].copy_from_slice(&(record.len() as u16).to_le_bytes());
    b[bthd + 12..bthd + 14].copy_from_slice(&1u16.to_le_bytes());
    b[bthd + 16..bthd + 24].copy_from_slice(&at.to_le_bytes());
    b[bthd + 24..bthd + 26].copy_from_slice(&((leaves.len() - 1) as u16).to_le_bytes());
    b[bthd + 26..bthd + 34].copy_from_slice(&(count as u64).to_le_bytes());
    let sum = jenkins_lookup3(&b[bthd..bthd + 34]);
    b[bthd + 34..bthd + 38].copy_from_slice(&sum.to_le_bytes());
    if b[8] >= 2 {
        // Superblock version 2 or 3: end of file at 28, checksum at 44.
        let eof = b.len() as u64;
        b[28..36].copy_from_slice(&eof.to_le_bytes());
        let sum = jenkins_lookup3(&b[..44]);
        b[44..48].copy_from_slice(&sum.to_le_bytes());
    }
    b
}
