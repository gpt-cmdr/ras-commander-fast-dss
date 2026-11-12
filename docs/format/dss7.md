# HEC-DSS version 7 on-disk format

These notes come from HEC's MIT-licensed `hec-dss` C source and were checked
read-only against public DSS7 files. Rust code that implements a rule from this document
cites the section here and the `heclib` function named in that section.

- **Source:** `hec-dss`, branch `master`, commit `283185bd8610ed2c4c11a597c9c202678e965498`
  (from `.git/packed-refs`). `RELEASE.md` tops out at 7-IQ-6. `DSS_VERSION` in
  `heclib_c/src/headers/hecdssInternal.h` is `"7-IX"`, dated "29 October 2025".
- **Citation form:** `file:function` (lines). Paths are relative to `heclib/heclib_c/src/`.
- **Tags:** **[UNVERIFIED]** means the code is ambiguous or we have not checked it on real
  files. Section 10 lists each one with a concrete check. **[VERIFIED-CORPUS]** means we
  confirmed it byte for byte on the files listed in section 0.
- **Audience:** each section ends with a **Reader needs / Writer only** split.

## 0. Files used for verification

These files were read read-only.
We copied no data into any repository. Only the first two files are public.

| File | Version string | Notes |
|---|---|---|
| `HEC-DSSVue/samples/sample7.dss` (HEC sample) | `7-10` | Old DSS7 revision: `kfileHeaderSize` = 0, no lock arrays, hash 4096, bin 100, stored sort list packed as int32 |
| `BaldEagleDamBrk.dss` (RasExamples) | `7-HL` | 102 RTS records, default layout |
| Private file A (local HEC-RAS run; not a fixture) | `7-HL` | 9,024 records, 87 expansions, dead space 10,405 words |
| Private file B (local HEC-RAS precipitation; not a fixture) | `7-IP` | 2,620 grid entries (type 430), of which 940 are deleted (status 11) |

All four files had:

- 100% matches between the pathname (bin) hash computed with §2.1 and the stored value
  (12,340 pathnames in total);
- 100% table-hash chain membership with §2.2;
- an info flag of −97534 at every bin info address.

---

## 1. Words, addresses, byte order, strings

| Fact | Value | Source |
|---|---|---|
| Word size | 8 bytes (`WORD_SIZE`, `LONG_SIZE`) | `headers/hecdssInternal.h` (lines 24–29) |
| Address unit | Every file address is a **word index**. Byte offset = address × 8. | `System/zreaddisk.c:zreadDisk()` (`address *= 8`) |
| Length units | Lengths are in 4-byte ints (`numberInts`) or 8-byte words, depending on the field. Each table states which. | `Internal/zget.c:zget()` (`numberInts = numberWords * wordSize`) |
| Byte order on disk | **Little-endian.** A big-endian host reverses each 8-byte word on read and write (`zswap` reverses bytes 0..7 of each word). | `System/zswap.c:zswap()`, `Internal/zinitIfltab.c` (`ifltab[kswap] = bigEndian()`), `Internal/zget.c`, `Internal/zput.c` |
| Two int32s in one word (`i4toi8` / `i8toi4`) | On little-endian, `a` is the **low** 4 bytes (bytes 0..3) and `b` is the **high** 4 bytes (bytes 4..7). Rust: `a = w as u32 as i32; b = (w >> 32) as i32`. | `Utilities/i4toi8.c`, `Utilities/i8toi4.c` |
| Strings in words | Bytes are stored in order. The last word is NUL-padded. A string of `n` bytes uses `((n-1)/8)+1` words (`numberLongsInBytes`). There is no terminator when `n` is a multiple of 8. | `Internal/charLong.c:charLong()`, `Utilities/numberLongsInBytes.c` |
| Big-endian ints and floats inside words | Big-endian hosts additionally run `zswitchInts` / `zswitchDoubles` on some arrays (TS headers and values). This matters only to readers of files written on big-endian machines. **[UNVERIFIED]** (the corpus has none: all 9,954 DSS7 files are little-endian) | `Internal/ztsRegReadBlock.c`, `Internal/ztsAggregate.c` |

**Reader needs:** all of this section. **Writer only:** nothing extra.

---

## 2. File header ("permanent section")

### 2.1 Detection and version

- Bytes 0..4 are `ZDSS`. `Internal/zpermCreate.c:zpermCreate()` (line 128) writes `"ZDSS\0"`
  into word 0.
- The version string is the 4 bytes at **16..20** (word 2, `kversion`), for example `7-IU`.
  `zpermCreate()` (line 136); `Public/zgetFileVersion.c:zgetFileVersion()` reads bytes
  16..19 and returns 5/6/7/8 from the first character. The same byte offset holds the DSS6
  version, so one check works for both formats.
- `Internal/zopenInternal.c:zopenInternal()` (lines 242–285) rejects a file when:
  - bytes 0..3 are not `ZDSS`; or
  - the first version character is not `7`. A `6` or `8` gives `INCOMPATIBLE_VERSION`.
- Numeric revision, `zinquire(…,"fver")`, in `Public/zinquire.c:zinquire()` (lines 131–136):
  `10000*major + 100*L(c2) + L(c3)`, where `L(c) = c-'@'` for `c < '['` and `c-'`'`
  otherwise (A=1 … Z=26, lowercase also maps to 1..26). Examples: `7-HA` → 70801,
  `7-IU` → 70921.
  - Digit revisions such as `7-10` (an HEC sample) produce values below 70801. HEC then
    treats them as "before 7-HA" (`Internal/ztsAggregate.c` uses the deprecated layout
    when `fver < 70801`).
- **Validity check** (`Internal/zpermRead.c:zpermRead()`, lines 78–81): word
  `kendFileHeader` (index **99**) must equal `DSS_END_HEADER_FLAG` = **−97531**. Otherwise
  the result is `INVALID_FILE_HEADER` / `CORRUPT_FILE`. The check uses the fixed index 99
  whatever `kfileHeaderSize` says.
- **Header size:** word 1, `kfileHeaderSize`. Current writers store `kendFileHeader+1` =
  100 (`zpermCreate()` lines 85–95; `DSS_MIN_FILE_HEADER_SIZE` = 100). When it is 0,
  `zpermRead()` (lines 61–64) uses 100. **[VERIFIED-CORPUS]** `7-10` files store 0; `7-HL`
  and `7-IP` store 100.

### 2.2 Word map (`zdssFileKeys`, assigned in `Internal/zinit.c:zinit()` lines 434–727; declared in `headers/zdssKeys.h` lines 91–191)

Every field is one signed 64-bit little-endian word unless noted. The byte offset is the
index × 8. Values in "Default / create" come from `zpermCreate()` and
`Internal/znewFileSize.c:znewFileSize()`.

| Idx | Bytes | Key | Meaning | Default / create | Reader |
|---|---|---|---|---|---|
| 0 | 0–7 | `kdss` | `"ZDSS"` + NUL | — | **yes** |
| 1 | 8–15 | `kfileHeaderSize` | Header length in words (0 ⇒ 100) | 100 | yes |
| 2 | 16–23 | `kversion` | 4-char version string + NUL, for example `7-IU` | `DSS_VERSION` | **yes** |
| 3 | 24 | `knumberRecords` | Live primary records | 0 | yes (sizing) |
| 4 | 32 | `knumberAliases` | Alias pathnames | 0 | yes (sizing) |
| 5 | 40 | `kfileSize` | **Address of the next free word.** That word holds the EOF flag −97532. File length ≥ (value+1)×8 bytes. | 100 initially | yes (bounds) |
| 6 | 48 | `kdead` | Dead (unreferenced) space, in words | 0 | info |
| 7 | 56 | `knumberExpansions` | Records expanded and moved since the last squeeze | 0 | info |
| 8 | 64 | `knumberCollections` | Collection pathnames | — | info |
| 9 | 72 | `knumberRenames` | Renames since squeeze | 0 | yes (sizing) |
| 10 | 80 | `knumberDeletes` | Deletes since squeeze | 0 | yes (sizing) |
| 11 | 88 | `knumberAliasDeletes` | Alias deletes | — | yes (sizing) |
| 12 | 96 | `kcreateDate` | Creation time, ms since 1970-01-01 UTC (`getCurrentTimeMillis`) | now | info |
| 13 | 104 | `klastWriteTime` | Last write time, ms since 1970 | now | info |
| 14 | 112 | `klockAddressWord` | Address of the 3 primary lock words (§6) | 100 | lock test |
| 15 | 120 | `kmaxHash` | Hash table size (number of entries) | 8192 | **yes** |
| 16 | 128 | `khashsUsed` | Non-zero table slots | 0 | info |
| 17 | 136 | `kmaxPathsOneHash` | Statistic | 0 | — |
| 18 | 144 | `kmaxPathsHashCode` | Statistic | 0 | — |
| 19 | 152 | `kaddHashTableStart` | Address of the hash table | — | **yes** |
| 20 | 160 | `khashCollisions` | Count of identical (table hash, bin hash) pairs | 0 | info |
| 21 | 168 | `kbinsPerBlock` | Bins per bin block | 32 | **yes** |
| 22 | 176 | `kbinsRemainInBlock` | Unused bins in the current block | 32 | writer |
| 23 | 184 | `kbinSize` | Bin length in words (valid range 20..1000) | 200 | **yes** |
| 24 | 192 | `kaddFirstBin` | Address of the first bin block | — | **yes** |
| 25 | 200 | `kaddNextEmptyBin` | Address of the next unused bin | — | writer |
| 26 | 208 | `ktotalBins` | Bins in use | 0 | info |
| 27 | 216 | `kbinsOverflow` | Overflow bins | 0 | info |
| 28 | 224 | `kfilePassword` | Encoded password (unused, 0) | 0 | — |
| 29 | 232 | *(gap)* | No key in heclib: `kfileError = kfilePassword + 2` (`Internal/zinit.c` lines 535-548; the same in tags 7-IG to 7-JA-7). heclib never reads or writes it and preserves it (`zpermWrite()` writes the cached header back whole). **The native writer uses it as its in-progress marker**: the new info address from the Allocation step until the write finishes, or minus the info address of a record being deleted (§6.4); 0 in all 25 private DSS7 test copies | 0 | — |
| 30 | 240 | `kfileError` | Unused | — | — |
| 31 | 248 | `kfileErrorCode` | Unused | — | — |
| 32 | 256 | `kcatSequenceNumber` | Catalog sort sequence. Copied into the high half of each bin's type word. Marked "obsolete" in the source. | — | — |
| 33 | 264 | `kcatSortStatus` | 0 none, 1 sorted and usable, 2 records added, 3 deletes/renames (unusable) | 0 | optional |
| 34 | 272 | `kcatSortNewWrites` | — | — | — |
| 35 | 280 | `kcatSortDeletes` | — | — | — |
| 36 | 288 | `kcatSortSize` | Physical length of the stored sort list | 0 | optional |
| 37 | 296 | `kcatSortNumber` | Entries in the sort list | 0 | optional |
| 38 | 304 | `kcatSortAddress` | Address of the sort list (§4.4) | 0 | optional |
| 39 | 312 | `kreclaimMin` | Smallest reclaimable segment (words); 0 means no reclamation | 100 | writer |
| 40 | 320 | `kreclaimMaxAvailable` | Largest reclaimable segment | — | writer |
| 41 | 328 | `kreclaimTotal` | Total reclaimable words | 0 | writer |
| 42 | 336 | `kreclaimTableAddress` | Address of the first reclaim table | — | writer |
| 43 | 344 | `kreclaimSegAvailableAdd` | — | — | writer |
| 44 | 352 | `kreclaimSegNumber` | — | 0 | writer |
| 45 | 360 | `kreclaimMaxSegment` | Maximum reclaim tables | 20 | writer |
| 46 | 368 | `kreclaimSegmentsUsed` | — | 0 | writer |
| 47 | 376 | `kreclaimNumber` | (size, address) pairs per table | 500 | writer |
| 48 | 384 | `kreclaimSize` | Reclaim table length in words | 1002 | writer |
| 49 | 392 | `kreclaimedPaths` | Statistic | — | — |
| 50 | 400 | `kreclaimedSpace` | Statistic | — | — |
| 51 | 408 | `kmaxPath` | Longest pathname, in bytes | 0 | sizing |
| 52–57 | 416–463 | `kmaxA`…`kmaxF` | Longest A..F part | 0 | sort |
| 58 | 464 | `kmaxInternalHeader` | Maximum internal header length (int32 units) | 0 | sizing |
| 59 | 472 | `kmaxHeader2` | — | 0 | sizing |
| 60 | 480 | `kmaxUserHeader` | — | 0 | sizing |
| 61–63 | 488–511 | `kmaxValues1Size`..`3` | — | 0 | sizing |
| 64–69 | 512–559 | `knumberInternalHeader`, `knumberHeader2`, `knumberDataArea3`, `knumberUserHeader`, `knumberDataArea1`, `knumberDataArea2` | Statistics | — | — |
| 70 | 560 | `kmaxRecordSize` | Words | 0 | sizing |
| 71–77 | 568–623 | `kmaxRtsSize`, `kmaxRtdSize`, `kmaxItsSize`, `kmaxItdSize`, `kmaxPdSize`, `kmaxPddSize`, `kmaxOtherSize` | Per-type maxima | — | — |
| 78 | 624 | `kmaxExpectedPathnames` | Value passed at creation | 0 | — |
| 79–83 | 632–671 | `kerrorMemory`, `kerrorAddress`, `kerrorWrite`, `kerrorRead`, `kerrorTotal` | Error counters | — | — |
| 84–88 | 672–711 | `klocBoundLR`, `LL`, `UR`, `UL`, `Elev` | Location bounds | — | — |
| 89 | 712 | `kdetune` | **Test only:** when non-zero, every hash is 3 / 1234 (`zhash()` lines 200–208) | 0 | **reject if ≠0** |
| 90 | 720 | `klockArraySizes` | Words in each lock array | 25 (`DSS_LOCK_ARRAY_SIZE`) | lock test |
| 91 | 728 | `klockWriteArrayAddress` | Passive write-lock array | 103 | lock test |
| 92 | 736 | `klockReadArrayAddress` | Passive read-lock array; **0 means old locking scheme** | 128 | lock test |
| 93 | 744 | `kpidArrayAddress` | PID array | 153 | info |
| 94 | 752 | `kendian` | 1 if ever written on a big-endian host | 0 | info (see §10 item 5) |
| 95–98 | 760–791 | `kreserved1..4` | Reserved | 0 | — |
| 99 | 792–799 | `kendFileHeader` | **−97531** | — | **yes** |

### 2.3 Layout of a newly created file (`zpermCreate()` lines 225–357) **[VERIFIED-CORPUS on 7-HL and 7-IP]**

| Words | Content |
|---|---|
| 0–99 | Permanent section |
| 100–102 | Primary lock words: primary, exclusive/advisory, extra (`klockAddressWord`). Written as zeros. |
| 103–127 | Write-lock array (25) |
| 128–152 | Read-lock array (25) |
| 153–177 | PID array (25): `mode × 10,000,000 + pid` (`Internal/zpidWrite.c`) |
| 178 … 178+maxHash−1 | Hash table (8192 by default, so words 178–8369) |
| 8370–9371 | First reclaim table (1002) |
| 9372 … | First bin block: 32 × 200 words + 1 next-block word. The first record's info block starts at 15773 when defaults are used. |
| `kfileSize` | EOF flag −97532 (`Internal/zwriteEOF.c:zwriteEOF()`) |

Old `7-10` files differ. There are no lock arrays (`klockReadArrayAddress` = 0); the hash
table starts at 110, `kmaxHash` = 4096 and `kbinSize` = 100. Readers must use the header
pointers and never assume these offsets.

**Hash table size selection** (`znewFileSize()` lines 95–137):

| Expected paths (create-time hint) | `kmaxHash` | `kbinSize` |
|---|---|---|
| ≤ 0 (default) or ≤ 10,000 | 8192 | 200 |
| ≤ 50,000 | 8192 | 150 |
| ≤ 100,000 | 8192 | 200 |
| ≤ 500,000 | **16348** (not a power of 2, as written in the source) | 200 |
| ≤ 1,000,000 | 32768 | 300 |
| larger | 64000 | 300 |

- Collection files (`newCollectionFile`) use `kbinSize` = 400.
- An explicit `hashSize` is accepted from 1 to 100000 and an explicit `binSize` from 1 to
  400. `kbinsPerBlock` is always 32.

**Reader needs:** words 0–2, 5, 15, 19, 21, 23, 24, 89, 99, and 3/4/9/10/11 for capacity
hints. Validate everything:

- `kbinSize` ∈ [20, 1000], as in `zpermRead()` line 88;
- `kmaxHash` ≥ 1. The native reader accepts 1..1,000,000. HEC's automatic sizing writes at
  most 64000 and an explicit `hashSize` at most 100,000 (`znewFileSize()` lines 97–100).
  Clamp `ibit` (§3.2) to ≥ 1: `kmaxHash` = 1 gives `frexp` = (0.5, 1), so `ibit` = 0
  and `zhash()` would divide by zero;
- every address is ≥ the header size and < `kfileSize`;
- `kfileSize` × 8 ≤ the file length.

**Writer only:** everything else, including the statistics. HEC updates the statistics,
and a writer that skips them still produces a valid file, but HEC's tools report them.

---

## 3. Hash table, pathname bins and lookup

### 3.1 Pathname ("bin") hash: `Internal/zhash.c:zhash()` lines 89–130 **[VERIFIED-CORPUS 12,340/12,340]**

1. Copy the pathname, of length `n`, into a C `char` buffer. Each byte is loaded into an
   `int` from a signed `char` (`ich = pathname[i]`, lines 91–100). If `ich < 32` it
   becomes `'?'`; otherwise it becomes `toupper(ich)`. The buffer is followed by 8 NUL
   bytes.
   - On MSVC/x86 `char` is signed, so every byte ≥ 0x80 is negative, tests `< 32`, and
     is hashed as `'?'`. Such bytes are **not** sign-extended. The native reader implements
     this rule.
   - Platforms with unsigned `char` would hash these bytes differently. No DSS7 corpus
     pathname contains a byte ≥ 0x80 (a survey of 2,846 private corpus files), so this stays
     source-only (§10 item 1).
2. `h: i64 = 0; for c in bytes: h = h.wrapping_mul(31).wrapping_add(c as i64)`. This is
   Java's `String.hashCode` widened to 64 bits. After step 1 every buffer byte is in
   32..127, so it is never negative.
3. If `h == 0`, then `h = 1`. A stored bin hash is never 0, and 0 marks the end of a bin.

### 3.2 Table hash: `zhash()` lines 145–195 **[VERIFIED-CORPUS]**

Inputs: the uppercased buffer `p` (with its NUL padding), `n`, and `maxHash = kmaxHash`.

```
(frac, e) = frexp(maxHash)          // maxHash = frac * 2^e, 0.5 <= frac < 1
ibit = e; if frac == 0.5 { ibit -= 1 }   // ibit = log2(maxHash) for powers of 2
nt = ((n*8 - 1) / ibit) + 1
nt = ((nt*ibit - 1) / 8) + 1        // number of bytes consumed (may exceed n; padding is 0)
it = 0; i2 = 0; ineed = ibit; ihave = 0; ibyte = 0
loop {
  if ihave > 0 {
    imove = min(ineed, ihave); i2 <<= imove; ineed -= imove; ihave -= imove
    if ineed <= 0 { itp = i2 / 256; i2 %= 256; it ^= itp; ineed = ibit }   // C int, truncating
  }
  if ihave <= 0 {
    ibyte += 1; if ibyte > nt { break }
    i2 += p[ibyte-1] as i8 as i32; ihave = 8
  }
}
tableHash = it % maxHash            // uses the §3.1 buffer, so `it` is never negative
```

- The algorithm XORs successive `ibit`-bit groups of the bit stream of the uppercased
  pathname.
- Collections: `zhash` calls `zcollectionsPath()` only to return the "is a collection"
  flag. The header comments in `zhash.c` (line 137) and `zcollectionsPath.c` say collection
  members share one table hash because the sequence is replaced with `XXXXXX`. **The
  current code does not do that substitution** (`Internal/zcollectionsPath.c:zcollectionsPath()`
  only calls `zisaCollection`). In this source, collection members are hashed like any
  other pathname **[UNVERIFIED for older writers]**; see §10 item 2.
- Pathnames longer than 392 bytes are truncated to 392 before hashing and lookup
  (`Internal/zcheckInternal.c:zcheckInternal()` line 125).

### 3.3 Hash table

- `kmaxHash` words at `kaddHashTableStart`. Slot `t` (at address
  `kaddHashTableStart + t`) holds the address of the **first pathname bin** for that table
  hash. A value of 0 means no record has that table hash (`zcheckInternal()` lines
  212–246).

### 3.4 Pathname bin (`zdssBinKeys`, `zinit()` lines 742–753; layout comment `zcheckInternal()` lines 277–308)

A bin is `kbinSize` words long and holds packed entries. Each entry is
`kbinSize_entry = 7` words followed by the pathname words.

| Off | Key | Content |
|---|---|---|
| +0 | `kbinHash` | Pathname (bin) hash, §3.1. **0 means no more entries in this bin.** |
| +1 | `kbinStatus` | Record status (§3.6) |
| +2 | `kbinPathLen` | `i4toi8(numberChars, pathnameSizeWords)`: low = pathname bytes, high = words |
| +3 | `kbinInfoAdd` | Address of the record's info block. For an alias: the primary's info, or the previous alias's bin entry (§3.7). |
| +4 | `kbinTypeAndCatSort` | `i4toi8(dataType, catSequenceNumber)`: low = record type (§5.3) |
| +5 | `kbinLastWrite` | Last write, ms since 1970. This is the value HEC's catalog struct reports (`zcatalogInternal` → `lastWriteTimeRecord`). It equals `kinfoLastWriteTime` in most files but can be later (§5.1). |
| +6 | `kbinDates` | `i4toi8(julianFirst, julianLast)`, in days (§7). Meaningful only for types 100..199; 0 otherwise. `Internal/zcatalogInternal.c` lines 392–398. |
| +7 … | `kbinPath` | Pathname bytes, NUL-padded to `pathnameSizeWords` words |

Next entry offset: `icount += 7 + pathnameSizeWords`
(`zcheckInternal()` line 415, `zbinUpdate()` lines 224/280).

End of a bin's entries (`zcheckInternal()` lines 312–420):

- an entry whose `kbinHash` is 0; **or**
- `icount >= kbinSize − 2`.

The writer only places an entry when `icount + 7 + size + 1 < kbinSize`
(`Internal/zbinUpdate.c:zbinUpdate()` lines 231–245). **Word `kbinSize−1` of every bin is
the overflow link:** the address of the next bin with the same table hash, or 0 if there
is none (`zcheckInternal()` line 426; `Internal/zbinNew.c:zbinNew()` lines 86–99). Word
`kbinSize−2` appears to be unused **[UNVERIFIED]**.

### 3.5 Bin blocks

- New bins are taken from a **block** of `kbinsPerBlock` (32) contiguous bins.
- Block size: `kbinSize × kbinsPerBlock + 1` words. The final word links to the next
  block's address, or 0 (`zbinNew()` lines 86–134; `zpermCreate()` lines 339–349).
- Bins inside a block are allocated in order, so an all-zero bin means the rest of that
  block is unused.
- Blocks are created only when the previous block is full, so only the last block can be
  partial.

### 3.6 Record status codes (`headers/zdssVals.h` lines 79–87; `Internal/zcompareRecordStatus.c:zcompareRecordStatus()`)

| Value | Name | Where used | Live? |
|---|---|---|---|
| 0 | `REC_STATUS_VALID` | Bin status after **undelete** (`Internal/zdeleteInternal.c` writes 0). Also in the `7-10` info status. **[VERIFIED-CORPUS]** one corpus file has 187 status-0 entries; all are live and both oracles list them. | **yes** (treated as primary; `statusRead <= 1`) |
| 1 | `REC_STATUS_PRIMARY` | Normal record | **yes** |
| 2 | `REC_STATUS_ALIAS` | Alias pathname | yes (resolves to primary) |
| 10 | `REC_STATUS_MOVED` | **Info** status of an expanded record's old copy (`Internal/zwriteExisting.c`) | no — dead space |
| 11 | `REC_STATUS_DELETED` | Bin and info after delete | no (may be undeletable) |
| 12 | `REC_STATUS_RENAMED` | Old bin entry after rename. Also the old info if the info moved. | no |
| 13 | `REC_STATUS_ALIAS_DELETED` | — | no |
| 15 | `REC_STATUS_REMOVED` | Deleted and its space reused | no |
| −1 | — | Listed in the `zinit()` comment ("expanded and moved"), but the code uses 10 | treat as not live |

`zcompareRecordStatus` treats any `statusRead <= 1` (including negative values) as live.
**We should accept only 0 and 1 as primary, and 2 as alias.** Any other value is skipped
and reported as an anomaly.

### 3.7 Lookup and alias resolution (`zcheckInternal()` lines 199–476; `Internal/zreadInfo.c:zreadInfo()`)

1. Compute `tableHash` and `binHash` (§3.1, §3.2).
2. `binAddr = word[kaddHashTableStart + tableHash]`. If it is 0, the record is not found.
3. Walk the entries of the bin (§3.4). A match requires both:
   - `kbinHash == binHash`;
   - `numberChars == n`.
   HEC then always confirms with a case-insensitive byte compare (`boolDoubleCheckPath = 1`;
   `Public/zpathnameCompare.c`).
4. Check the status against the wanted status (§3.6). If there is no match, follow
   `bin[kbinSize-1]` and repeat until it is 0.
5. Alias (status 2): read 8 words at `kbinInfoAdd`.
   - If word 0 is −97534 (`DSS_INFO_FLAG`), this is the info block.
   - Otherwise it is another bin entry whose status must be 2 (else
     `INVALID_BIN_STATUS` / corrupt); set the address to that entry's `+3` and repeat
     (`zcheckInternal()` lines 444–475).
   - Bound this loop.
6. Read the info block: `30 + ceil(n/8)` words at the info address (`zreadInfo()`). If
   `info[kinfoHash] != binHash`, the space was reused and the record is not found.
   - **Alias caveat:** `zreadInfo()` compares the info hash with the hash of the
     *looked-up* pathname. For an alias that is the alias's hash, but the info block
     belongs to the primary. The native reader skips the hash check for aliases. An earlier
     concern that HEC's alias read fails as written is superseded by the check below: HEC
     reads aliases correctly.
   - **[VERIFIED-HEC]** On the HEC-written `synthetic_catalog_ops7.dss`, HEC's C
     library reads three aliases (two of one primary block, one of the second block)
     with the primary's values, and the engine agrees. The primary pathname is read from
     the info block with the primary's own length (`Public/zaliasGetPrimary.c` lines
     126-138, `7-IU-15`). `zaliasRemove()` sets status 13 and stores one character fewer
     (lines 150-162), so the removed alias's bin pathname has no final slash.

**Reader needs:** all of §3. A catalog-only reader can skip the hash (§4); point lookup
needs it. **Writer only:** §3.5 allocation, collision counting, and reuse of
deleted-entry slots (`kbinWithSpace`, `zbinUpdate()` lines 120–191).

---

## 4. Catalog

### 4.1 Enumeration (`Internal/zcatalogInternal.c:zcatalogInternal()` lines 177–487)

HEC does **not** use the hash table to catalog. It scans bin blocks in physical order:

```
addr = kaddFirstBin
loop:
  for j in 0..kbinsPerBlock:
     bin = read(addr, kbinSize)
     if bin[0] == 0: stop scanning this block (and, because it's the last block, the file)   // line 280
     walk entries (§3.4); for each entry whose status matches: emit
     addr += kbinSize
  addr = word[addr]          // block link (the word after the 32nd bin)
  if addr == 0: done
```

- The only place HEC reads the block link is after a full block. When the scan stops at
  an empty bin, HEC reads that empty bin's first word (0) and ends. A Rust reader should
  stop explicitly.
- Each emitted entry gives: pathname, data type (low half of +4), bin hash, last write
  (+5), and first/last julian day (+6, types 100–199 only).
- Default status filter (`catStruct->statusWanted = 0`, meaning valid): status ≤ 1, or 2
  for aliases.
- When deleted records are requested and reclamation is on, HEC also checks that the info
  block still has status 11 (lines 350–360).
- Expected capacity: `numberRecords + numberAliases + numberRenames + numberDeletes +
  numberAliasDeletes` (lines 186–188). Writers may append during a scan; HEC truncates.
- **Order:** this unsorted catalog is in physical bin order, which is effectively hash
  order. **[VERIFIED-CORPUS]** HEC's unsorted catalog order (`hec_dss_catalog` on copies)
  equals bin order on 25 of 25 files read with hecdss, 435,593 entries (25 private test
  files).
- **Orphan bins** (in the block chain, but no hash slot or overflow link reaches them):
  HEC's catalog lists their entries, but lookup cannot reach them. **[VERIFIED-CORPUS]**
  on all 12 loose files with orphan bins (a survey of 2,846 private corpus files). When a pathname
  is duplicated, the hash-reachable copy is the one lookup returns; the stale copies sit
  in orphan bins.
- **Unlinked bins** (the reverse case): a hash-table slot or overflow link reaches a bin
  that is not in the bin-block chain. Because this walk never uses the hash table, HEC's
  catalog **omits** their entries, but a lookup **finds** them. **[VERIFIED-CORPUS]** in
  one damaged `7-IQ` corpus file: 2,930 slots reach 2,970 unlinked bins holding
  15,471 live entries. It is an interrupted-write signature (§6.4).
- **Recovery mode** (a reader option, not HEC behaviour): `dss_core::dss7::CatalogOptions::recover_unlinked`
  also walks the bins that the hash table and overflow links reach outside the block
  chain, and appends their entries after the bin-order entries. In this mode every copy
  of a duplicated pathname is checked, and the best-preserved copy wins: a valid info
  block first, then the latest info write time, then HEC's lookup order. HEC's own rule
  (and the default mode) takes the first copy in the hash chain, which can be a damaged
  copy when an intact one exists. On that file recovery yields 217,564 distinct live
  records, all with valid info blocks, against 202,291 valid in the default catalog. On
  undamaged files it changes nothing (2,846 private corpus files).

### 4.2 Deleted records, dead space, renames, expansions

- Bin entries with status 11, 12, 13 or 15 stay in the bins and must be skipped.
  **[VERIFIED-CORPUS]** Private file B (§0) has 940 entries with status 11.
- Expanded records: the new info is written at EOF and the bin's `+3` is updated. The old
  info block remains on disk with info status 10; it is reachable only by scanning the file.
- Dead space is a counter (`kdead`), not a list. The reclaim tables (§2.2, words 39–48)
  list reusable (size, address) segments, but readers never consult them.
- A reader must never scan the data area linearly. It must reach info blocks only through
  bin entries.

### 4.3 Collections

- An F part shaped `C:<seq>|<rest>` (the `C` is case-insensitive) marks a collection
  member (`Internal/zisaCollection.c:zisaCollection()`).
- `knumberCollections` counts them.
- A collection catalog is a normal scan filtered with `zpathnameCompareCollection`.

### 4.4 Sorted catalog and stored sort list (`Internal/zcatInternalSort.c`, `Internal/zcatSortPath.c`, `Internal/zgetCatalogSortAddresses.c`, `Internal/zsetCatalogSortAddresses.c`)

- **Sort key** (`zcatSortPath()`): fixed-width fields, each padded to that part's file-wide
  maximum length (`kmaxA..F`).
  - Normal order: A B C D E F.
  - Time series (100–199): **A B C F E D**, with E converted to interval seconds and D to
    julian + 10,000,000, both zero-padded so they sort numerically.
  - Grids (400–499): **A B C F D E**, with D converted to julian + 100000 and minutes.
  - Ties are broken by index. The sort is an external text sort (`sortfiles`).
- **Stored sort list:** when `kcatSortStatus == 1`, HEC reorders using the list at
  `kcatSortAddress`:
  - `[−97536] [kcatSortNumber addresses of bin entries] [−97537]`. The addresses come from
    `zcatalogInternal` (line 390): `binAddress + icount`.
  - The `zinit()` comment says −97535/−97536; the header constants (−97536 start, −97537
    end) are correct. **[VERIFIED-CORPUS]**
  - **The `7-10` sample stores this list as packed int32 pairs.** The end flag is at
    `addr + ceil(n/2) + 1`, not `addr + n + 1`.
  - **Recommendation:** never trust the stored list; sort in Rust with the key above.
- **Side effect:** a sorted catalog in HEC may **write** the sort list into the file
  (`zcatInternalSort()` line 476 calls `zsetCatalogSortAddresses`, which needs write
  access). A comparison harness must open such files read-only or work on copies.
- **Condensed catalog:** no DSS7 C code exists. "Condensed" catalogs are produced only by
  the Fortran DSS6 catalog writers (`heclib_f/src/zcat6.f`, `zcatit.f`,
  `Support/makedsscatalog.f`). It is a presentation that groups TS D parts and has no
  on-disk structure. That belongs to the DSS6 notes (`dss6.md`) or the Python layer.

**Reader needs:** §4.1–§4.3, plus the §4.4 sort key if catalog order must match HEC.
**Writer only:** the sort list and its status words (writers should set
`kcatSortStatus` = 2 or 3 as HEC does: `zwriteNew` sets 1→2; delete/rename sets 3).

---

## 5. Record info block ("record header")

### 5.1 Layout (`zdssInfoKeys`, `zinit()` lines 809–920; written by `Internal/zwriteNew.c:zwriteNew()`) **[VERIFIED-CORPUS]**

`zdssVals.infoSize` = **30** words before the pathname. Total = `30 + ceil(pathLen/8)`
words, plus space for up to 20 alias words (`maxInfoSize`).

| Idx | Key | Content (units) |
|---|---|---|
| 0 | `kinfoFlag` | **−97534** (`DSS_INFO_FLAG`) |
| 1 | `kinfoStatus` | §3.6. 1 = good; 10 moved; 11 deleted; 12 renamed. `7-10` files store 0. |
| 2 | `kinfoPathnameLength` | Pathname bytes |
| 3 | `kinfoHash` | Pathname (bin) hash; must equal the bin entry |
| 4 | `kinfoTypeVersion` | `i4toi8(dataType, version)`: low = type; high = number of times written (starts at 1) |
| 5 | `kinfoExpansion` | `i4toi8(numberExpansions, expansionFlag)` |
| 6 | `kinfoLastWriteTime` | ms since 1970. In `7-10` files the stored values are seconds-like (about 1.25e9, that is 2009), but HEC (Monolith) still interprets them as ms (15 Jan 1970); the bins in the same files hold real ms (§3.4). **[VERIFIED-CORPUS]** (corpus survey). The bin value `kbinLastWrite` equals this word in most files, but is later in `7-10` (all records), in some `7-IC`/`7-IQ` files, and by about 47 minutes for every record of one 309k-record file. |
| 7–8 | `kinfoProgram` | Program name, 16 bytes, NUL-padded (for example `"not specified"`, `"DSSUTL"`) |
| 9 | `kinfoFirstDate` | `i4toi8(julianDay, secondsOfDay)` of the **first stored** value (TS), not the block start. It equals the first valid (non-missing) value only when the writer does not store leading missing values. `(j, 86400)` is common: a first value at 24:00, for example (43350, 86400). 0 for non-TS. **[VERIFIED-CORPUS]** 3,378/3,393 sampled TS records match the first non-missing value from hecdss; a follow-up check shows every exception is a stored value that is missing (see the exceptions list after this table). |
| 10 | `kinfoLastDate` | Same, for the last **stored** value. **[VERIFIED-CORPUS]** 3,390/3,393 match the last valid value. The bin `kbinDates` days equal the info days in 3,393/3,393. |
| 11 | `kinfoCreationTime` | ms since 1970 |
| 12 | `kinfoReserved1` | Unused (formerly a record password) |
| 13 | `kinfoInternalHeadAddress` | Address |
| 14 | `kinfoInternalHeadNumber` | Length in **int32** |
| 15 | `kinfoHeader2Address` | Compression header, address |
| 16 | `kinfoHeader2Number` | int32 |
| 17 | `kinfoUserHeadAddress` | Address |
| 18 | `kinfoUserHeadNumber` | int32 |
| 19 | `kinfoValues1Address` | Address |
| 20 | `kinfoValues1Number` | int32 used |
| 21 | `kinfoValues2Address` | Address |
| 22 | `kinfoValues2Number` | int32 |
| 23 | `kinfoValues3Address` | Address |
| 24 | `kinfoValues3Number` | int32 |
| 25 | `kinfoAllocatedSize` | int32 allocated for values1..3 (≥ used; room for growth) |
| 26 | `kinfoNumberData` | Number of logical values from `blockStartPosition` to `blockEndPosition` (the uncompressed count), **not** the count of stored elements **[VERIFIED-CORPUS]**. For irregular records it equals the decoded value count (13,827/13,827 in a corpus run). **[RESOLVED]**: it is the number of block positions stored, uncompressed: `blockEnd − blockStart + 1` (`ztsRegStoreBlock.c` lines 791–836; `ztsWriteBlock.c` around lines 230–250; `zwriteNew.c:294`). It is 1 when only one valid value is stored. On 11 private DSS7 test files, all 7,348 records with `kinfoNumberData ≠ kinfoLogicalNumber` satisfy this. The about 1,200 corpus files with the value 1 are single-position uncompressed blocks, trimmed to one valid value; they are not all-repeat compressed blocks. |
| 27 | `kinfoLogicalNumber` | Expanded count the user sees (for example, 1440 for a 1-minute daily block) |
| 28 | `kinfoAliasesBinAddress` | Address of the **last** alias's bin entry, or 0 (`Public/zaliasAdd.c` line 198) |
| 29 | `kinfoReserved` | 0 |
| 30… | `kinfoPathname` | Pathname bytes, NUL-padded |

- **Areas are contiguous and word-aligned,** in this order:
  - info;
  - internal header (`ceil(n/2)` words);
  - header2;
  - user header;
  - values1, values2, values3, with the allocation following
  (`zwriteNew()`; `numberLongsInInts`).
- The zinit block comment (lines 760–806) mentions a "compression flag / precision" word.
  **That comment is stale; the key table above is authoritative.**
- **`kinfoFirstDate` / `kinfoLastDate` exceptions** **[VERIFIED-CORPUS]**:
  they are the time of the first and last *stored* value. They differ from the first and
  last valid value only where a stored value is itself missing:

  | Writer | Exceptions |
  |---|---|
  | 7-10 | 3 last |
  | 7-GT | 2 first, 1 last |
  | 7-IC irregular gauges | 2,545 first, 38 last, and 6,139 all-missing blocks that still carry stamps |

  Files from 7-HL, 7-IE and 7-IU match the valid values 100%. Writers should set both words
  the way current writers do: the first and last valid value.

### 5.2 Headers

- **Internal header:** int32 array whose meaning depends on the type.
  - Time series: §7.4.
  - Paired data: `INT_HEAD_pdNumberOrdinates` 0, `pdNumberCurves` 1,
    `pdBoolIndependentIsXaxis` 2, `pdLabelsLength` 3, `pdPrecision` 4, `pdUnits` 5.
  - Text: `INT_HEAD_text_size` 6.
  - Grids: `INT_HEAD_grid_*` 0–15 and 24.
  - Sources: `headers/zStructAllocation.h` lines 87–117 and `headers/zStructSpatialGrid.h`
    lines 92–108.
- **Header2:** compression information (repeat bit masks for TS, §7.5).
- **User header:** opaque int32s owned by the application.

**Paired data (200/205)** (`Public/zpdRetrieve.c:zpdRetrieve()`,
`Internal/zpdUnitsToHead.c:zpdHeadToUnits()`) **[VERIFIED-CORPUS]**: 17,032
DSS7 PD records of private test files (17,031 PD + 1 PDD; 34 multi-curve, 34 labelled) are identical to both
hecdss and Monolith, and 405,187 sampled corpus records decode with 0 errors.

- **Data areas:**
  - values1 holds the `numberOrdinates` ordinates.
  - values2 holds `numberOrdinates*numberCurves` values, curve-major. Each value is `float`
    (200) or `double` (205).
  - header2 holds the labels: `INT_HEAD_pdLabelsLength` bytes of NUL-terminated strings.
- **Label parsing:** one string per curve, stopping once the position reaches
  `labelsLength`. If header2 exists but `labelsLength` is 0, HEC returns one empty label.
  Leading blanks are kept (a label ` 5%LIMIT` reads identically in hecdss and Monolith).
- **Strings:**
  - Unit strings start at int 5 (deprecated layout) or at int 6 (when int 5 is 0 or
    blanks, `unitsHavePadding()`, as for TS, §7.4).
  - There are five NUL-terminated strings, at most 400 bytes in total: units independent,
    type independent, units dependent, type dependent, time zone.
  - **The first empty string ends the list** (`zpdHeadToUnits()` lines 122-123), so an empty
    independent-units string hides every later field. One record in `sample7.dss` shows all
    four unit/type strings empty for this reason, while its DSS6 twin stores
    `"" / PROB / cfs / LOG`. hecdss and Monolith show the same; readers reproduce it.
  - Blank-only strings mean "not set"; the others are trimmed.
- **Other:** `INT_HEAD_pdPrecision` is `10x + y`, and negative means not set. A header
  shorter than 5 ints is `WRONG_RECORD_TYPE`. In the corpus, 99.99% of PD records use
  padded units (405,150 of 405,187 sampled); the exceptions are 7-10 and 7-GT files.
- **Sizes** come from the internal header (`INT_HEAD_pdNumberOrdinates` and
  `INT_HEAD_pdNumberCurves`), never from `kinfoLogicalNumber` (`dss6.md` §12 O9).
- No public fixture contains PDD; no vertical-datum conversion is applied unless HEC's
  `VDTM` option is set (neither oracle sets it).

**Grids (400–431)** **[VERIFIED-CORPUS]**. Decoded
bit-for-bit against Monolith (and hecdss where it can read the layout) on 14 private
test files (7,784 grids) and on 57,487 loose-corpus grids with 0 errors. The first int of
the internal header selects one of two layouts (`Public/zspatialGridRetrieve.c:
zspatialGridRetrieveVersion`):

- **First int `-100`: modern layout.**
  - Internal header ints 0..15 and 24 (`headers/zStructSpatialGrid.h` lines 92–108,
    tag `7-IU-15`): 0 `structVersion`, 1 `type`, 2 `version`, 3 `dataType`,
    4/5 `lowerLeftCellX/Y`, 6/7 `numberOfCellsX/Y`, 8 `compressionMethod`,
    9 `sizeofCompressedElements`, 10 `numberOfRanges`, 11 `srsDefinitionType`,
    12 `timeZoneRawOffset`, 13 `isInterval`, 14 `isTimeStamped`, 15 `storageDataType`,
    24 `internalHeaderNumber`. **[RESOLVED from the header file; an earlier "0..15 only" note
    omitted index 24]**
  - header2 holds five strings.
  - The user header holds the geometry (four geometry floats).
  - values1 holds statistics and range tables.
  - values3 holds the cells.
  - Cells are `f32`, x varying fastest, no row flip; compression method 1 (none) or 26
    (ZLIB; the wrapper and checksum follow `Public/zlibcompress.c:uncompress_zlib`). The
    inflated buffer is exactly `nx*ny*4` bytes. **For odd-sized grids one alignment float
    follows the cells** (`zspatialGridStore.c`, `numLongs*2*4`); the stream must end there.
  - Stored statistics keep their bits, including non-finite values. The missing-value
    policy is independent of the file name: modern grids always keep stored bits.
  - **Null value (`_nullValue`, geometry float 3)** **[VERIFIED-ORACLE]**: HEC's C
    header documents it as "the value used in the grid's data array to represent a 'no
    data' cell" (`headers/zStructSpatialGrid.h:74`) for every grid, but Monolith carries
    it only for specified grids: `GridStructContainer`'s `(GridInfo)` constructor copies
    `SpecifiedGridInfo._nullValue` (offsets 432-440 in `javap -c`, Monolith 3.3.27) and
    `returnGridInfo` copies it back (offsets 422-430); no other grid-info class has the
    field. So an HRAP or Albers grid Monolith writes to DSS7 stores the Java default 0.0
    (`Public/zspatialGridStore.c:199`), and Monolith's readers treat those 0.0 cells as
    data (`GridData.getNumberMissing` counts only -9999 and -FLT_MAX). The reader keeps
    the stored value in `Grid.null_value` but, matching Monolith, applies it as a missing
    marker only for specified grids (`Grid::is_missing`; Python `grid_missing_mask`).
    Checked on `fixtures/public/real_grid7.dss` (4 affected records, 94,306 zero cells)
    and on the 14 grid-bearing private test files (7,784 grids: missing counts equal
    Monolith's under both rules; 2 private HRAP/Albers grids store a null value other than
    -FLT_MAX, 0.0 in one and a non-zero value in the other, and no cell equals either).
- **First int non-negative: legacy flat layout**, in the internal header and in the values1
  payload. The internal header is the same Java `GridInfoFlat` int array as DSS6 grids
  (`dss6.md` §5.2; compression method is **int 15**), with the HRAP, Albers or Specified tail.
  - **A positive first int (for example 300) is the legacy flat-header byte size, not a
    modern struct revision.** Applying the `-100` offsets to it mislabels ordinary header
    fields (lower-left, dimensions, cell size, compression) as compression, storage type and
    SRS values. Never read legacy storage codes or SRS from modern offsets.
  - The first-int size can be stale (296 on specified headers whose actual transfer is
    longer); the actual transfer length is authoritative.
  - Legacy compression 26 (ZLIB) and 101001 (PRECIP_2_BYTE) occur. Legacy ZLIB missing
    values follow Java heclib: values `<= -1e38` become the undefined marker, and
    specified null markers and HRAP/Albers values outside the stored min/max are normalized
    too. PRECIP_2_BYTE missing runs decode to the undefined marker.
- **Strings:** specified WKT and time-zone strings are returned verbatim except trailing
  Hollerith padding/NUL termination. HRAP supplies heclib's HRAP WKT. **[OPEN: for
  modern `-100` HRAP (410) records the Rust reader keeps the stored string, while Monolith
  `HrapInfo.getSpatialReferenceSystem` always returns its HRAP constant; no corpus
  instances]** Legacy Albers headers
  store no WKT: every stored projection parameter is returned, and the reported WKT follows
  the Albers SRS rule below; other legacy Albers projections keep their stored parameters only.
- **Albers SRS rule** **[VERIFIED-ORACLE: `javap -p -c` of the pinned Monolith 3.3.27
  jar; 4,955 Albers grids in 7 private test files and 81 synthetic records compared with
  Monolith]**.
  Monolith reports either `SHG_SRC_DEFINITION` or `""` for every Albers grid and never
  another WKT. `AlbersInfo.getSpatialReferenceSystem` returns the SHG WKT only when all of
  these hold, otherwise `""`:
  - datum `== 2`;
  - the units satisfy `AlbersInfo.isMeters`: `toLowerCase()`, then an exact match of
    `meters`, `meter`, `m`, `metres` or `metre`, with no trimming;
  - first and second standard parallel, central meridian, latitude of origin, false
    easting and false northing `==` 29.5, 45.5, -96.0, 23.0, 0.0 and 0.0;
  - the cell-zero origin x and y `== 0`. These are Java float compares: NaN fails and -0.0
    passes.

  The inputs come from the layout:
  - **Legacy layouts** (`GriddedData.retrieveGriddedData` -> `AlbersInfo.convertToGridInfo`)
    use ints 63..74 of the flat header (`dss6.md` §5.2). The units string is
    `Heclib.holString(units, 12)`: the bytes before the first NUL, with **trailing blanks
    kept**. So `Meters` padded with blanks is not metres, but `Meters` followed by NUL and
    junk is.
  - **Modern `-100` layout** (`GridStructContainer.returnGridInfo`, type 420 only):
    `setAlbersSpatialReferenceInfo` parses the stored WKT with regular expressions, then
    discards it.
    - Datum: the first match of `DATUM\["D_North_American_19(\d\d)`; 27 gives 1, 83
      gives 2, any other digits give 0, and no match gives 0. So
      `D_North_American_1983_HARN` counts as NAD83.
    - Each parameter: the first match of `PARAMETER\["<name>",(-?\d*\.*\d+)`, parsed with
      `Float.parseFloat`. No match (for example a blank after the comma) or a parse failure
      (`29..5`) gives 0.0, so a WKT without a false easting still passes.
    - Units: the **last** match of `UNIT\["(.*?)"` (`.` excludes line terminators), which
      is normally the linear unit. No match gives `""`.
    - The origin comes from the geometry x0/y0.
    - **[VERIFIED-ORACLE]** The Rust reader reports these parsed values as
      `Grid.albers` for modern 420/421 records (`grid::albers_from_wkt`; Monolith
      `GridStructContainer.returnGridInfo` offsets 288-326 and
      `setAlbersSpatialReferenceInfo` offsets 0-207). Equal to Monolith on the 3 modern
      Albers records of `real_grid7.dss`, the 25 modern records of
      `synthetic_albers7.dss` and the 2 modern Albers grids of two private test files.

    Monolith creates no grid-info class for a modern 421 record, so its decode fails there.
    The Rust reader applies the same rule to modern 421 records and keeps the stored string
    separately (`stored_srs_definition`).
- **Corpus decoded (raw first int, compression, count)**: DSS7 420 `-100` ZLIB 16,997;
  420 `-100` uncompressed 1; 420 raw 300 ZLIB 6,353; 420 raw 300 PRECIP_2_BYTE 1,323;
  430 `-100` ZLIB 22,684. No DSS7 HRAP grids exist in the corpus.

- **Writing (`dss7::write::write_grid`)** **[VERIFIED-ORACLE]**. A grid record is
  the five areas `Public/zspatialGridStore.c:zspatialGridStore()` hands to `zwrite()`,
  filled the way HEC Monolith 3.3.27 fills `zStructSpatialGrid`
  (`GridStructContainer(GridInfo)`, `javaHeclib/src/Hec_zgridStore.c`):
  - the internal header is 24 ints (indexes 16..23 zero); `version` is 1, 2 for
    specified grids; HRAP and Albers store `isInterval` and `isTimeStamped` 0 and time
    zone offset 0;
  - header2: HRAP `units, source, "WKT", HRAP WKT, "UTC"`; Albers `units, "", "WKT",
    WKT, ""`; specified its own five strings;
  - the user header is 11 ints; Monolith stores 0.0 as the HRAP/Albers null value (the
    native writer stores -FLT_MAX);
  - values1 is `min, max, mean`, 13 range limits and 13 counts
    (`GridData.updateStatistics`); values3 the cells: ZLIB of `numLongs*2` floats, or
    plain (`sizeofCompressedElements` = `n*4`);
  - `kinfoNumberData` and `kinfoLogicalNumber` are 0; `kinfoFirstDate`/`kinfoLastDate`
    come from the D and E parts (E empty: the D time). **HEC quirk:** without a D part
    heclib leaves `ifltab[kdataFirstDate]`/`kdataLastDate` unchanged
    (`zspatialGridStore.c` lines 297-317), so such a record gets the previous store's
    dates on the same handle; the writer stores 0;
  - `zwriteInternal()` keeps `kmaxValues3Size`, `knumberDataArea3` and, for grid types,
    `kmaxOtherSize` (lines 281-317); PD and PDD both keep `kmaxPdSize`.
  - For non-metre Albers units Monolith's built WKT ends `UNIT["<units>",0.0]]`: a
    conversion factor of 0.0, which other WKT tools reject; the writer reproduces it for
    Monolith parity (readers here and Monolith take only the unit name).
  - Native and Monolith records from the same empty file are equal word for word except
    the write times, the program name, the HRAP/Albers null value and the deflate stream
    (identical in 6 of 18 records; the inflated cells always equal).
### 5.3 Record (data) type codes (`headers/zdataTypeDescriptions.h`)

| Code | Abbr | Name |
|---|---|---|
| 0 | — | Other / undefined |
| 20 | LOC | Location (internal record, pathname `/A/B/Location Info////`, `Public/zlocationPath.c`) |
| 90 | ARR | Array (generic) |
| 91 / 92 / 93 | — | Int / float / double array |
| 100 | RTS | Regular-interval TS, float |
| 101 | RTPA | Regular-interval TS pattern (float) |
| 102 | RTPR | Regular-interval TS profile (float) |
| 105 | RTD | Regular-interval TS, double |
| 106 | — | Regular-interval TS pattern, double |
| 107 | RTPD | Regular-interval TS profile, double |
| 110 | ITS | Irregular-interval TS, float |
| 111 | ITPA | Irregular pattern |
| 112 | ITPR | Irregular profile |
| 115 | ITD | Irregular TS, double |
| 116 | — | Irregular pattern, double |
| 117 | ITPD | Irregular profile, double |
| 200 | PD | Paired data, float |
| 205 | PDD | Paired data, double |
| 300 | TXT | Text |
| 310 | TT | Text table |
| 400 / 401 | GUT / GU | Grid, undefined projection, with / without time |
| 410 / 411 | GHT / GH | Grid, HRAP, with / without time |
| 420 / 421 | GAT / GA | Grid, Albers, with / without time |
| 430 / 431 | GST / GS | Grid, specified (SHG in `zdssRecordDiscriptions.h`), with / without time |
| 450 | TIN | Spatial TIN |
| 600 | FILE | Generic file |
| 610 | IMAGE | Image |

- **Profiles** **[VERIFIED-CORPUS]**: 102 is the RTS profile (float) and 107
  is the RTD profile (double). Profile values are rows of `profileDepthsNumber` values; the
  depths are in values2. One 107 record decodes bit-identically to Monolith (366 x 449
  values, 366 times); hecdss crashes on it (access violation). Irregular profiles
  (112/117) and irregular patterns (111/116) remain unverified (no corpus data).
- Types < 50 are internal: HEC does not record them as the "last type"
  (`zcheckInternal()` line 366).
- HEC's default catalog still lists them. **[VERIFIED-CORPUS]** type 20 records are listed
  by both hecdss and Monolith.
- Overwriting an existing record with a different type is refused, except
  RTS↔RTD and ITS↔ITD (`zwriteExisting()`).

**Reader needs:** §5.1 (validate the flag and the hash; accept info status 0/1; bound every
address by `kfileSize`), and §5.3. **Writer only:** version/expansion bookkeeping and the
allocation policy (`zwriteExisting()` grows by ¼, ½, full, then +10% of
`totalExpandedSize`).

---

## 6. Locking, multi-user access and write ordering

### 6.1 Lock words (`zpermCreate()` lines 234–302; `Internal/zlockActive.c`, `Internal/zlockPassive.c`, `System/zlockdss.c:zlockDss()`)

These are byte-range locks on 8-byte words. The byte offset is the address × 8.

- Windows uses `_locking`, which takes **mandatory** region locks: `_LK_NBLCK` to try,
  `_LK_LOCK` to wait (10 retries).
- POSIX uses `lockf` (`F_TLOCK`, `F_TEST`, `F_ULOCK`), which is fcntl/POSIX
  record-lock-compatible on Linux and macOS. **It is not `flock`.**

| Word(s) | Purpose | Who holds it |
|---|---|---|
| `klockAddressWord` (100) | **Primary write lock** (8 bytes; 16 bytes covering 100..101 for exclusive) | A writer during each write. In "advisory" mode (`SINGLE_USER_ADVISORY_ACCESS` = 3) it is held from the first write until close or until another process wants to write. |
| `klockAddressWord+1` (101) | Exclusive / advisory-request word | Exclusive opens (squeeze, copy) |
| `klockAddressWord+2` (102) | Unused ("compatibility") in the new scheme; exclusive in the old scheme | — |
| `klockWriteArrayAddress` … +24 | Passive **write** markers. Each writing process locks one free word on its first write and keeps it until close. | Every HEC writer |
| `klockReadArrayAddress` … +24 | Passive **read** markers. **Every HEC open** (read-only included: `zopenInternal()` calls `zlockPassive(…, LOCKING_ACCESS_READ)`) locks one free word until close. | Every HEC process with the file open |
| `kpidArrayAddress` … +24 | Not a lock: `mode × 10,000,000 + pid` (`Internal/zpidWrite.c`) | Informational |

- **Old files** (`klockReadArrayAddress == 0`, for example `7-10`) use `zlockingOld`:
  fixed `klockAddress = 100`, words 100..102, and no passive arrays
  (`Internal/zlockingOld.c` line 127).
- HEC tests for another holder by taking a lock and immediately releasing it (mode 3)
  (`zlockDss()`; `zlockPassive()` LOCK_TEST loop).

### 6.2 What "refuse to write to a file locked by HEC software" should test

A writer opens the file and, without writing, performs a non-blocking test lock (try
lock, then unlock) on:

1. the primary lock word (8 bytes at `klockAddressWord × 8`);
2. each word of the write-lock array;
3. each word of the read-lock array.

Recommended policy:

- Refuse (`DssLockedError`) if any of (1) or (2) is held.
- Also refuse if any word of (3) is held. An HEC process with the file open, even
  read-only, can become a writer later, and in advisory mode it caches the header and
  does not flush. **[OPEN DECISION]**
- Old-scheme files: test words 100..102 only. A passive HEC reader cannot be detected.

### 6.3 Reader implications

- Our reader takes no locks and writes nothing. HEC readers do not see our reader, which
  is fine.
- HEC writers in advisory mode buffer writes and update the permanent section only on
  unlock or flush (`zlockActive()` unlock path; `zpermWrite()` is "called near the end of a
  write sequence"). A concurrent read can see a stale header or partially linked
  structures. Readers must bounds-check, tolerate dangling addresses as errors (not
  panics), and may re-read the header and retry once.
- Windows mandatory locks make **reads** of locked byte ranges fail. Never read words
  100..(read-array end) on Windows. Only §2–§5 structures need to be read, and none of
  them overlap the lock area. Behaviour of memory-mapped reads against `_locking` regions
  is **[UNVERIFIED]** (§10 item 10); prefer positioned reads.

### 6.4 HEC write ordering (for contrast)

`Internal/zwriteInternal.c:zwriteInternal()` and `zwriteNew()` run in this order:

1. lock (and re-read the header);
2. for a new table hash, `zbinNew`, then write the table slot;
3. `zbinUpdate` writes the **bin entry pointing at a not-yet-written info block**;
4. write the info block (buffered);
5. write the headers and values (buffered);
6. write the EOF flag and flush;
7. write the header on unlock.

Expansions mark the old info as MOVED **before** writing the new one
(`zwriteExisting()`). HEC relies on the lock and its buffers, **not** on "data first,
pointers last".

**Interrupted-write signatures** seen in the corpus (rerun with full info
checks). A reader must report these as anomalies, not fail:

- table slots pointing at allocated but all-zero bins (one private test file: 433 slots,
  `ktotalBins` 9,937 against 9,504 walked);
- table slots and overflow links pointing at whole bin blocks missing from the block
  chain (the damaged `7-IQ` file of §4.1 "Unlinked bins");
- zeroed info blocks behind live bin entries (6,437 of 208,732 walked entries in that
  file; 7,800 in the private test file above);
- `knumberRecords` ahead of the walk (that file: 251,496 against 208,732; even with
  recovery, 33,932 records are in no reachable bin).

Our writer (`dss_core::dss7::write`, crash-tested after every step by
`crates/dss-core/tests/p64_kill.rs` and the writer fault tests) runs, per
record:

1. **Data:** allocate at EOF and write the record (and a new zeroed bin block if one is
   needed). The info block is written with status MOVED (10), so heclib's brute-force
   `zcheckLinks()` treats it as dead space until step 9.
2. **EndOfFile:** write the EOF flag at the new `kfileSize`; flush.
3. **Allocation:** the header with the new `kfileSize` and the in-progress marker in
   header word 29 (see §2.2) holding the new info address; flush. HEC allocates at
   `kfileSize`, so nothing later overwrites the new space.
4. **Chain** (new bin block only): the previous block's chain word → the new, all-zero
   block. It comes after step 3, so a block is never chained past `kfileSize`.
5. **Retire** (rewrite only): the old info status → MOVED. Readers do not test the
   info status (`zreadInfo()` compares the hash only), so the bin still reads the old
   copy, and `zcheckLinks()` never sees two primary copies.
6. **Publish:** the bin entry, words +1..+7 first and the hash word +0 last (a bin entry
   becomes visible as soon as its hash word is non-zero). For an existing record it is
   the bin's `+3` info address together with +4..+6 (type, write time, first/last days),
   in **one** write call: process death does not split a single `write`. Engines up to
   0.1.0 wrote them in two calls, and death between them left the new info block
   published under the old type and dates (finding **R17**). An entry placed in a
   **new** bin is not reachable yet.
7. **Counters:** every header counter and statistic and the bin allocation state
   (`kaddNextEmptyBin`, `kbinsRemainInBlock`, `ktotalBins`), marker still set. The bin
   state reaches the disk before any slot points at the new bin, so heclib's
   `zbinNew()` (which hands out `kaddNextEmptyBin` without looking at it) never gives
   that bin to another chain.
8. **Link:** the table slot or overflow link to a new bin, which already holds the
   entry. No slot ever points at an empty bin. Allocating the bin in the header before
   its entry exists would instead risk an empty bin in front of used ones, and heclib's
   catalog walk stops at the first empty bin (`zcatalogInternal()` lines 275-282, then
   it reads that bin's first word as the next block address).
9. **Activate:** the new info status → 1.
10. **Finish:** the marker cleared.

The next native write reconciles a file whose marker is set, from the file itself: an
entry that names the pending info block but that no chain reaches is linked (roll
forward). A published entry whose words 4-6 do not describe the pending info block
(the R17 state of step 6) gets them from that info block, keeping the entry's catalog
sequence number. The bin cursor is moved past bins in use, and the only bin cleared is
the cursor bin holding this write's own leftover entry body (below). The pending
info block is activated or a retired old copy restored, and `knumberRecords`,
`khashsUsed`, `kbinsOverflow`, `khashCollisions`, `kmaxPathsOneHash`, `ktotalBins` and the
pathname maxima are rebuilt (`kdead` and the area statistics are not).

**When HEC writes first after an interrupted native write** (heclib keeps word 29; it
writes its cached header back whole), verified by having the heclib 7-JA C library write
a record under a fresh table hash and one in the crashed record's chain after every
crash point: no bin is ever shared and every other record stays readable. Two windows
leave `zcheckFile` errors until the next native write repairs them:

- a new record in a **new bin**, interrupted after Counters and before Link: its entry
  stays unlinked (HEC catalogs list it, reading it fails, `zcheckFile` reports it). If
  interrupted after Publish but before Counters, heclib's next new bin reuses that bin
  and the unpublished entry is simply gone;
- a new record in an **existing bin**, interrupted after Publish and before Counters:
  `knumberRecords` stays one low.

The crash model is **process death** (the OS keeps every completed `write`). The steps
above say "flush" where the transaction has a **flush point**. Whether the writer calls
`fsync` there is the flush policy, `WriteOptions.sync` (Python `sync=`), with three modes:

| Mode | `fsync` calls | Notes |
|---|---|---|
| `SyncMode::End` (`sync="end"`, or `True`) | One `sync_all` at the end of the write call, before the lock is released. None at the flush points | **The default since 0.1.2.** A call that stores several records (gridded import, grid tools, time tools, grid to point) writes them without a flush and flushes once after the last |
| `SyncMode::Step` (`sync="step"`) | `sync_data` at every flush point, then the same final `sync_all` | The default up to 0.1.1. The flush points are: the record area together with the EOF flag (the Data step has no flush of its own), the allocation header, the chain word, an entry body before its hash word and the hash word itself, the counters, the link, the activation and the cleared marker. A step with nothing to write flushes nothing |
| `SyncMode::None` (`sync="none"`, or `False`) | None | Opt-in since 0.1.1 |

The three modes make **the same `write` calls in the same order**; only the `fsync` calls
differ. Under process death they therefore leave the same states at every point, and
the next write reconciles them the same way: the killed-process matrix
(`crates/dss-core/tests/p64_kill.rs`) runs every kill point in all three modes.
A single `write` of the 800-byte header spans two 512-byte
sectors and is not atomic on power loss; neither is heclib's. **No power-loss claim is
made in any mode**: `Step` orders what reaches storage, but that ordering
was never qualified against power loss, and `End` and `None` do not order it at all.
Damaged files are recovered by copy-out (`repair_dss7`, Python `repair`; threat model
below).

**Interrupted Publish of a new bin (finding F1, repaired in 0.1.1).** The
killed-process matrix (`crates/dss-core/tests/p64_kill.rs`) kills the writing process at
every flush point, not only between the steps above. Up to 0.1.0 one point was not
repaired: a write killed inside **Publish** for a record in a **new bin**, after the
entry body (words 1..) and before its first word. Reconcile skipped that bin as in use
because a word other than the first was non-zero, the repairing write took the next
bin, and the catalog walk, which stops at the first bin whose first word is zero (ours
and `zcatalogInternal()` line 280), no longer reached it or any later bin. The engine now follows
heclib's own rules instead:

- **Bin-empty test.** A bin is empty when its first word (the entry hash) is zero
  (`zcatalogInternal()` line 280, `zbinUpdate()` line 194). Reconcile step 2 and the
  bin allocator test only that word, so the half-written bin stays the cursor.
- **Allocation.** `zbinNew()` (lines 130-139) hands out `kaddNextEmptyBin` without
  reading it and clears the bin in memory; `zbinUpdate()` (line 271) writes the whole
  bin. The native writer now writes a new bin whole too: words 1..`kbinSize` (entry body,
  zero tail, zero overflow word), flushed, then the hash word. These are the same writes
  in the same order as before, with the zero tail added to the body write; it is needed
  so a shorter entry cannot leave the killed entry's tail behind as a phantom entry. An
  entry added to an existing bin, and the reuse of a deleted entry, are unchanged.
- **Leftover body.** Reconciliation clears words 1..`kbinSize` of the cursor bin only
  when they are exactly this interrupted write's own entry body.
  Every word except the unwritten hash word 0 must equal what Publish writes, as derived
  from the marker's info block and the header:
  - word 1: status 1;
  - word 2: the pathname length words;
  - word 3: the marker's info address;
  - word 4: the info block's record type and the header's `kcatSequenceNumber`;
  - word 5: the info block's last-write time;
  - word 6: the Julian days of its first and last dates;
  - then the pathname that info block names, and zeros to the end of the bin.

  Anything else non-empty there is not the write's own, so reconciliation refuses with
  `WriteError::Corrupt` and writes nothing. Clearing keeps
  the file clean when the next write takes no new bin: heclib's `zcheckPathnameBins()`
  (lines 111-125) requires every word after a bin's last entry to be zero, except the
  overflow word.
- **Cursor validation.** Before reconciliation writes anything, the
  allocation cursor must be what heclib keeps. That means `kaddNextEmptyBin` =
  `block + (kbinsPerBlock − kbinsRemainInBlock) · kbinSize` for a block of the
  bin-block chain, with `0 ≤ kbinsRemainInBlock ≤ kbinsPerBlock` (`zbinNew()` lines 114,
  123, 131-134). Both the stored cursor and the cursor reconciliation will reach are
  checked. If either is invalid, reconciliation refuses and writes nothing.
- **Roll-forward link targets.** Before roll-forward writes the link,
  every bin that the pending pathname's chain reaches must be the start of a bin of the
  bin-block chain, with no loop. This covers the hash-table slot and then each overflow
  word. It is the same bounded walk as cursor validation. Otherwise reconciliation
  refuses and writes nothing. The link then goes only into the slot, or into the
  overflow word of a verified bin.
- **What reconciliation writes.** In place, reconciliation of an
  interrupted write writes only locations that this write's own marker and allocation
  state identify:
  - its bin entry and link (the roll-forward of step 1);
  - its own leftover bin body (step 2b, exact match only);
  - its info block (step 3; for a delete, the rollback of the info status);
  - the header counters, which are written together with the cleared marker.

  It never repairs earlier damage found by scanning. A hidden-bin repair, and the
  allocation map it needed, were tried during development and removed: a scanned
  "protected set" cannot be proven complete on an arbitrarily damaged file. Damaged files are recovered by copy-out instead (below).
- **Threat model (maintainer decision).**
  - **Guarantee.** A file that was consistent before an interrupted write is reconciled
    exactly, after ordinary process death at any point: no reachable record changes. The
    kill matrix and the HEC checks test this. "Any point" means between any two separate
    `write` calls: the matrix kills the writer at every flush point,
    every step boundary, and immediately before every separate write of both engines.
  - **Damage outside the guarantee.** Independent extra damage on top of that, such as
    corrupted pointers that process death cannot produce, is outside the in-place
    guarantee, as it is for heclib. Reconciliation and normal writes refuse it when
    **cheap, bounded** checks detect it:
    - the cursor (cursor validation above);
    - the roll-forward link targets;
    - the exact leftover match (leftover body above);
    - the allocator's non-empty and permanent-section checks.

    They do not promise to detect every case. No whole-file ownership map is built on
    the in-place path.
  - **Safe path.** For a damaged file it is copy-out, `repair_dss7`.
  - **Documented limitations** of these checks: R7 (below) and R12.
- **Known limitation R7 (present in 0.1.0 and 0.1.1).** If a file's bin allocation metadata
  is **already damaged**, a write can publish a new bin over another structure. Here
  "damaged" means the cursor, or the chain word it adopts, points outside bin storage.
  The overwritten structure can include zero-valued record data. This applies to any
  write, including one that reconciles a marker.

  The allocator makes cheap checks only:
  - it refuses a "next empty" bin that holds a non-zero word;
  - it refuses a bin that overlaps the permanent section as `zpermCreate()` lays it out
    (header, lock word and arrays, hash table, first reclaim table; lines 95, 272-325).

  It makes no whole-file ownership check: a map costs O(records) per write, 0.2 s on a
  390 MB file and 1.4 s on a 2.9 GB file. An intact heclib or native file always has a
  valid cursor; the corpus check confirms this for every HEC- and engine-written file.

  heclib's own allocator validates **less**. `zbinNew()` hands out `kaddNextEmptyBin`
  without reading it or checking where it lies (lines 129-135) and clears the bin in
  memory (lines 136-139). `zbinUpdate()` then writes the whole bin there (line 271). So
  heclib writes over whatever the damaged cursor names, non-zero data included.

  To recover such a file, copy it out with `repair_dss7(src, dest)`.
- **Known limitation R12.** The checks above cannot tell that an
  unrelated record's **damaged** pointer names a location that reconciliation
  legitimately writes. Two examples:
  - **A**: the pending pathname's hash slot was changed to name zero-filled words inside
    another record's values. This one is **now refused** by the roll-forward link-target
    check, and nothing is written.
  - **B**: another record's values2 pointer was changed to name the cursor bin, which
    holds this write's authentic, exactly matching leftover. Reconciliation clears its
    leftover, and that record's values (which were the leftover's words) then read as
    zeros. Detecting B needs knowledge of every record's extents, that is, the
    whole-file map that the in-place path does not build.

  Both need a pointer damaged independently of the interrupted write. A test pins B's
  behaviour (`r12_limitation_shared_leftover_is_pinned`), and copy-out of the B file
  keeps the record exactly as the damaged file shows it.
- **Result.** The repaired file is what an uninterrupted write produces: the record in
  the bin the killed write had taken, listed by native and HEC catalogs, nothing hidden.
  The record of the killed attempt is never visible (its info block stays unreferenced,
  status 10), as for any write interrupted before Publish.

**Files damaged by engines up to 0.1.0.** Such a file has a *hole*: a handed-out bin
(before `kaddNextEmptyBin`) whose first word is zero while its other words hold one
unpublished entry. The hole is followed by bins that lookups reach but the catalog
walk does not list. The checked catalog reports them: links outside the walked bins,
and the `knumberRecords` mismatch. Native writes leave the hole as it is.

**Copy-out repair (`dss7::write::repair_dss7(src, dest)`, Python `repair`).**
`repair_dss7` writes nothing into `src`; it opens `src` read-only.

1. **Find the records: discovery.** One procedure,
   `Dss7File::discover`, defines what copy-out considers:
   - **roots:** every bin of every block of the bin-block chain (the catalog walk) and
     every non-zero hash-table slot (where `lookup()` starts);
   - **bins:** each bin reached is parsed exactly as `lookup()` parses it, and every
     entry is recorded;
   - **overflow:** the overflow word of **every** bin reached is followed, **also from
     an empty bin** (first word zero), as `lookup()` does. The recovery catalog did not,
     which hid records behind an empty bin;
   - **bounds:** each bin is visited once, at most `kfileSize / kbinSize + 1` bins;
   - **records:** each entry's info block is read and checked as by `lookup()` (flag;
     the hash rule for non-aliases); an alias's chain is resolved to its primary.

   It reaches what `lookup()` reaches and the bins of every **whole** bin block of the
   chain. It does **not** reach everything the catalog lists: when a bin block's declared
   span (`kbinsPerBlock` bins plus its chain word) does not fit inside the data area,
   discovery lists that block in `not_copied` as unclassified and stops following the
   bin-block chain there, whereas the catalog walk still reads the whole bins of such a
   partial block (`dss7/walk.rs`). Records reachable only through those bins (and not
   from a hash-table slot) are then not copied, and `complete` is false. A pointer target that the procedure cannot classify is
   listed in `not_copied` as unclassified, never dropped: a link outside the data area,
   a bin block that does not fit, an entry that does not parse after a non-zero hash
   word, or reaching the bound. **Out of scope** are bytes
   that no pointer reaches, for example an info block that no entry names (such as a
   replaced record's old copy), or entries after a zero hash word in a bin, where
   `lookup()` stops.
   - **Choosing a copy.** For each pathname (case-insensitive) it tries the
     record a lookup returns, then every catalog copy of the pathname. It copies the
     first whose areas read in full. `fallback_copies` lists a copy only when the
     preferred one was not usable (the lookup failed, or an earlier candidate did not
     read), with the source info block copied and why. Errors of alternatives that were
     never needed do not make a copy a fallback.
   - **Identity.** A record is written, and verified, under its own
     identity: the pathname its info block stores. A change of identity is accepted only
     for an alias resolving to its primary, or for a pure ASCII case difference;
     `variants_copied` records either. A primary whose info block names another record
     (or cannot be read) is not used. Another copy of the pathname is tried; if none
     qualifies, the pathname is listed in `not_copied`.
   - **Duplicates.** A further copy of a pathname, an alias, or a destination
     collision is skipped only when it is the record copied, or its payload is equal.
     A readable duplicate with a different payload is listed in `not_copied` with a
     reason. `complete` is therefore true only when every readable record discovery found
     was copied and verified, or is the same as one that was.
   - **Aliases.** An alias entry (status 2) counts as covered only when its
     primary record was copied. Otherwise the primary is copied through the alias, under
     the primary's own pathname, and reported in `fallback_copies`. If that copy is not
     possible, the alias is listed in `not_copied`.
2. **Copy them.** It copies each record's six data areas, record type, dates, value
   count and logical number, unchanged, into a new file with the normal writer's record
   transaction, in one session. The new info blocks get a new write time, creation
   time, program name and version count. An empty internal header keeps address 0, as in the source:
   heclib's `zcheckInfo()` reads 18 ints at that address for every type-1xx record,
   whatever its length.

   Time series in a source older than 7-HA are not copied. They use the deprecated
   aggregate layout, which the reader interprets by the file's revision, so in a new
   7-JA file they would mean something else. They are reported as not copied, with
   that reason.
3. **Verify.** It reads the new file back. Every copied record must match the source
   twice: bit-for-bit equal raw areas, and an identical decoding by the normal readers
   (time series, profile, paired data, grid, location; the write time and program
   name are excluded). The source side of the decoding is the very source record that
   was copied. A record that neither file's reader decodes, or of a kind without a
   decoder, is compared by its raw areas alone. The file must also catalog without
   anomalies. Otherwise nothing is published.
4. **Publish.** The new file is built under a temporary name next to `dest`
   (`.<name>.<pid>.<nanos>.partial`) and only then published at `dest`, never replacing
   anything:
   - first by a hard link, which fails if `dest` exists;
   - if hard links are not available, by an atomic no-replace rename: Linux
     `renameat2(RENAME_NOREPLACE)`;
   - if neither exists (for example Windows on a filesystem without hard links;
     `MoveFileExW` would need `unsafe` code, which `dss-core` forbids), it fails with an
     error that names the verified temporary copy, which it keeps.

   `dest` must not exist (`WriteError::AlreadyExists`, Python `FileExistsError`). There
   is no check-then-rename fallback.

Every returned error removes the temporary file, except the "cannot publish
without replacing" error above. A process killed before publication leaves `src`
byte-identical, no `dest`, and the temporary file. A process killed just after the hard
link leaves the verified `dest` and the temporary name, which can be deleted.

**The ledger.** Every source candidate gets exactly one ledger entry
with one outcome. A candidate is each distinct info block that a pathname's lookup or
catalog entries reach, each alias entry, or a pathname refused as a whole. The outcomes
are:
- `Copied`;
- `EqualToCopied` (its payload equals the copy);
- `UnreadableCoveredBy` (it does not read, and the pathname was copied from another
  candidate);
- `NotCopied` with a reason.

Every candidate is read. A readable one that is not copied, rejected for any reason
(identity included), is compared with the copy, and is `NotCopied` unless equal. An
alias's copy covers a primary's unresolved candidates only when they are unreadable or
equal to it. `not_copied` is derived from the ledger, and `complete` is true exactly
when no entry is `NotCopied`.

**A partial recovery is published.** A successful return means the
selected records were copied and verified and `dest` was published. It does not mean
everything found was copied. When `not_copied` is not empty, `complete` is false and
`dest` is still published. Callers must check `complete` and `not_copied`.

The report also gives the hidden records among those copied (records the source's
catalog did not list), the fallback copies and variants, and the aliases and duplicates
skipped. It says whether the source carried an interrupted-write marker; the source is
not reconciled. Whether hidden records come back is reported, not guaranteed. A
complete copy is not an integrity certificate for `src`.

**Deletes.** The irregular writer deletes blocks that its time window empties
(heclib storage flag 1, `ztsStoreIrreg7()` lines 411-415). A delete is one transaction
in the same step order: Allocation (the marker holds **minus** the deleted record's info
address, and the header already carries `knumberDeletes` +1, `kcatSortDeletes` +1,
`kcatSortStatus` 3 and `kdead`), Retire (info status DELETED, 11; readers do not test it,
so the record still reads), Publish (bin status DELETED: the record is gone), Counters
(`knumberRecords` −1, area counts; `zdeleteInternal()` lines 157-231), Finish. Reconcile
of a negative marker rolls back a delete whose bin entry is still live (info primary,
Allocation counters taken back) and otherwise keeps it. Interrupted after Publish and before Counters,
`knumberRecords` stays one high (`zcheckFile` reports it, also after an HEC write) until
the next native write rebuilds it.

**Paired data.** A paired-data store (`Public/zpdStore.c:zpdStore()`, full record)
is one record transaction in the same step order. Its areas: internal header ints 0-4
(`numberOrdinates`, `numberCurves`, `boolIndependentIsXaxis`, `labelsLength`, precision
`10x+y` or −1), int 5 four blanks, then the X/Y units and types and the time zone packed by
`zpdUnitsToHead()` (each string a whole number of 8-byte words; at most 392 bytes fit,
because heclib stops at 398 and then stores none); header2 the NUL-terminated labels; values1 the ordinates; values2 the curves,
curve-major; `kinfoNumberData` = `kinfoLogicalNumber` = `(numberCurves+1)·numberOrdinates`;
allocated size `2·(longs(values1) + longs(values2))`. heclib updates `kmaxValues2Size`,
`knumberDataArea2` and `kmaxPdSize` for both PD and PDD (`zwriteInternal()` lines
308-314). A PD ↔ PDD change is a delete and a new record in heclib (`zpdStore()` lines
904-911); the native writer replaces the record in one relocating transaction. heclib's
in-place shrink leaves the old bytes in the unused half of the last values1/values2 word
(readers ignore them). Writer: `crates/dss-core/src/dss7/write/engine/pd.rs`.

**Reader needs:** §6.3. **Writer only:** §6.1, §6.2 and §6.4.

---

## 7. Time series layout (enough to plan the reader)

### 7.1 Dates and times

- Julian day 1 = 1 January 1900; day 0 = 31 December 1899. Sources: `JULIAN_01JAN1970` =
  25568 and `JULIAN_BASE_DATE` 693960 in `headers/heclibDate.h`.
  **[VERIFIED-CORPUS]** bin date 43350 = 08 Sep 2018, matching the D part.
- Info first/last stamps are `(julian, secondsOfDay)` (§5.1). Bin dates are days only.
- Time granularity is per record (internal header word 0: `SECOND_GRANULARITY` 1,
  `MINUTE_` 60, `HOUR_` 3600, `DAY_` 86400; `headers/hecdssInternal.h` lines 427–430).
  **[VERIFIED-CORPUS]** Regular times use the record's granularity `g`
  (internal header word 0): `times[i] = (julian − base)·(86400/g) + secs/g`
  (`ztsRegAddTimes`, integer arithmetic). Irregular stored times are seconds from the block
  start (minutes for IR-Century), divided by the granularity with C truncation. Absolute
  seconds equal hecdss's `julianBaseDate*86400 + times*granularity` (432/432 dataset
  requests bit-identical).

### 7.2 Regular-interval blocks (`Internal/ztsRegGetBlockStart.c:ztsRegGetBlockStart()`)

| Interval | Block | Block start (on-disk D part) |
|---|---|---|
| < 15 min (1s … 12Minute) | 1 day (`BLOCK_1_DAY` = 1) | that day |
| 15Minute … 12Hour (< 1 day) | 1 month (2) | 1st of the month |
| 1Day … < 1Week | 1 year (3) | 01 Jan |
| 1Week, Tri-Month, Semi-Month, 1Month (< 1Year) | 1 decade (4) | 01 Jan of year − year mod 10 |
| 1Year | 1 century (5) | 01 Jan of year − year mod 100 |

- The on-disk pathname for each block has D = the block start date, formatted `ddMmmyyyy`
  (`08Sep2018`), and E = the DSS7 interval name (`1Minute`, `1Hour`, `1Day`, …). The
  standard names and seconds are in the `eParts7` / `secondsInInterval7` tables in
  `headers/standardIntervals.h` and in `SemiPublic/ztsGetStandardInterval.c`.
- DSS6 names (`1MIN`, `1MON`) are recognized on input and mapped to DSS7 names. Whether
  older DSS7 writers stored DSS6-style E parts, and how month case appears in D, are
  **[UNVERIFIED]**; a corpus catalog run must check what it finds.
- Pseudo-regular (`~1Day`) block sizes: **[UNVERIFIED]**.
- Logical values per block = number of intervals in the block. Example: a 1-minute daily
  block has `kinfoLogicalNumber` = 1440 **[VERIFIED-CORPUS]**.
- **E-part matching** **[VERIFIED-CORPUS]**: `ztsGetStandardInterval` takes
  the first table entry whose first `min(len(name), len(E))` characters equal the E part,
  case-insensitively. A `~` prefix means pseudo-regular.
- **Window semantics** **[VERIFIED-CORPUS]**: HEC moves both ends of an
  explicit regular window forward to the end of their standard interval (a window ending
  05:30 on hourly data includes the 06:00 value). A single-block read covers the whole block
  (HEC passes the interval as the start's seconds to `numberPeriods`). A whole-dataset read
  (no dates) uses the bin dates (`kbinDates`): it runs from the first interval of the day of
  the first block's first valid value to 24:00 of the day of the last block's last valid value,
  and keeps missing values inside those days. Weekly data uses decade blocks, with a special
  case when `blockStartPosition == -1`. Blocks that do not exist read as −FLT_MAX with
  quality 0. The reader matched hecdss on 432/432 requests (whole, trimmed, mid-block and
  multi-block windows).

### 7.3 Irregular-interval blocks (`Internal/ztsIrregGetBlockStart.c`)

| E part | Block | Block-size code |
|---|---|---|
| IR-Day | day | 1 |
| IR-Month | month | 2 |
| IR-Year | year | 3 |
| IR-Decade | decade | 4 |
| IR-Century | century | 5 |

Irregular codes are stored as negative "seconds" (−1..−5) in `secondsInInterval7`.
values1 begins with a times array (int32 offsets from the block start, in granularity
units; seconds, or minutes for IR-Century, divided by the granularity with C truncation,
§7.1 **[VERIFIED-CORPUS]**; IR-Century minute times themselves have no
corpus data).

**Irregular writes** **[VERIFIED-ORACLE, heclib 7-JA C writer]**: times are
stored as seconds from the block start (minutes for IR-Century) whatever the
granularity; internal-header word 0 holds the granularity, words 2-5 are 0. A time equal
to the block length (24:00 of the block's last day) belongs to the block. values1 is
`[times][values][pad][quality][pad]`; times are never compressed and are not counted in
the compression ratios. `kinfoNumberData` is the number of values stored and
`kinfoLogicalNumber` is heclib's estimate `days × 86400 / (span / n)` with
`days = next block − block start + 1` (minutes for blocks of 370 days or more;
`ztsIrregStoreBlock()` lines 717-743). Missing values are stored; irregular blocks are
never trimmed. With storage flag 1, heclib's merge drops stored values after the window
when more stored values precede it than it replaces (`ztsIrregMergeBlocks()` lines 488
and 552; found by comparing the native irregular writer with heclib's). heclib's calendar for years 1-4 (`yearMonthDayToJulian()`, one
leap day counted before year 5) maps dates to julian numbers one day apart from the
proleptic Gregorian calendar this engine uses; dates and block-relative times are the
same, only julian numbers differ.

**Irregular block reads** **[VERIFIED-CORPUS]**: HEC filters values with
`isTimeInRange`, whose status is **sticky**: the start check applies only until the first
in-range value. HEC sizes each block read by the total `kinfoNumberData` over the whole
window; a reader can cap it at the block's own count, with the same result and without the
quadratic allocation. Irregular windows are inclusive at both ends.

### 7.4 TS internal header (int32 indices; `headers/zStructAllocation.h` lines 87–104; `Internal/ztsInternalHeaderPack.c`, `Internal/ztsInternalHeaderUnpack.c`)

| Idx | Name | Meaning |
|---|---|---|
| 0 | `timeGranularity` | Seconds per time unit |
| 1 | `precision` | Decimal places (−1 = unset) |
| 2 | `timeOffset` | Seconds offset within the interval |
| 3 | `profileDepthsNumber` | 0 if not a profile |
| 4 | `blockStartPosition` | First stored position in the block |
| 5 | `blockEndPosition` | Last stored position |
| 6 | `valuesNumber` | int32 count of the values area |
| 7 | `valueSize` | int32 per value (1 float, 2 double) |
| 8 | `valueElementSize` | — |
| 9 | `valuesCompressionFlag` | 0 none; 1 repeat-bit compression (bits in header2); 2 all repeats |
| 10–12 | `qualityNumber`, `qualityElementSize`, `qualityCompressionFlag` | Quality array (0 = none) |
| 13–15 | `inotesNumber`, `inotesElementSize`, `inotesCompressionFlag` | Integer notes |
| 16 | `cnotesLength` | Character notes bytes |
| 17 | `units` | Current writers blank-fill int 17 and start the strings at **int 18**. Old writers (for example `7-10`) start the strings at int 17. **[VERIFIED-CORPUS]** `7-10` has `"CFS\0INST-VAL\0"` at int 17. **Rule (`ztsInternalHeaderUnpack` + `unitsHavePadding`):** if int 17 is 0 or four blanks, the strings start at int 18; otherwise at int 17. HEC copies at most 200 bytes (and at most 200 ints of header). **Quirk mirrored by readers:** if the first string (units) is empty, the type and time-zone strings are also left unread. 7-GT writes padded units; 7-10 does not. |
| 18… | strings | NUL-separated: units (or depth units + value units for profiles), type, time zone name |

### 7.5 Values (values1) layout (`Internal/ztsAggregate.c`, `Internal/ztsDisaggregate.c`)

values1 is a single int32 stream:

1. `[times]` (irregular only);
2. `[values]`, padded to an even int32 count;
3. `[quality]`, padded;
4. `[inotes or cnotes]`.

Compression (when more than 12 values, not a profile, and more than 5% repeats) keeps the
non-repeated values and a repeat bitmask in header2. **[VERIFIED-CORPUS; §10 item 12 closed]**

- **Repeat bits:** header2 holds one repeat bit per stored position, bit `i%32` of word
  `i/32`, where 1 means "repeat the previous value". The quality bits follow the values bits
  only when the values compression flag is 1; the notes bits follow both.
- **Flag 2** (`valuesCompressionFlag` 2) stores a single value; the whole block repeats it.
- **values1 layout:** `[times][values][pad to even][quality][pad][notes or cnotes]`.
- **Deprecated layout** (`ztsAggregateDep` / `ztsDisaggregateDep`): used when `fver < 70801`,
  that is **7-10 and 7-GT**; it has no padding. `ztsDisaggregate` makes this choice by `fver`
  too, not only `ztsAggregate`.
- Writer thresholds (HEC): more than 12 values, not a profile, more than 5% repeats for
  values and more than 10% for quality and notes; flag 2 only when quality and notes are
  absent. A writer should use the aggregate layout that matches the file's `fver`.
- Verified against hecdss and Monolith on 7-10, 7-GT, 7-HL and 7-IC fixtures (409,001
  of 416,845 TS records bit-identical to hecdss; every other record is a pattern whose only
  difference is time, or fails in both oracles, or is the crashing profile).
- Float vs double comes from the record type (100/110 float; 105/115 double) and from
  `valueSize`.

### 7.5b Writing notes, multi-word quality and patterns

`ztsAggregate()` places values1 as: times (irregular), values, pad to even, quality, pad, integer notes, pad, character notes, pad. Each integer array compresses on its own repeat ratio (values above 5%, quality and notes above 10%), and header2 holds one bit array per compressed array, in that order. Character notes are not compressed: `INT_HEAD_inotesNumber` is `(cnotesLength - 1) / 4 + 1`, and the bytes are stored as given with the last int zero-filled.

A pattern (`ztsStorePattern()`) stores no quality, notes or time offset, and leaves internal-header words 3 to 16 at 0. Its first and last dates are never set, so in the tested sequence the info block's dates depend on what the HEC handle wrote before; the native writer stores 0.

### 7.5a Pattern records (101/106/111/116)

**[VERIFIED-ORACLE]** A D part starting `TS-P` (`Internal/ztsProcessTimes.c`
lines 268-289) marks pattern data. No dates are stored.

- **Writer.** `SemiPublic/ztsStorePattern.c` lines 137-140 stores the caller's
  `timeOffsetSeconds` in `INT_HEAD_timeOffset` (§7.4 int 2). Neither hecdss 0.1.33
  (`hec_dss_tsStoreRegular`) nor Monolith 3.3.27 (`Heclib.Hec_ztsStore`, `javaHeclib`
  `Hec_ztsStore.c`) sets it, and `ztsProcessTimes()` returns for a pattern before it derives
  an offset from the start time. HEC software therefore writes 0. No corpus record stores a
  nonzero DSS7 pattern offset (0 of 5,918).
- **values1.** Regular patterns: `numberData` raw floats (101) or doubles (106). Irregular
  patterns: `numberData` int32 times, then the values (`Public/ztsRetrievePattern.c` lines
  300-357).
- **Irregular pattern times [VERIFIED-HEC].** The stored times count the internal
  header's granularity from julian 0 (`ztsRetrievePattern.c` line 330 copies them into
  `tss->times`, line 365 sets `julianBaseDate = 0`). A stored 30 at minute granularity is
  1800 s for HEC's C library and Monolith 3.3.27; dss-core reports seconds (raw x
  granularity).
- **heclib C start.** `ztsRetrievePattern()` lines 360-372 sets the start to 01Jan3000 00:00
  plus one interval (`incrementTime(+1)`), then, when the stored offset is nonzero, moves it to
  that offset within the interval (`Public/ztsOffsetAdjustToOffset.c` lines 93-120). So a
  15Minute pattern starts at 00:15 with offset 0 and at 00:03 with offset 180 s. pydsstools
  reports this start; Monolith reports it as `startTime`. Monolith's `times[0]` is 00:00.
  hecdss subtracts one interval from it (`_get_timeseries`).
- **Reader convention (dss-core).** The first regular value is at 01Jan3000 00:00, then one
  value per interval. This matches Monolith's time array. The stored offset is **not
  applied**; the DSS6 reader does the same (dss6.md §7.3a). The applied offset
  (`time_offset_seconds`) is 0. `Dss7Ts::pattern_origin()` reports the raw
  `INT_HEAD_timeOffset` (`None` when the internal header has fewer than 3 ints) and the
  heclib C start (`None` for irregular patterns). With no offset word, the start uses 0.
- **Short-header quirk.** `Internal/ztsInternalHeaderUnpack.c` line 76 rejects only headers of
  fewer than 2 ints (`< INT_HEAD_timeOffset`). So a 2-int header makes heclib read
  `internalHeader[2]` one int past the stored header.
  - On a builder-written 2-int header, pydsstools and Monolith reported a start of
    3003-08-06 09:59, which is garbage.
  - dss-core reports no stored offset there and uses 0, giving 00:15 for 15Minute.
  - The 1-int and 3-int headers, a −180 s offset (2999-12-31 23:57) and a 3600 s offset on
    15Minute (01:00, applied after `cleanTime()`) all match pydsstools and Monolith
    `startTime`.
- **Check.** `fixtures/public/pattern_offset7.dss` holds builder-written offsets of 180, 1800
  and 0 s. pydsstools 3.1.0, Monolith 3.3.27 `startTime` and hecdss 0.1.33 all report starts
  of 00:03, 00:30 and 00:15, equal to `pattern_heclib_first_seconds`. Monolith `times[0]` is
  00:00 for all three, equal to the native first value.

### 7.6 Missing values (`Utilities/zmissingFlag.c`; `headers/heclib.h` lines 21–22)

- Current heclib **writes** −FLT_MAX (`UNDEFINED_FLOAT`). For doubles it writes
  `-(double)FLT_MAX`, not −DBL_MAX.
- Readers must treat **−901.0, −902.0 and −FLT_MAX** as missing (`lismissingf_`,
  `lismissingd_`).
- Blocks that do not exist read as missing. Quality and notes read as 0.
- This **corrects SPEC.md §7**, which lists only −901 and −902. It also means the
  NaN→sentinel rule for writers must pick a constant (HEC currently writes −FLT_MAX).

### 7.7 Quality and notes

- Quality and notes exist when `qualityElementSize` / `inotesElementSize` > 0 or
  `cnotesLength` > 0.
- values2 holds profile depths. values3 is used by other types.

**Reader needs:** §7.1–§7.6 for TS read. **Writer only:** block splitting (inverse
of §7.2), compression decisions, and `kdataFirstDate` / `kbinDates` bookkeeping
(`Internal/ztsRegStoreBlock.c` line 813).

---

## 8. Pathname text rules

| Rule | Value | Source |
|---|---|---|
| Max pathname | 393 bytes (`MAX_PATHNAME_LENGTH`; buffer 394); lookup truncates at 392 | `headers/zdssVals.h` lines 50–51; `zcheckInternal()` line 125 |
| Max part | 128 bytes (`MAX_PART_SIZE` 129 including NUL) | `headers/zdssVals.h` line 52 |
| Case | Stored as written (mixed case). Hashing and comparison are case-insensitive (C `toupper`). | `zhash()`, `Public/zpathnameCompare.c` |
| Encoding | Bytes. No charset conversion. Bytes < 32 become `'?'` **for hashing only**. With signed `char` (MSVC/x86) bytes ≥ 0x80 also hash as `'?'` (§3.1); unsigned-`char` platforms differ. No corpus pathname is non-ASCII. | `zhash()` lines 91–101 |
| Form | `/A/B/C/D/E/F/`, 7 slashes (`zpathnamePartPositions` requires 7) | `Internal/zcatSortPath.c` |
| Storage | `ceil(n/8)` words, NUL-padded, in both the bin and the info block | `zbinUpdate()` line 261; `zwriteNew()` |

**Reader needs:** decode as bytes. Use lossless Latin-1 or bytes in the API, and compare
case-insensitively in ASCII only.

---

## 9. Reader checklist

1. Read 800 bytes. Require `ZDSS`, a version starting with `7`, word 99 = −97531, and
   word 89 = 0.
2. Use the header size (0 ⇒ 100). Validate `kbinSize` ∈ [20, 1000], `kbinsPerBlock` ∈ [1,
   1024], `kmaxHash` ∈ [1, 1,000,000] (§2.3), and `kfileSize × 8 ≤ len`. Every address
   must satisfy `lock_area_end ≤ a < kfileSize`. `lock_area_end` covers words 100–102
   plus any write-lock, read-lock and PID arrays that lie before the hash table. This is
   stricter than `100 ≤ a` and also enforces §6.3 (never read the lock area).
3. Catalog: scan the blocks (§4.1).
   - Per entry: decode `+2` and require `0 < chars ≤ 393` and `words == ceil(chars/8)`.
     `icount + 7 + words` must stay inside the bin.
   - Keep entries with status 0, 1 or 2.
   - Bound the number of blocks by `kfileSize / blockWords`.
4. Optionally verify each live entry's info: flag, status 0/1, hash, pathname bytes.
5. Point lookup: §3.7. Bound the overflow chain length by `ktotalBins + 1` and alias
   hops by `knumberAliases + 1`.
6. Never read the lock area, never lock, never write.

---

## 10. Open items for oracle and corpus verification

| # | Item | Concrete check |
|---|---|---|
| 1 | Non-ASCII bytes in pathnames. **Resolved from source:** with signed `char`, bytes ≥ 0x80 test `< 32` and hash as `'?'`; there is no sign extension, `toupper` never sees a negative value, and `tableHash` is never negative (§3.1). Still open: files written on unsigned-`char` platforms. | No DSS7 corpus pathname has a byte ≥ 0x80 (2,846 private corpus files). If one appears, compare the stored `kbinHash` and table-chain membership with the `'?'` rule and an unsigned variant. |
| 2 | Collections: do any writers substitute the `C:` sequence before table hashing? | For files with `knumberCollections` > 0, verify that each collection path is found in the table chain for its unmodified hash. |
| 3 | `kfileHeaderSize` = 0 (seen in `7-10`) | Header survey: tabulate word 1 by version string. Confirm that word 99 = −97531 everywhere. |
| 4 | `7-10` writer quirks: info status 0, packed int32 sort list. (`kinfoLastWriteTime` in seconds-like units, read by HEC as ms: verified on the corpus, §5.1.) | Read every `7-1x` and other digit-version file in the corpus read-only. Compare with `hecdss` catalog and last-write output. |
| 5 | `kendian`: `zopenInternal()` line 408 writes `ifltab[zdssFileKeys.kendian]` (the ifltab array, not the header). This is an HEC bug, so the header word may never be set. | Survey word 94 across the corpus. Expect 0 everywhere. |
| 6 | Word `kbinSize−2` of each bin unused; end-of-bin rule | Corpus: assert word `binSize−2` is 0 in every bin. Assert that entries never cross `binSize−1`. |
| 7 | An empty bin implies the rest of the block is empty, and only the last block is partial | Corpus: after the first empty bin in a block, all later bins in that block are zero. That block contains `kaddNextEmptyBin`. |
| 8 | Catalog set and order vs the oracle (location records type 20; aliases; status-0 undeleted). **Mostly closed:** live pathname multisets equal hecdss on 25/25 fixtures and Monolith on 26/26; unsorted order equals bin order on 25/25 (435,593 entries); type 20 is listed by both oracles; status 0 is listed by both (§3.6). Still open: aliases (none in the corpus) and the sorted catalog. | Diff the sorted catalog with `hecdss`; test alias reads on a file that has aliases (§3.7 step 6). |
| 9 | Deleted records whose space was reused (status 15; info hash mismatch) | Find files with `knumberDeletes` > 0 and `kreclaimTotal` usage. Check the info hash for each status 11 entry. |
| 10 | Windows `_locking` vs our reads (positioned reads and mmap) while HEC holds the file | With HEC-DSSVue holding a test copy open, read the header, bins and data with the Rust reader on Windows. Confirm there are no `ERROR_LOCK_VIOLATION` errors outside words 100–177. |
| 11 | Old locking scheme (`klockReadArrayAddress` = 0) | Confirm with HEC software that a test lock on words 100–102 detects an active writer. Document that passive readers cannot be detected. |
| 12 | **CLOSED.** TS values1 / header2 compression bit layout; deprecated (< 7-HA) layout | Layout and repeat bits documented in §7.5; 409,001 of 416,845 TS records bit-identical to hecdss (§7.5) |
| 13 | **Verified.** Irregular times: base and units vs granularity | Seconds from block start (minutes for IR-Century) divided by granularity (§7.1, §7.3). IR-Century minute times: no corpus data |
| 14 | **CLOSED.** `unitsHavePadding` detection rule | Int 17 is 0 or four blanks: strings start at int 18; otherwise at int 17 (§7.4) |
| 15 | Non-power-of-two `kmaxHash` (16348, 64000) | Find corpus files with these sizes. Verify §3.2. |
| 16 | E/D part spellings written by older DSS7 revisions; pseudo-regular blocks | Tabulate E parts and D formats by version string from a corpus catalog run. |
| 17 | Lock policy for passive read markers (§6.2) | Open decision. Test with HEC-RAS, HMS and DSSVue holding the file read-only and read/write. |


## 11. Location records (type 20) and associated locations

A maintainer decision (2026-10-27) extends the catalog: location records are read, not only catalogued.
Reader: `crates/dss-core/src/dss7/location.rs`; shared model
`crates/dss-core/src/location.rs`.

### 11.1 Pathname (`Public/zlocationPath.c:zlocationPath()`) **[VERIFIED-CORPUS]**

A location belongs to a point shared by every dataset with the same A and B parts. Its
pathname is the input pathname up to the start of the C part (`zpathnamePartPositions()`
position 2, after the third `/`) followed by `Location Info////`:
`/A/B/Location Info////`. `zlocationRetrieve()` and `zlocationStore()` accept *any*
pathname and map it this way; the data record itself is not consulted. `ztsRetrieve()`
(lines 419–421) and `zpdRetrieve()` (lines 704–706) call `zlocationRetrieve()` with the
data pathname when the caller asks for location data, so the "associated location" of a
time-series or paired-data record is exactly that lookup. Lookups are case-insensitive
(§3.7). Both functions return `INCOMPATIBLE_VERSION` for a DSS6 file.

### 11.2 Record layout (`Public/zlocationStore.c:zlocationStore()`, read by `Public/zlocationRetrieve.c:zlocationRetrieve()`) **[VERIFIED-CORPUS + ORACLE]**

Written with `zwrite()` (`dataType = DATA_TYPE_LOCATION` = 20, no internal header, header2
or user header). Only values1 and values2 are used:

| Area | int32 index | Content |
|---|---|---|
| values1 | 0–5 | `xOrdinate`, `yOrdinate`, `zOrdinate`: three little-endian doubles (`convertDataType(..., 6, 6)`) |
| values1 | 6 | `coordinateSystem`: 0 none set, 1 lat/long, 2 State Plane FIPS, 3 State Plane ADS, 4 UTM, 5 local (other) |
| values1 | 7 | `coordinateID`: UTM zone, FIPS or ADS State Plane zone number |
| values1 | 8 | `horizontalUnits`: 0 unspecified, 1 feet, 2 meters, 3 decimal degrees, 4 degrees/minutes/seconds |
| values1 | 9 | `horizontalDatum`: 0 unset, 1 NAD83, 2 NAD27, 3 WGS84, 4 WGS72, 5 local (other) |
| values1 | 10 | `verticalUnits`: 0 unspecified, 1 feet, 2 meters |
| values1 | 11 | `verticalDatum`: 0 unset, 1 NAVD88, 2 NGVD29, 3 other/local (`IVERTICAL_DATUM_*`, `headers/verticalDatum.h`; names `"UNSET"`, `"NAVD-88"`, `"NGVD-29"`, `"OTHER"`) |
| values1 | 12.. | location time-zone name: `numberIntsInBytes(strlen)` ints, only when the name is non-empty; `zlocationStore()` caps values1 at 30 ints, so names longer than 72 bytes are dropped |
| values2 | all | supplemental text, `numberIntsInBytes(strlen + 1)` ints (always NUL-terminated); absent (`values2Number = 0`) when the struct's pointer is NULL, one zero int when it is `""` |

Read rules (`zlocationRetrieve()`): `values1Number < 6` is `STATUS_NOT_OKAY`; the six
codes are read only when `values1Number >= 12` (otherwise the struct keeps 0); the time
zone is read when `values1Number > 12` from int 12, and the supplemental text when
`values2Number > 0`; both are C strings (`charInt(..., zeroEnd = 1)`, end at the first
NUL). Codes are stored as given: `zlocationStore()` does not validate them (the public
fixture `synthetic_location7.dss` stores vertical datum 9 and HEC reads it back as 9). The `zlocationRetrieve()`
header comment lists horizontal units 5 "Local (other)", which `zStructLocation.h` does
not; the reader decodes 5 as unknown. **[CONFLICT, harmless: no corpus record uses it]**

Typed and textual datums are independent. Some corpus records have vertical datum 0
(unset) while their supplemental text names `VerticalDatum:NGVD29`; HEC only consults the
text when converting values to a requested datum (`ztsRetrieve()` lines 425–470, key
`verticalDatumInfo`). The reader keeps both and never maps 0 to NAVD88. Code 3 (`OTHER`) is typed but unnamed: the Python layer reports it with `vertical_datum_unknown=False` and `vertical_datum_named=False` (only 1 and 2 are named datums).

Supplemental convention (`zlocationRetrieve()` header comment): free text; segments may
be newline-separated, XML, or `Keyword:Item;` pairs (keyword before `:`, item ended by
`;`). HEC's own lookups (`Utilities/verticalDatum.c:extractFromDelimitedString()`) take
the first occurrence of `keyword:` and do not trim.

`zlocationStore()` also updates the file's location bounds (`zlocationUpdatePerm()`,
header words 84–88, §2.2); readers ignore them.

**Verification:** on the hecdss-written fixture `synthetic_location7.dss` and two private
DSS7 test files (8 records),
every field equals `hecdss` 0.1.33 (`hec_dss_locationRetrieve`), and the associated
location of every catalogued pathname agrees with hecdss (19,077 pathnames; 19,068
with a location) and Monolith's `HecTimeSeries.read` (19,043 time-series records;
19,042 with a location). Monolith re-serializes supplemental
text as `Keyword:Item;` pairs (it appended `;` to one fixture record); hecdss truncates
supplemental text to its 256-byte buffer.
### 11.x Writing

`zlocationStore()` layout, with `zlocationUpdatePerm()` updating header words 84-88 for
coordinate system 1 with horizontal units 3. The writer differs deliberately in one place: a time-zone name
whose length is a multiple of 4 gets one more zero int, because heclib stores no NUL and
`zlocationRetrieve()` then reads past values1. Writers: `crates/dss-core/src/dss7/write/location.rs` and
`crates/dss-core/src/dss7/write/engine/profile.rs`.

## 12. Time-series user header (scope note)

DSS7 time-series records also have a user header. It is an int32 area located by the
info-block keys `kinfoUserHeadAddress` / `kinfoUserHeadNumber` (`headers/zdssKeys.h`
lines 214-215; `dss7/info.rs` `USER_HEAD_ADDRESS` 17, `USER_HEAD_NUMBER` 18), and the
reader already keeps it (`dss7::ts::TsMeta::user_header`). HEC writes the vertical
datum information there as `verticalDatumInfo:<base64>;` (`Public/ztsStore.c`,
`Utilities/verticalDatum.c`). Other programs can write free text as in DSS6.

Monolith maps it the same way as DSS6 except for the conversion call:
`Utilities/verticalDatum.c:userHeaderToString` copies up to the first NUL and returns
NULL for empty text. Its trailing-blank loop never runs, because of operator precedence
(`(cp - start) > 1 == ' '`). The result becomes `supplementalInfo`
(`javaHeclib/src/Hec_ztsRetrieve.c`, "User header (supplemental)", `zgetVersion == 7`
branch). It is parsed into items by `DataContainer.getSupplementalInfo()` (dss6.md §14.3).

**Out of scope for now.** The Python layer returns `None` for every `user_header_*` key
on DSS7 reads. Exposing it needs two checks first:

- Which block a DSS7 dataset read reports. `Internal/ztsRetrieveReg7.c` lines 524-556
  re-allocate `tss->userHeader` for every block that has one, so HEC appears to keep the
  last such block. `dss7::ts` currently keeps the first block's header.
- How `verticalDatumInfo` text should be shown, given that HecDss wraps it into a
  `TimeSeriesContainerVertDatum` (`hec.heclib.dss.HecDss.get`).

Both are open items.
