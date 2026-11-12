# DSS6 file format

This spec comes from HEC's MIT-licensed `hec-dss` source, `master` branch,
commit `283185bd8610ed2c4c11a597c9c202678e965498`. The Fortran library version string
in that tree is `6-YO` (`heclib_f/src/zinit6.f:ZBKDAT` line 855). Paths below are
relative to `hec-dss/heclib/`. A citation looks like `file:ROUTINE` or `file:ROUTINE
lines`. Text marked **[UNVERIFIED]** is inferred and needs a check from the list in
§12. Text marked **[VERIFIED-CORPUS]** was confirmed against HEC Monolith, raw bytes, or
files of a private test corpus; those files are not part of this repository.

Reader/writer tags used in tables:

- **R**: a read-only reader needs this field.
- **W**: only a writer needs it (maintain it exactly; readers may ignore it).
- **I**: information only (statistics or HEC bookkeeping).

The source rules were checked read-only against 34 public local DSS6 files (HEC-DSSVue
`sample.dss`, the HEC-HMS 3.3–4.13 sample zips, and the RasExamples Muncie, BaldEagle,
and BaldEagleCrkMulti2D files), covering versions `6-JE` to `6-YN`.

---

## 1. Physical layout: words, records, addresses

### 1.1 Word and record size

| Rule | Value | Source |
|---|---|---|
| Machine word | 4 bytes (`INTEGER*4`). `NCPW = NCMW = 4` characters per word | `heclib_f/src/Support/bkdatw.f:BKDATW` line 36 (`DATA IWORD/4,4,8,0,0,32,4,1,1,2/`); `heclib_f/src/zinit6.f:ZINIT6` lines 238-244 |
| Logical record (buffer) | `NBSIZE = 127` words = 508 bytes | `heclib_f/src/headers/zdsskz.h` line 7 |
| Physical record on disk | 512 bytes. Record `r` (1-based) starts at byte `(r-1)*512` | `heclib_f/src/zrrec6.f:ZRREC6` lines 74-78; `heclib_f/src/zwrec6.f:ZWREC6` lines 93-110 |
| Bytes read per record | 508 (`readf(...,508,...)`) | `zrrec6.f:ZRREC6` lines 85-112 |
| Bytes written per record | 512. The last 4 bytes come from memory past the 127-word buffer and are **garbage**. | `zwrec6.f:ZWREC6` lines 107-118 |

So **each 512-byte record holds 127 data words followed by 4 pad bytes**. Readers
must skip those pad bytes. In the surveyed files they hold leftover memory: often
zero, sometimes `-9630` or other words.

### 1.2 Addresses

Every pointer in DSS6 (header, hash table, bins, info blocks) is a **1-based word
address** (`INTEGER*4`, signed).

| Conversion | Formula | Source |
|---|---|---|
| address → (record, word) | `rec = (addr-1)/127 + 1`, `word = addr - (rec-1)*127` (word is 1..127) | `heclib_f/src/zgetrw6.f:ZGETRW6` lines 10-11 |
| (record, word) → address | `addr = (rec-1)*127 + word` | `heclib_f/src/zgetad6.f:ZGETAD6` line 26 |
| address → byte offset | `((addr-1) div 127) * 512 + ((addr-1) mod 127) * 4` | derived from the two rows above and §1.1 |

A logical read of `n` words (`zgtrec6`) loops over records and copies words 1..127 of
each one. Any multi-word item (a pathname, a `REAL*8`, a data array) **can cross a
record boundary**, and the 4 pad bytes are not part of it
(`heclib_f/src/zgtrec6.f:ZGTREC6` lines 81-191). A reader must collect logical words
first and decode them afterwards. In particular, never reinterpret raw file bytes as
an `f64` array.

Size limits: addresses are signed 32-bit, so the most HEC allows is about 2^31 words
(≈8 GB). Writes and reads refuse record numbers above 16,777,200
(`zrrec6.f:ZRREC6` lines 49-72; `zwrec6.f:ZWREC6` lines 40-63; `L8GB` comment
`zinit6.f:ZBKDAT` lines 886-887, "8 GB for DSS Version 6-Q and later"). Use 64-bit
byte offsets.

### 1.3 File-size and end-of-file rules (verified)

- `KFSIZE` (header word 5) is the **next free word address**. The last used word is
  `KFSIZE-1` (`heclib_f/src/znwrit6.f:ZNWRIT6` lines 118, 154-186 advance it after
  each allocation).
- On the last record, `zptrec6` writes `JEOFFL = -9630` into the word at address
  `KFSIZE` and zeroes the rest of that record (`heclib_f/src/zptrec6.f:ZPTREC6` lines
  197-210; `zinit6.f:ZINIT6` lines 311-313).
  **Readers must not require this flag.** A corpus survey found the word at `KFSIZE` is 0
  in 242 of 1,851 DSS6 files, including 4 public HMS 4.3 samples (`castro`, `river_bend`,
  `tenk` 6-QF; `hrap` 6-QE). It is usually, not always, `-9630`.
- Physical file length = `ceil(KFSIZE/127) * 512` bytes. All 34 surveyed files match,
  except one `6-JE` file whose **final record is only 508 bytes** (older writers wrote
  508-byte records). Readers must accept a short final record. HEC treats EOF on the
  last record as acceptable (`zgtrec6.f:ZGTREC6` lines 165-177).
- `KFSIZE = -1357` on disk means the file was left in, or is currently in, "write lock"
  mode, so the size is unknown. HEC then opens the file **read-only** and estimates
  the size from the physical length (`heclib_f/src/zopen6Int.f:ZOPEN6INT` lines
  212-229; set in `heclib_f/src/zwlock6.f:ZWLOCK6` line 69 and `zopen6Int.f` lines
  561-577). Any other negative `KFSIZE` makes the open fail (`zopen6Int.f` lines
  269-276).

### 1.4 Byte order

- The canonical file byte order is **little-endian**. Big-endian hosts swap every
  4-byte word on read and write (`zrrec6.f:ZRREC6` lines 116-126; `zwrec6.f:ZWREC6`
  lines 69-79). The comment says, "For now, little endian is used for all files (PC
  compatiable) (thus always swapped on sun, mips)"
  (`zopen6Int.f:ZOPEN6INT` lines 61-67).
- **Detection:** after reading the header, if `KTABLE` (word 15) is `> 10000`, the file
  is the opposite endianness, and the word swap flag is toggled
  (`zopen6Int.f:ZOPEN6INT` lines 256-267). `KTABLE` is always 1 or 2, so a swapped
  value is 0x01000000 or 0x02000000.
- `REAL*8` values take two consecutive words. For files with version ≥ `6-O`
  (`KVERNO ≥ 67900`), HEC on big-endian hosts also swaps the two words, so that
  doubles are stored as little-endian 8-byte values (`zopen6Int.f:ZOPEN6INT` lines
  542-556; `heclib_f/src/zdswap6.f:ZDSWAP6`). In a little-endian file, the double is
  the 8 bytes of the two logical words in order: the low-order word comes first.
- All 9,657 DSS6 files of the private test corpus are little-endian. Big-endian
  support can be "detect and reject" (see §12, item O4).

### 1.5 Character packing

- Strings are packed into words byte by byte, in **file byte order**. Character `k`
  (1-based) of a string is byte `(k-1) mod 4` of word `(k-1)/4 + 1`.
  `PUTHOL`/`GETHOL` index the bytes of an equivalenced `CHARACTER*4`, and reverse them
  only on big-endian hosts (`IWORD(4) = -1`). So in a little-endian file the bytes
  appear in plain string order (`heclib_f/src/Support/gethol.f:GETHOL` lines 39-45;
  `Support/puthol.f:PUTHOL` lines 37-44; `zinit6.f:ZINIT6` lines 256-268).
- Whole-word string copies (`CH2HOL`/`HOL2CH`, used for pathnames) pad the last word
  from the Fortran `CHARACTER*400` buffer, which means **blanks**
  (`Support/ch2hol.f:CH2HOL`). Every string field has an explicit character count or
  fixed length, and readers must use it rather than the padding. Verified: pathname
  pad bytes are 0x20.
- Fixed-length fields are blank-padded ASCII. Dates are 7 characters (`DDMMMYY`, for
  example `28NOV05`) in 2 words, and the eighth byte is NUL in the surveyed files.
  Times are 8 characters (`HH:MM:SS`) in 2 words (`zinit6.f:ZINIT6` lines 324-330;
  `heclib_f/src/Support/when.f:WHEN`).
- Bit numbering (compression flags, §8): bit `i` (1-based) is bit `(i-1) mod 8` (LSB =
  0) of byte `(i-1)/8 + 1` (`Support/bits.f:BITS` lines 14-36; Fortran `BTEST`).

---

## 2. File structure overview

```
word addr 1 .. 69            permanent section (header), NPERM = 69 words
word addr 70 .. 69+KHASH     hash table (KTABLE = 1 only), one word per hash code
(next record boundary)       lock record (one 512-byte record; no live data from 6-K on, §4.4)
KAFBIN ..                    first pathname-bin block (KBNBLK bins x KBNSIZ words)
...                          info blocks + headers + data, appended
...                          more bin blocks, appended when needed (linked list)
KFSIZE                       -9630 end-of-file flag (usually; 0 in some files)
```

Sources: structure comment in `zinit6.f:ZINIT6` lines 17-56 and 102-145; creation
sequence in `zopen6Int.f:ZOPEN6INT` lines 371-517. Note that the comment in
`zinit6.f` line 23 ("224 words") is out of date: `NPERM` comes to 69 (§3).

Structure type (`KTABLE`, `zinit6.f` lines 73-76; `heclib_f/src/zfsize6.f:ZFSIZE6`
lines 189-193):

- **1, hash table (default).** A table of `KHASH` words follows the header. Entry
  `h` holds the address of the first bin for hash code `h`, or 0.
- **2, no table** ("stable" files, `zset6 'STABLE'`). `KHASH` bins are preallocated
  from `KAFBIN`, and hash `h` maps directly to bin address
  `KAFBIN + (h-1)*KBNSIZ` (`heclib_f/src/zcheck6.f:ZCHECK6` lines 219-223).

---

## 3. Permanent section (header)

The header is `NPERM = 69` words at word addresses 1..69 (bytes 0..275, all in the
first record). Offsets come from the pointer chain in `zinit6.f:ZINIT6` lines 446-572,
with `NDSS = NVERS = 1`, `NDATE = NTIME = NPASS = NTAG = NPROG = 2`, and `NMXPRT = 2`
(`zinit6.f` lines 286-339). The header is read in one go by
`heclib_f/src/zrdprm6.f:ZRDPRM6` line 51 and written by `zptrec6(..., NPERM, 1, ...)`
(`znwrit6.f` line 216). All offsets below were verified on the sample files.

| Word (0-based) | Byte | Words | Name | Meaning | Use |
|---|---|---|---|---|---|
| 0 | 0 | 1 | `KDSS` | ASCII `ZDSS` | R (signature) |
| 1 | 4 | 1 | `KNRECS` | number of live records | R (HEC stops cataloging at this count, §6) |
| 2 | 8 | 1 | `KSEQNO` | sequence number for default tags `T<n>` (`heclib_f/src/zgetag6.f:ZGETAG6`) | W |
| 3 | 12 | 1 | `KHSIZE` | hash size code 1..9 (§4.2) | I (R uses `KHASH`) |
| 4 | 16 | 1 | `KVERS` | version string, 4 characters, for example `6-YN` | R |
| 5 | 20 | 1 | `KFSIZE` | next free word address (§1.3); `-1357` means write-locked or unknown | R |
| 6 | 24 | 1 | `KDEAD` | dead space in words | I/W |
| 7 | 28 | 2 | `KCREAT` | creation date `DDMMMYY` | I |
| 9 | 36 | 2 | `KLWDAT` | last-written date | I |
| 11 | 44 | 2 | `KLWTIM` | last-written time `HH:MM:SS` | I |
| 13 | 52 | 1 | `KTAGBK` | address of the tag-hash block (starts with `JTAGFL = -8642`), or 0 | W (readers ignore) |
| 14 | 56 | 1 | `KHASH` | hash table size, also the maximum hash code | R |
| 15 | 60 | 1 | `KTABLE` | 1 = hash table, 2 = direct bins; `> 10000` means byte-swapped (§1.4) | R |
| 16 | 64 | 1 | `KBNBLK` | bins per block | R (catalog walk) |
| 17 | 68 | 1 | `KBNREM` | bins left in the current block | W |
| 18 | 72 | 1 | `KBNSIZ` | bin size in words (127 since `6-M`, `zfsize6.f` line 186) | R |
| 19 | 76 | 1 | `KAFBIN` | address of the first bin | R |
| 20 | 80 | 1 | `KANBIN` | address of the next available bin | W |
| 21 | 84 | 16 | `KTAGS` | record-tag scheme (pairs of part number and position, `zgetag6.f`) | W |
| 37 | 148 | 1 | `KBINS` | bins used | I |
| 38 | 152 | 1 | `KHUSED` | hash codes in use (equals the nonzero table entries, verified) | I |
| 39 | 156 | 1 | `KBOVER` | overflow bins | I |
| 40 | 160 | 1 | `KMAXPH` | maximum pathnames on one hash | I |
| 41 | 164 | 1 | `KMAXHC` | hash code with the most pathnames | I |
| 42 | 168 | 1 | `KCOMPI` | address of file-default compression information | W (0 in all samples) |
| 43 | 172 | 1 | `KCOMPN` | length of that information | W |
| 44 | 176 | 1 | `KITSIN` | irregular-TS block growth increment (values) | W |
| 45 | 180 | 5 | `KITSDA..KITSCE` | minimum ITS block sizes for day, month, year, decade, century | W |
| 50 | 200 | 2 | `KFPASS` | file password (encoded) | W (0 in all samples) |
| 52 | 208 | 2 | `KMXPRT` | maximum length of pathname parts A..F, **one byte each** (bytes 1..6; 7..8 unused) | I (catalog formatting; `heclib_f/src/zcat6.f:ZCAT6` lines 102-110) |
| 54 | 216 | 1 | `KCOLL` | 1 if the file holds collection records | I |
| 55 | 220 | 14 | `KRES` | reserved (zero in all samples). Words 67/68 hold the native writer's in-progress marker during a write and are 0 afterwards (§9.3) | W (marker) |

`KEY2` (IFLTAB only, not on disk) follows, so `NPERM = KEY2 - KPERM = 69`
(`zinit6.f` lines 566-572).

**Version handling.** `KVERNO = 60000 + 100*ichar(c3) + ichar(c4)`, for example
`6-YN` → 68978 (`zopen6Int.f:ZOPEN6INT` line 250). HEC accepts any file whose first
two characters match `6-` (`zopen6Int.f` line 254). Thresholds that appear in the
code:

| Threshold | Effect | Source |
|---|---|---|
| before `6-K` | the lock record could also hold file data ("pre 6-K file structure saved information on the same record used for locking") | `zgtrec6.f:ZGTREC6` lines 122-139; `heclib_f/src/zmultu6.f:ZMULTU6` lines 197-198 |
| `6-M` | all bins are 127 words | `zfsize6.f:ZFSIZE6` line 186 |
| `6-O` (67900) | words of a double are swapped on big-endian hosts | `zopen6Int.f` line 550 |
| `6-Q` | 8 GB files allowed | `zinit6.f:ZBKDAT` line 886 |
| `6-E` (third character < `E`) | record passwords are ignored | `heclib_f/src/zrdinf6.f:ZRDINF6` lines 65-70 |

The surveyed corpus has 25 files at `6-JE`/`6-JF` (before `6-K`) and about 60 more
before `6-M` (`6-KE`, `6-LD`).

**Reading the header (R).** Read record 1 → words 0..68. Check `ZDSS`, a version that
starts with `6-`, `KTABLE ∈ {1,2}` (else check for byte swap), `KFSIZE > 0` (or
`-1357`, which means read-only with an estimated size), `KBNSIZ ≥ 13`, `KHASH ≥ 1`,
and `1 ≤ KAFBIN < KFSIZE`. Also:

- `KBNSIZ ≤ 127`, because HEC's bin buffer is `IPNBIN(NBSIZE+4)`
  (`heclib_f/src/headers/zdssiz.h` line 8).
- `KBNBLK ≥ 1` when `KTABLE` is 1.
- A `KTABLE > 10000` whose byte swap is not 1 or 2 is corruption, not a byte-swapped
  file: HEC toggles the swap flag only when the swapped value is 1 or 2
  (`zopen6Int.f:ZOPEN6INT` lines 256-267). The public `snodas_test.dss` is this case.

---

## 4. Hash function, hash table, and pathname bins

### 4.1 Pathname normalization before hashing

These steps follow `zcheck6.f:ZCHECK6`:

1. Characters `a-z` become `A-Z`, and any character below 32 becomes `?` (lines
   70-82). Stored pathnames are therefore upper case (verified: there were no
   lower-case pathnames in the samples).
2. Pseudo-regular paths (E part `~1DAY` and similar) are converted to the stored
   irregular form (`IR-YEAR` and so on) before lookup (lines 87-97,
   `heclib_f/src/zpseudorts6.f:ZPSEUDORTS6`; see §6.3).
3. **Collections.** If the pathname contains `|` and `/C:` with the `|` exactly 9
   characters after the `/`, the 6 characters after `/C:` are replaced by `XXXXXX` and
   the result is upper-cased. **That collection path is what gets hashed**, but the
   real pathname is what is stored and compared (lines 174-208;
   `heclib_f/src/zcolpath6.f:ZCOLPATH6` lines 25-39).
   The source is subtle: `strcpy(CHPATH(ICOLL+3:IBAR),'XXXXXX')` (line 33) writes into a
   7-character slot that includes the bar. Line 34 then restores the bar from
   `CPATH(IBAR:)`, leaving the 6 X's described above. There are no collections in the
   surveyed corpus, so §12 O12 stays open.

### 4.2 Hash function (`zhash6`)

Source: `heclib_f/src/zhash6.f:ZHASH6` lines 11-80. Input: the pathname bytes `p[1..n]`
and `MAX = KHASH`.

1. `IBIT` = the smallest `i` in 9..13 with `MAX ≤ 2^i`, or 14 if there is none. So
   `KHASH ≤ 512` gives 9, 1024 gives 10, 2048 gives 11, 4096 gives 12, and 8192
   gives 13.
2. `NT = ceil(n*8 / IBIT)` groups, then `NT = ceil(NT*IBIT / 8)` bytes to consume. The
   bytes past `n` are NUL: positions `n+1` and `n+2` are set to `CHAR(0)`, and
   blank-fill beyond that is never reached for `IBIT ≤ 13`.
3. Treat bytes `1..NT` as an MSB-first bit stream. Split it into consecutive
   `IBIT`-bit groups, dropping any incomplete final group, and XOR all the groups
   together into `IT`.
4. `hash = (IT mod MAX) + 1`, in the range `1..MAX`.

**Verified:** this algorithm reproduces the stored `KBHASH` for every live record in
all 34 sample files (more than 17,000 records), and every hash-table chain reaches the
record's bin.

Hash size codes (`zfsize6.f:ZFSIZE6` lines 19-30 and 58-75; the default is code 6,
lines 174-178):

| `KHSIZE` | Name | `KHASH` | `KBNBLK` | Default record range |
|---|---|---|---|---|
| 1 | Tiny | 8 | 4 | 1-20 |
| 2 | Extra small | 32 | 4 | 21-99 |
| 3 | Small | 128 | 8 | 100-499 |
| 4 | Medium | 512 | 16 | 500-1,999 |
| 5 | Large | 1024 | 32 | 2,000-7,999 |
| 6 | Extra large (default) | 2048 | 32 | 8,000-14,999 |
| 7 | Huge | 4096 | 32 | 15,000-29,999 |
| 8 | Extra huge | 8192 | 32 | 30,000-60,000 |
| 9 | Super | 8192 | 64 | > 60,000 |

The `IRANGE` thresholds in the data statement (1, 2, 20, 100, ...) do not match the
"Default Range" comment. Writers that pick a code from a record count should follow
the `DATA` values (`zfsize6.f` lines 58-75, 164-166). Readers must use `KHASH`,
`KBNBLK`, and `KBNSIZ` from the header and never derive them from `KHSIZE`.

### 4.3 Hash table (KTABLE = 1)

- Entry for hash `h`: word address `NPERM + h = 69 + h`, so the table occupies
  addresses 70..69+KHASH (`zcheck6.f:ZCHECK6` lines 211-217; `znwrit6.f` lines 91-92).
- A value of 0 means no bin exists, so the pathname is not in the file.
- The table can span several records, so use the address mapping in §1.2.

### 4.4 Lock record and first bin

At creation, HEC rounds the first bin up to a record boundary, reserves the
**preceding record** as the lock record, and puts the first bin at the start of the
next record (`zopen6Int.f:ZOPEN6INT` lines 410-427 for type 1 and 435-451 for type 2):

- Type 1: compute address `KHASH + 70`, round up to the next record `JREC`. The lock
  record is `JREC`, and `KAFBIN` is the address of word 1 of record `JREC+1`.
- Type 2: the same calculation starting from address 70.
- `KLOCKB` = lock byte offset = `(JREC-1)*512` (lines 280-287 recompute it on open).

**At open, HEC finds the lock record from the header, not from `KAFBIN`**
(`zopen6Int.f:ZOPEN6INT` lines 280-287): address `KHASH+NPERM+1` (or `NPERM+1` when
`KTABLE` is 2), converted with `zgetrw6` and rounded up to the next record unless it
falls on word 1. The native reader uses HEC's rule and warns if `KAFBIN` is not after the
lock record.

So in practice the **lock record is the record just before `KAFBIN`**, in every corpus
file surveyed. This was
verified on every sample (castro: lock record 3 at byte 1024, first bin at record 4;
Muncie: lock record 18 at byte 8704). In samples written at version `6-K` or later,
the lock record contains only garbage, and no live address points into it. In the
`6-JE` sample it contains about 190 bytes of garbage, and still nothing points into
it (§12, item O6).

**Pre-`6-K` files: the lock record can hold live bins [VERIFIED-CORPUS].**
"Nothing points into it" holds for the loose files surveyed, but not for every file. One
`6-EA` file (the §12 O21 file, a zip member) has `KAFBIN` inside the lock record, and HEC
reads that record normally (`zgtrec6.f:ZGTREC6` lines 122-157). Readers must not exclude
the lock record for files before `6-K`. The `dss-core` reader was fixed accordingly
(`dss6/mod.rs`).

### 4.5 Bin layout

A bin is `KBNSIZ` (127) words, numbered 1-based as in HEC. It holds entries packed
from word 1. For an entry starting at word `j` whose pathname is `npath` characters,
let `P = (npath-1)/4 + 1` (pathname words):

| Offset from `j` | Name | Meaning | Use |
|---|---|---|---|
| +0 | `KBSTAT` | status (table below) | R |
| +1 | `KBNPAT` | pathname length in characters | R |
| +2 .. +1+P | `KBPATH` | pathname (packed characters) | R |
| +2+P | `KBAINF` | address of the info block | R |
| +3+P | `KBNHEA` | **user** header length in words (`NUHEAD`, `znwrit6.f` line 119; verified to equal `KINUHE`) | I |
| +4+P | `KBNDAT` | stored data length in words | I (use the info block) |
| +5+P | `KBLNDA` | logical number of data | I (may differ from the info block, §12 item O9) |
| +6+P | `KBTYPE` | record type (§5.3) | R (catalog type without reading the info block) |
| +7+P, +8+P | `KBTAG` | record tag, 8 characters | I |
| +9+P | `KBHASH` | hash code used for this record | R (validation) |
| +10+P | `KBPINTL` | pseudo-regular interval in minutes (0 = none) | R (catalog display path, §6.3) |

Entry length = `P + NLBIN` with `NLBIN = 11` (`zinit6.f:ZINIT6` lines 654-696). The
next entry starts at `j + P + 11`.

Reserved bin words: **word `KBNSIZ-1` (126)** is the address of the next (overflow) bin
for the same hash chain, and **word `KBNSIZ` (127)** is the address of the next bin
**block**. Word 127 is meaningful only in the last bin of a block, and is used only
by the catalog walk (`zinit6.f` lines 144-145; `heclib_f/src/znwbin6.f:ZNWBIN6` lines
51-84).

Scan termination within a bin (`zcheck6.f` lines 249-322; `heclib_f/src/zcatfi6.f:ZCATFI6`
lines 91-214):

- status `0`: no more entries in this bin.
- status `-1`: this bin is full. For a hash lookup, continue at the address in word
  126. For a catalog walk, move on to the next bin.
- After an entry, if the next start is `> KBNSIZ-2` (125), the bin is finished.
- A new entry is only placed if `j + P + 11 + 2 ≤ KBNSIZ`
  (`znwrit6.f:ZNWRIT6` lines 98-104).

Status codes (`zinit6.f` lines 120-131; set in `znwrit6.f` lines 110-114,
`heclib_f/src/zdelet6.f:ZDELET6` lines 54-63, `zcheck6.f` line 301 (+2 on
re-mark), `heclib_f/src/zundel6.f:ZUNDEL6` line 65 (-1 on undelete)):

| Status | Meaning | Catalog (R) |
|---|---|---|
| 1 | live | list |
| 11 | live, "long" pathname (> 80 characters or any part > 32) | list |
| 2 / 12 | deleted (short or long pathname) | skip |
| 3 / 13 | renamed (old name) | skip |
| 4 / 14 | deleted, then rewritten under the same name (old entry re-marked +2) | skip |
| 0 | end of entries in this bin | stop this bin |
| -1 | bin full; overflow pointer in word 126 | stop this bin |
| < -1000 or > 1000 | corruption (`zplist6` returns an error) | error |

### 4.6 Locating a pathname (R)

1. Normalize (§4.1) and compute `h = zhash6(hash_path, KHASH)`.
2. For type 1, `a = word[69+h]`, and if it is 0 the pathname is not found. For type 2,
   `a = KAFBIN + (h-1)*KBNSIZ`.
3. Read the bin at `a`. Scan its entries. A match needs status 1 or 11, equal length,
   and an equal pathname (byte compare). If the scan hits status -1, set
   `a = bin[126]` and repeat this step. Status 0 or the end of the bin means not found.
4. Confirm the match in the info block (§5): flag -9753, the same pathname, and the
   same length (`zrdinf6.f:ZRDINF6` lines 56-62).

**How lookup reads status codes**. `zcheck6` subtracts 10 from any
status above 10 before comparing, so statuses 11–14 compare as 1–4
(`zcheck6.f:ZCHECK6` lines 255-260). Status 0 ends the bin (line 252). The native reader
also stops the bin at any status ≤ 0 other than -1; HEC would misparse such a value.

---

## 5. Record information block, headers, and data

### 5.1 Info block

The info block is at the address `KBAINF` taken from the bin. Its length is
`NINFO + P` with `NINFO = 28` (`zinit6.f:ZINIT6` lines 699-774). Offsets are 0-based
from the info address, with `P` the number of pathname words. All were verified.

| Offset | Name | Meaning | Use |
|---|---|---|---|
| 0 | `KIFLAG` | `-9753` (`NPFLAG`) marks an info block | R (validate) |
| 1 | `KISTAT` | status: same codes as the bin, plus **-1 = relocated** (data now elsewhere) | R (validate) |
| 2 | `KINPAT` | pathname length in characters | R |
| 3 .. 2+P | `KIPATH` | pathname | R |
| 3+P, 4+P | `KIPASS` | record password (encoded); 0 in all samples | R (nonzero means HEC denies access, `zrdinf6.f` lines 64-73). **HEC checks only the first password word (`KIPASS` word 1), and only when the file version's third character is `E` or later** **[VERIFIED-CORPUS]** |
| 5+P | `KIADAT` | data address | R |
| 6+P | `KINDAT` | data length in words (as stored) | R |
| 7+P | `KILNDA` | logical number of values (uncompressed; for RTS the value count of the block) | R (TS) |
| 8+P | `KIVER` | record version (times written, capped at 999) | I |
| 9+P, 10+P | `KIPROG` | program that last wrote it, 6 characters (`RasUns`, `HECHMS`, `DSSUTL`, ...) | I |
| 11+P, 12+P | `KIDATE` | last-written date `DDMMMYY` | I |
| 13+P, 14+P | `KITIME` | last-written time `HH:MM:SS` | I |
| 15+P, 16+P | `KITAG` | tag, 8 characters | I |
| 17+P | `KITYPE` | record type (§5.3) | R |
| 18+P | `KICOMP` | data compression scheme (0 = none, 1..5, §8) | R |
| 19+P | `KIQUAL` | quality flag present (0/1) | R |
| 20+P | `KIAIHE` | internal header address | R |
| 21+P | `KINIHE` | internal header length in words | R |
| 22+P | `KIACHE` | compression header address | R |
| 23+P | `KINCHE` | compression header length in words | R |
| 24+P | `KIAUHE` | user header address | R |
| 25+P | `KINUHE` | user header length in words | R |
| 26+P | `KIPREC` | data precision (0 undefined, 1 tenths, 2 hundredths, ...) | I (odd values such as 21 and 54 appear in old files) |
| 27+P | `KIUNUS` | unused words left after a shrinking in-place rewrite | I/W |

**Layout of a record.** As written by `znwrit6` and by `zowrit6` when the record
grows, the parts are contiguous: `info | internal header | compression header | user
header | data` (`znwrit6.f:ZNWRIT6` lines 154-186; `heclib_f/src/zowrit6.f:ZOWRIT6`
lines 131-163). This held for every live record sampled. Readers must still follow
the **addresses** in the info block and not assume adjacency: an in-place rewrite
recomputes the addresses but leaves `KIUNUS` slack (`zowrit6.f` lines 164-173).

Reading a record (R): read the info block → validate it → read each header with its
own address and length, then the data (`heclib_f/src/zreadx6.f:ZREADX6` lines 40-66).
Bounds-check every address and length against the physical file size, not against
`KFSIZE + 20000` as HEC does (`zgtrec6.f` lines 65-72).

### 5.2 Internal headers by record type

**Regular TS (100/105), `KINIHE` up to 24 words** (`heclib_f/src/zrrtsi.f:ZRRTSI6`
lines 576-603; written `heclib_f/src/zsrtsi6.f:ZSRTSI6` lines 871-887):

| Word (1-based) | Meaning |
|---|---|
| 1 | time offset in minutes (`IOFSET`) |
| 2-3 | units, 8 characters |
| 4-5 | data type, 8 characters (`INST-VAL`, `PER-AVER`, `PER-CUM`, ...) |
| 6 | time-zone offset (`IWTZONE`; -1 = not set), **in minutes, west negative**: -480 for `GMT-08:00` and `PST8PDT` **[VERIFIED-CORPUS; O14 closed]** |
| 7-12 | time-zone name, 24 characters (only 7-9, 12 characters, when `KINIHE = 9`) |
| 13-18 | coordinates, 3 × `REAL*8` |
| 19-24 | coordinate descriptors, 6 integers |

Observed lengths: 5 (older writers: offset, units, and type only), 9, and 24.

**Irregular TS (110/115), up to 28 words** (`heclib_f/src/zritsi6.f:ZRITSI6` lines
724-746; written `heclib_f/src/zsitsi6.f:ZSITSI6` lines 981-1001):

| Word | Meaning |
|---|---|
| 1 | allocated block capacity in values (`IBSIZE`) |
| 2 | number of values stored (`NPAIRS`) |
| 3 | block start julian day (`JULSB`) |
| 4 | time of the last value, in minutes from the block start |
| 5 | pseudo-regular interval in minutes (`INTLPS`) |
| 6-7 | units |
| 8-9 | data type |
| 10 | time-zone offset (`dss-core` decodes it like RTS word 6, minutes with west negative; the corpus check established the rule for the RTS time-zone word and gave no ITS-specific evidence) |
| 11-16 | time-zone name, 24 characters (11-13 when `KINIHE = 13`) |
| 17-22 | coordinates, 3 × `REAL*8` |
| 23-28 | coordinate descriptors |

**Paired data (200/205)** (`heclib_f/src/zrpdi6.f:ZRPDI6` lines 171-200;
`DssInterface/v6and7/zpdRetrieve6.c:zpdRetrieve6()`). **[VERIFIED-CORPUS]**
against Monolith on 1,302 records of private HEC-written test files and 1,494 other corpus records (the one difference is
the label trim noted below); decoded without error on 403,000 sampled corpus records.

Internal header (1-based words):

- Word 1 is `NORD` (ordinates), 2 is `NCURVE`, 3 is `IHORIZ`.
- Words 4-5, 6-7, 8-9 and 10-11 hold, in order, the independent units, independent type,
  dependent units and dependent type. Each is 8 characters. HEC's C layer returns them with
  trailing blanks trimmed and cut at the first NUL.

Short labels:

- Curve `I` has a 12-character label at words `3I+9 .. 3I+11`.
- A label is read only while `3I+11 ≤ NIHEAD` (`KINIHE`) and `I ≤ 100`.
- Labels are returned only if some label has a non-blank character in its first 8.

Long labels:

- They exist when `NIHEAD > 3*NCURVE+11`.
- Word `3*NCURVE+12` is `NWDS`, and each label is `NWDS` words starting at word
  `3*NCURVE+13`.
- They are used only if `NIHEAD ≥ 3*NCURVE+12+NWDS`. In that case short labels are ignored.

Label strings returned by HEC's C layer: at most 100 labels of 101 characters. Trailing
blanks are trimmed; **leading blanks are kept** (`zpdRetrieve6.c` lines 196-212 trim with
`chrlnb_`, a Fortran `len_trim`). Monolith's Java DSS6 path also trims leading blanks: one
public record (`sample6.dss`, curve label ` 5%LIMIT`) reads `5%LIMIT` in Monolith, while
the C semantics (and the same record in a DSS7 file) give ` 5%LIMIT`. One further corpus
label shows the same trim.

Data: `NORD` ordinates followed by `NCURVE*NORD` values, curve-major, `REAL*4` (200) or
`REAL*8` (205).

Sizes:

- Size PD from `NORD`/`NCURVE`, never from `KILNDA`. HEC reads `KINDAT` words; values past
  `KINDAT` would come back as zeros, so a reader should report an error instead.
- `KILNDA` is stale in 10,161 of 403,000 sampled records (246 files); castro has
  `KINDAT = 20`, `KILNDA = 144`.
- `KINDAT` larger than needed occurs (2,195 of 403,000 sampled records); extra words are
  ignored.

Other:

- Precision is the info word `KIPREC` (`x = p/10`, `y = p%10`; negative means not set).
  DSS6 stores no time zone.
- Observed `KINIHE`: 11 (254,602 sampled records, 63%) and 14 (148,390, 37%), plus 17, 20,
  23, 42, 68 and 80. Long labels (`NWDS` 7, 11 and 14) occur in one loose file.

**Grids (400-431):** observed `KINIHE` is 66 (HRAP, type 410) and 75 (Albers, 420). The
info block internal header is the Java `GridInfoFlat` int array (classes `GridInfoFlat`,
`HrapInfoFlat`, `AlbersInfoFlat` and `SpecifiedGridInfoFlat`, inspected from the pinned
HEC Monolith 3.3.27 jar). Ints are zero-based
**[VERIFIED-CORPUS; decoded on 14 private test files with
bit-for-bit matches on 7,784 grids against Monolith, and on 57,487 loose-corpus grids
with 0 errors]**:

| Ints (zero-based) | Meaning |
|---|---|
| 0..5 | flat byte size (may be stale), type, info size, grid-info size, start/end `HecTime` minutes |
| 6..8, 9 | 12-byte units, data-type code |
| 10..14 | lower-left x/y, nx/ny, f32 cell size |
| 15..18 | compression, exact compressed byte count, f32 scale/base |
| 19..22 | f32 max/min/mean, range count |
| 23..42, 43..62 | fixed 20 range-limit slots and 20 range-count slots |
| 63..65 (HRAP) | 12-byte data source |
| 63..74 (Albers) | datum; 12-byte units; six projection f32s; cell-zero x/y |
| 63 onward (Specified) | version; name word count + string; definition type; definition word count + string; x0/y0/null; time-zone word count + string; offset; interval/timestamp flags |

Rules that go with the table:

- Some ints are reinterpreted as `f32`; strings are word-counted (the 63-int base and fixed
  20-slot tables come from `GridInfoFlat`; variable strings from `SpecifiedGridInfoFlat`).
- Compression methods seen: 1 (none), 26 (ZLIB), 101001 (PRECIP_2_BYTE). PRECIP_2_BYTE
  uses RLE with the **high half-word first** within each stored int (`DataCompression.uncompress`
  and `GriddedData.retrieveGriddedData`), the exact compressed byte length from int 16, and
  scale/base arithmetic done in `double` then rounded to `f32`.
- The first-int byte size can be **stale**: DSS6 specified grids (type 430) store 296 while
  the actual internal-header transfer is longer (`SpecifiedGridInfoFlat` variable fields are
  populated after the constructor sets the size). The real header length (`KINIHE`) is
  authoritative; bound every variable string independently. All 127 records of one private
  test file and 1,680 grids in one loose file show this.
- Missing values follow Java heclib: legacy ZLIB values `<= -1e38` become the undefined
  marker (`DataCompression.zuncompress`); specified null markers and HRAP/Albers values
  outside the stored min/max are normalized too; PRECIP_2_BYTE missing runs decode to the
  undefined marker. A reader that wants raw bits needs an audit option.
- Cell order is native flat order, x fastest, no row flip.
- Albers WKT: the header stores no WKT. Monolith reports the SHG WKT or `""` from ints
  63..74 (datum 2, `isMeters` units, the six SHG parameters, cell-zero origin 0/0); the
  units are the bytes before the first NUL with trailing blanks kept. See the `dss7.md`
  §5.2 Albers SRS rule **[VERIFIED-ORACLE]**.

- **Writing (`dss6::write::write_grid6`)** **[VERIFIED-ORACLE]**. HEC Monolith
  3.3.27 `GriddedData.storeGriddedData` (DSS6 branch) builds the record and stores it with
  `Heclib.zwritx` (`javaHeclib/src/Hec_zwritx.c` -> `DssInterface/v6and7/zwritex.c` ->
  `zwritex6.f`):
  - int 0 is the flat size the constructor set: 264 (HRAP), 300 (Albers), 296
    (specified, before its strings are known); int 2 the info size 128/164/160 and int 3
    the grid-info size 124 (`GridInfo.<init>`, subclass constructors);
  - ints 4/5 are `HecTime.value()` minutes of the D/E times; an unset `HecTime` stores
    -2147483647 (`UNDEFINED_TIME`, `headers/heclibDate.h`), which Monolith writes for
    grids without times and reports as no time (the reader treats it so);
  - strings are NUL-padded (`Heclib.StringHol`): units, HRAP data source and Albers units
    in three ints; specified strings `ceil(len/4)` words after a word count;
  - statistics are taken first, then -FLT_MAX cells become the null value (-1.0E30, or
    the specified grid's), then the cells are compressed (offsets 81-133). ZLIB:
    `DataCompression.zcompress` turns -FLT_MAX into -1.0E38, deflates little-endian floats
    at level 9, pads to whole ints; scale/base stay the class defaults (-FLT_MAX twice;
    specified 1.0 and 0.0). PRECIP_2_BYTE: `DataCompression.compress`, two codes per int
    with the first in the **high** half-word (the Windows swap at offsets 824-881; the
    reader swaps back); a run of exactly 16000·k cells ends with a zero-length run, which
    Java's `uncompress` accepts (the reader does too);
  - with no compression chosen, Monolith picks PRECIP_2_BYTE for a version-1 grid (HRAP,
    Albers) whose D-E period is exactly one hour and whose type is PER-CUM, else ZLIB
    (offsets 288-363); its automatic scale/base (offsets 425-528) can leave the maximum
    above 32767 steps, and such cells are stored as missing (the native writer refuses);
  - `KITYPE` is the grid type (410/420/430 for every Monolith grid), `KILNDA` = `KINDAT`,
    `KIPREC` 0.
  - Native and Monolith records from the same empty file are equal word for word except
    dates, program and tag, and the deflate stream of ZLIB records (identical in 16 of 19
    records of the fixture; the inflated cells always equal).
  - **HEC defect (synthetic data only):** Monolith cannot read back its own DSS6
    specified grid whose time-zone name is longer than four characters (more than one
    int; `SpecifiedGridInfoFlat.unflatten` throws `ArrayIndexOutOfBoundsException`;
    `UTC` and two-character names read, `ABCDE`, `ABCDEFGH` and `GMT-05:00` do not); the
    engine reads them.
### 5.3 Record type codes

From `zinit6.f:ZBKDAT` lines 934-971. These are the only codes DSS6 defines. Anything
else catalogs as `UND` (`zcatfi6.f` lines 178-185). A generic `zwritex6` write with no
type set stores 0 (`zinit6.f` line 271; `znwrit6.f` line 188).

| Code | Abbreviation | Description |
|---|---|---|
| 100 | RTS | regular-interval time series (`REAL*4`) |
| 101 | RTP | regular-interval TS pattern |
| 105 | RTD | regular-interval TS, doubles |
| 110 | ITS | irregular-interval TS |
| 111 | ITP | irregular-interval TS pattern |
| 115 | ITD | irregular-interval TS, doubles |
| 200 | PD | paired data |
| 205 | PDD | paired data, doubles |
| 300 | TXT | text |
| 400 / 401 | GUT / GU | grid, undefined (with or without time) |
| 410 / 411 | GHT / GH | grid, HRAP |
| 420 / 421 | GAT / GA | grid, Albers |
| 430 / 431 | GST / GS | grid, SHG |

Observed in the samples: 100, 101, 105, 200, 410, 420.

---

## 6. Catalog (listing records)

### 6.1 HEC's walk

HEC lists records by **walking the bin blocks sequentially**, not through the hash
table (`zcatfi6.f:ZCATFI6` lines 61-237; also `heclib_f/src/zplist6.f:ZPLIST6` lines
97-237, which is what the C API uses through
`heclib_c/src/Dss6/zcatalog6Internal.c:zcatalog6Internal` lines 164-212):

```
nbin = (KTABLE == 1) ? KBNBLK : KHASH     # zcatfi6 lines 62-69
addr = KAFBIN
loop:
  bin = read KBNSIZ words at addr
  for each entry (sec. 4.5 scan rules):
     if status in {1, 11}: emit (pathname, type, sizes, KBPINTL)
     stop the entire walk once KNRECS entries have been emitted   # zcatfi6 196, zcatalog6Internal 205
  nbin -= 1
  if nbin == 0:
     if bin[KBNSIZ] == 0: done
     addr = bin[KBNSIZ]; nbin = KBNBLK           # next block
  else:
     addr += KBNSIZ                               # next bin in this block
```

- **Order** is bin order, which is effectively hash/insertion order. It is not sorted.
  Sorting is a separate step in `zcat6` (default part order A, B, C, F, E, D,
  `zcat6.f:ZCAT6` lines 159-165).
- **Skipped:** statuses 2/12, 3/13, 4/14, and `≤ 0`. Deleted and renamed entries stay
  in the bins until a squeeze (`heclib_f/src/zsqueeze6.f`). Dead data areas are never
  referenced by live entries, so a bin walk skips dead space automatically.
- The walk visits overflow bins because they are allocated inside the same chain of
  blocks.
- The type-1 first block holds `KBNBLK` bins. For type 2, the first "block" is all
  `KHASH` preallocated bins, and the link to the next block is in word 127 of the
  **last** preallocated bin (`znwbin6.f` lines 67-76 with `KANBIN` set in
  `zopen6Int.f` lines 453-455).
- **Discrepancy:** `zplist6` always starts with `nbin = KBNBLK`, even for type-2 files
  (`zplist6.f` line 100). For a type-2 file that would read word 127 of bin `KBNBLK`,
  which is usually 0, and stop early. Follow `zcatfi6` (§12 O3).
- `zplist6` also skips entries with `npath ≤ 0` or `npath > 390` (lines 148-149).
  `zcatfi6` and `zcatalog6Internal` stop once `KNRECS` paths have been emitted. In all
  34 samples the full walk produced exactly `KNRECS` live entries. A reader should
  walk to the end, report any mismatch, and by default emit what it finds.

Verified examples: tifton (6-NB) has 132 deleted entries (status 2) and lists 468 =
`KNRECS`. Muncie (6-UF) has 30 status-11 long paths (up to 90 characters) and 15
overflow bins. Bald_Eagle_Creek (6-ND) has 242 overflow bins and 123 long paths.

### 6.2 What a read-only lister needs

`KAFBIN`, `KBNBLK`, `KBNSIZ`, `KHASH`, `KTABLE`, `KNRECS`, and `KFSIZE` from the
header, plus the bin entry fields `KBSTAT`, `KBNPAT`, `KBPATH`, `KBTYPE`, and
`KBPINTL`. The info block is optional (needed only for the extended catalog: program,
date, version), and the hash table is not needed. Cycle-guard the block chain and
bounds-check each address against `KFSIZE` and the physical size.

### 6.3 Displayed pathnames

- **Pseudo-regular TS.** If `KBPINTL > 0`, HEC (with `LPSEUDO = .TRUE.`, the default
  in `zinit6.f:ZBKDAT` line 883) displays the E part as `~<interval>` (for example
  `~1DAY`) in place of the stored `IR-*` (`zplist6.f` lines 155-164;
  `zpseudorts6.f:ZPSEUDORTS6` lines 76-86). The stored pathname, which is what gets
  hashed, keeps `IR-*` (§12 O15).
- **Time series blocks.** Every TS block is its own record, and the D part is the
  block start date (`DDMMMYYYY`, §7). Condensed catalogs (one line per series) are a
  presentation step, not file content.
- DSS6 HEC tools also write **external** catalog files (`datab.dsc`, plus condensed
  `.dsd`) beside the DSS file (`heclib_f/src/zopnca6.f:ZOPNCA6` line 14). They are not
  part of the format. Readers must not depend on them, and writers need not maintain
  them.

---

## 7. Time series layout

### 7.1 Julian days, times, and intervals

- Julian day: `IYMDJL` gives days with **01JAN1900 = 1** (31DEC1899 = 0)
  (`heclib_f/src/Support/iymdjl.f:IYMDJL` lines 6, 18, 47). Two-digit years go through
  `ADDCENTURY`.
- Time of day: minutes 1..1440. Midnight is 1440 (24:00) of the previous day (the
  `ISTIME = 0` case becomes `JULS-1, 1440`: `zrrtsi.f:ZRRTSI6` lines 214-218).
- Interval E parts, minutes, and values per block (`heclib_f/src/zgintl6.f:ZGINTL6`):
  `1YEAR` 525600/100, `1MON` 43200/120, `SEMI-MONTH` 21600/240, `TRI-MONTH` 14400/360,
  `1WEEK` 10080/522, `1DAY` 1440/366, `12HOUR` 720/62, `8HOUR`/93, `6HOUR`/124,
  `4HOUR`/186, `3HOUR`/248, `2HOUR`/372, `1HOUR` 60/744, `30MIN`/1488, `20MIN`/2232,
  `15MIN`/2976, `12MIN`/120, `10MIN`/144, `6MIN`/240, `5MIN`/288, `4MIN`/360,
  `3MIN`/480, `2MIN`/720, `1MIN`/1440. Matching uses the **first 4 characters**,
  case-insensitive. A leading `~` means pseudo-regular. `IR-CENTURY`, `IR-DECADE`,
  `IR-YEAR`, `IR-MONTH`, and `IR-DAY` mark irregular data.

### 7.2 Regular-interval TS blocks

Block selection by interval (`heclib_f/src/zbegdt6.f:ZBEGDT6` lines 11-57; stepping in
`heclib_f/src/zincbk6.f:ZINCBK6`):

| Interval (minutes) | Block | Block start |
|---|---|---|
| < 15 | day | that day |
| 15 ≤ i < 1440 | month | first of the month |
| 1440 ≤ i < 10080 | year | 01JAN |
| 10080 ≤ i < 525600 | decade | 01JAN of year `(y/10)*10` |
| ≥ 525600 | century | 01JAN of year `(y/100)*100` |

**Files before `6-I` (`KVERNO < 67300`) put 15- and 30-minute data in daily blocks**
**[VERIFIED-CORPUS; O21 closed]**. HEC master has this rule commented out
(`zbegdt6.f` lines 27-32, comment "1990 time frame"), but Monolith 3.3.27 still applies it;
verified on a `6-EA` file (the §12 O21 file). The reader implements it as
`regular_block_for(interval, kverno)`; with the rule applied, counts and stamps match
Monolith exactly (30 dataset requests on that file). Writers must never write 15- or
30-minute daily blocks. For weekly data, see the weekly rules below.

- The **D part on disk** is the block start date in `DDMMMYYYY` form (`YMDDAT` style
  104), for example `01JAN1951` (`zrrtsi.f:ZRRTSI6` lines 258-268).
- Every block is stored **full**. `KILNDA` = `NOPERS(interval, block start, next
  block start)`, and unwritten slots are `-901.0` (`zsrtsi6.f:ZSRTSI6` lines 678-691,
  715-745). Verified: 1HOUR April block = 720 values, 15MIN December block = 2976.
- **Value `k` (1-based) is at block start 00:00 + `k` × interval**, the end-of-period
  convention: `NPOS = NOPERS(INTL, 0, JULSD, 0, JULS, ISTIME)` (`heclib_f/src/zrrtsb6.f:ZRRTSB6`
  line 43; the first value without a time window is `INCTIM(+1 interval)` from the D
  date, `zrrtsi.f` lines 200-206). So the first `1HOUR` value of a January block is
  01JAN 01:00, and the last is 31JAN 24:00. Calendar intervals (`1MON`, `1YEAR`,
  semi-month, tri-month) step through `INCTIM` **[VERIFIED-CORPUS; O19
  closed]**: the port matches HEC-written data (a `6-YO` file written with Monolith: 1MON,
  1YEAR, SEMI-MONTH, TRI-MONTH, stamps included).
- **Weekly data (`1WEEK`)** **[VERIFIED-CORPUS]**: value positions count
  from the Sunday on or before the D date. A block holds `NOPERS` values between aligned
  bases: HEC wrote 522 values for a 2000-2009 block, and our 522 values equal `KILNDA` and
  match Monolith's first 522 values and stamps. Monolith's reader appends one extra missing
  slot (523 values), a Monolith quirk. Align weekly blocks to Sunday when writing.
- Internal header word 1 (`IOFSET`) is the offset in minutes of the data from the
  standard interval boundaries (`zrrtsi.f` line 576; `heclib_f/src/zofset6.f`).
  **Offsets and windows** **[VERIFIED-CORPUS]**:
  - A value's standard stamp is `INCTIM(k)` from the block start; its actual stamp is
    `standard - interval + offset` (`ZOFSET6` with `IFLAG` 2).
  - A time window selects **standard** stamps, and both ends move up to the next standard
    stamp (a window ending 13:13 on daily data includes that day's 24:00 value). Each
    selected stamp is then moved to `standard - interval + offset`.
  - When blocks have different offsets, the last block's offset is used for all stamps
    (`zrrtsi.f` line 576).
  - Absent blocks inside a window are filled with `-902.0` (`zrrtsx6` behaviour, §7.4).
  - Irregular windows are inclusive at both ends.
  - A `~1DAY` pathname reads the stored `IR-YEAR` blocks (§6.3).
  - Verified against 19,451 of 19,451 records (stamps included) in six corpus files with
    offsets and time zones, and against HEC-written 1DAY (480-minute offset) and 1HOUR
    (30-minute offset) records. Corpus offsets seen: 15, 30, 90, 360 and 480 minutes.
- **Multi-block header selection (`zrrtsi.f:ZRRTSI6`)** **[VERIFIED-CORPUS]**:
  when a dataset read spans several blocks, the **units, data type and time zone** (and the
  time offset above) are taken from the **last block read**, not the first block and not the
  last block in the file. The public fixture `b1_multiblock6.dss` (`6-YO`, written by Monolith
  3.3.27; three month blocks, each with different units/type headers) gives, for regular and
  irregular datasets alike: whole dataset -> last block's header; a Jan+Feb window -> Feb's;
  a Jan-only window -> Jan's. Monolith agrees on the irregular path too (the first
  confirmation of that path).

Data area encodings (`zrrtsb6.f:ZRRTSB6` lines 71-221; sizes `zsrtsi6.f` lines
678-691):

| Type | `KIQUAL` | `KICOMP` | Data area (`KINDAT` words) |
|---|---|---|---|
| 100 | 0 | 0 | `KILNDA` × `REAL*4` |
| 100 | 1 | 0 | `KILNDA` × (`REAL*4` value, `INTEGER*4` quality), **interleaved** (`2*KILNDA`) |
| 105 | 0 | 0 | `KILNDA` × `REAL*8` (`2*KILNDA` words) |
| 105 | 1 | 0 | `KILNDA` × `REAL*8`, **then** `KILNDA` quality words (`3*KILNDA`) |
| 100 | 0 | 1..5 | compressed (§8), with the compression header at `KIACHE` |

Compression is never combined with quality or doubles (`zsrtsi6.f` lines 600-605;
"If data compression used, record cannot be double", `zrrtsb6.f` lines 73-76).

### 7.3 Irregular-interval TS blocks

- Block size comes from the E part (`IR-DAY`, `IR-MONTH`, `IR-YEAR`, `IR-DECADE`,
  `IR-CENTURY`, matched on the first 6 characters). For a pseudo `~interval`, the
  thresholds are < 15 day, < 1440 month, < 10080 year, < 50000 decade, otherwise
  century (`heclib_f/src/zirbeg6.f:ZIRBEG6` lines 20-90). The D part is the block
  start date.
- Data: `NPAIRS` (internal header word 2) groups of `IMULT` words, with `IMULT = 2 +
  (KIQUAL==1) + (type==115)` (`zritsi6.f:ZRITSI6` lines 233-246). Each group is
  `[time, value (1 word REAL*4 or 2 words REAL*8), quality?]`
  (`heclib_f/src/zircpy6.f:ZIRCPY6` lines 26-80).
- **Time** is the `INTEGER*4` minutes from the block start day (internal header word
  3) at 00:00 (`zsitsi6.f:ZSITSI6` lines 683-684). A value at 24:00 of the last day
  belongs to the block.
- `KINDAT` can exceed `NPAIRS*IMULT`, because blocks are allocated with spare capacity
  (internal header word 1; growth `KITSIN`, minimum block size `KITS*`, `zirbeg6.f`).
  Read only `NPAIRS` groups. **[VERIFIED-CORPUS]** Spare words exist (two private test files and
  `synth6`) and are never read; counts agree with `NPAIRS` for every irregular record in
  the corpus run (O18).

### 7.3b Writing patterns and pseudo-regular data

A regular pattern (`zsrtpa6.f`, type 101) has a 5-word internal header and `KBLNDA` n. An irregular pattern (`zsitsi6.f` lines 213-266, type 111) has the 28-word header, n pairs and `KBLNDA` 2n. Both are floats only. HEC pattern writes omit the end-of-file flag that the native transaction writes; that word is a documented difference. Irregular patterns are written only on explicit opt-in (`Dss6IrregularPattern::AllowUnconfirmed`; Python `allow_unconfirmed_dss6_irregular_pattern=True`): Monolith 3.3.27 misreads the tested type-111 records from both writers, no HEC reader has confirmed them, and the private test corpus holds none.

A pseudo-regular write is an irregular write to the `IR-*` block `zpseudorts6.f` selects, with the interval in minutes in internal-header word 5 (`INTLPS`) and in the bin (`KBPINTL`, `zsetfi6.f` line 91).

### 7.3a Pattern time series (101/111)

**[VERIFIED-CORPUS; O20]**

- The value count is `KINDAT` (`zrrtpa6` calls `zreadx6`); `KILNDA` is stale for patterns.
- Patterns carry an RTS internal header but never compression or quality.
- Their D part starts with `TS-` and holds no dates (§10), so no real times exist. Values
  match Monolith (443 type-101 records in the private test files); Monolith's pattern times are
  overflowed year-2571 values, so times were not compared. `dss-core` uses
  k x interval counted from the epoch, a convention still to be agreed with the DSS7
  pattern-time convention.
- **Time origin (decided).** `dss-core` (`dss6/ts.rs`) still returns regular pattern
  times as k × interval seconds from the HEC epoch, with k = 1, 2, …: a 15MIN pattern gives
  900, 1800, …. The Python layer (`ras_commander_fast_dss._file._normalize`) subtracts one interval
  and adds 01JAN3000 00:00. So the public first value is at 01JAN3000 00:00, then one per
  interval, which is the DSS7 convention (dss7.md §7.5a). `pattern_heclib_first_seconds` is
  computed in `dss-core` as an absolute 01JAN3000-based time on both versions.
  Internal header word 0 is the "offset" in minutes. `zsrtpa6.f` (called from `zsrtsi6.f`
  lines 225-231) writes `INTBUF(1)=IOFSET`, but `IOFSET` is an uninitialized implicit local,
  so the word is unreliable. heclib's Fortran reader resets it to 0 (`zrrtsi.f` line 183).
  heclib C applies it as `internalHeader[0] * SECS_IN_1_MINUTE` when the header has at least
  5 words (`Public/ztsRetrievePattern.c` lines 279-288). That is why pydsstools and Monolith
  `startTime` show a private HEC-written test file's patterns at 00:03. dss-core **never applies** it.
  `Dss6TsRecord::pattern_origin()` reports word 0 × 60 (source
  `dss6_internal_header_word0_minutes`) and the heclib C start: 01JAN3000 00:00 plus one
  interval, moved to the word-0 offset when that is nonzero.
  - Irregular DSS6 patterns (111) keep the irregular header, where word 0 is the capacity
    (`zritsi6.f`), so they report no stored offset.
  - **HEC writes 111.** `zsitsi6.f` lines 213-266 store a `TS-` D part irregular
    write as (minutes, value) pairs, with `IIHEAD(3)` the block start day.
    `zritsi6.f` lines 154-184 read the pairs back, and so does the engine. Monolith 3.3.27
    and HEC-DSSVue (Monolith 5.0.1, heclib 7-IU) read through
    `Public/ztsRetrievePattern.c`, whose lines 280 and 326-336 apply the DSS7 layout (all
    times, then all values; the comment at lines 220-221 notes DSS6 is interleaved) and
    return twice the value count with values that do not match those written, in the tested
    builds. The engine follows the Fortran reader (`synthetic_hec_variants6.dss`,
    `/P33/IPAT/...`). The native DSS6 writer's type 111 records are therefore written but not
    confirmed by a HEC reader.
  - Checked on four private DSS6 test files and HMS 4.13 `castro/Current.dss`: 1,769 records; first
    times are unchanged.
  - Of these, 11 selected records plus 4 castro records were compared with the oracles. All
    match pydsstools 3.1.0, and all match Monolith `startTime` modulo 2^32 (Monolith's
    DSS6 pattern times overflow 32 bits).

### 7.4 Missing values and quality

- `-901.0` means missing. `-902.0` means "no data / record not present". HEC fills
  absent blocks with -902 on read (`zrrtsb6.f` lines 225-236; status checks in
  `zrrtsi.f` lines 617-651). Compressed data cannot represent -902: it decodes as -901
  (§8). **Stored `-902.0` also occurs on disk** (1,460 values in one private test file, 2,320 in
  another corpus file, and `sample.dss`), so it is not only HEC's fill for absent blocks; stored
  `-FLT_MAX` also occurs (in two private test files) **[VERIFIED-CORPUS]**. Readers
  keep the stored bits and map all three markers at the binding boundary.
- **All-missing regular blocks.** HEC DSS6 reports "no data" for a
  regular block whose values in the read window are all -901 or -902, although the
  block exists and is fully decoded. The source chain (DSS6 sources from hec-dss fork
  tag `7-IU-15`, commit `34ffe0c`, the release whose DSS6 code Monolith 3.3.27's
  `javaHeclib` carries; fork `main` has no DSS6 Fortran):
  1. `heclib/heclib_f/src/zrrtsi.f:ZRRTSI6` (tag `7-IU-15`) lines 618-649 set `LFOUND`
     only for a value that is neither -901 nor -902 (floats line 631, doubles line 645);
     **line 651 `IF (.NOT.LFOUND) ISTAT = 4`** ("No data found, although a pathname was
     read", line 56);
  2. `heclib/heclib_c/src/Dss6/ztsRetrieveReg6.c` (tag `7-IU-15`) copies values, units,
     user header and location, then **lines 255-259** return `STATUS_RECORD_NOT_FOUND`
     (-1) for status 4; `Public/ztsRetrieve.c` lines 373-377 and
     `javaHeclib/src/Hec_ztsRetrieve.c` pass it on unchanged;
  3. Monolith 3.3.27 `hec.heclib.dss.HecTimeSeries.read(TimeSeriesContainer, boolean)`,
     **bytecode 327-334** (`javap -c`), returns -2 for -1, with no values.

  DSS7 does not do this: `heclib/heclib_c/src/Internal/ztsRetrieveReg7.c` (hec-dss fork
  `main`, commit `6591930`) sets `foundOne = 1` for a block read whatever its values
  (line 641) and returns `STATUS_RECORD_FOUND` (lines 699-700). The rule is for regular
  data (`ZRRTSI6`); irregular reads have no such status.

  **By default (`no_data="return"`) the engine does not refuse such a block:**
  - **Record reads** (`read_record`, `read_record_arrays`, Rust `read_ts_record`) return
    the block: every slot, NaN by default, the stored -901/-902 with `raw=True`.
  - **Dataset reads** (`read_timeseries`, `read_arrays`, Rust `read_ts_dataset`) count
    the block as found. With `trim_missing=False` its slots are returned (NaN, raw
    markers kept). By default (`trim_missing=True`) leading and trailing missing values
    are trimmed as heclib's `ztsTrim` trims them, so a dataset made **only** of
    all-missing blocks has **no values**: Rust `read_ts_dataset` and Python `read_arrays`
    return empty arrays (not an error), and Python `read_timeseries` raises
    `DssPathnameNotFoundError` ("no data", as for any read with no values left). A
    leading or trailing all-missing block of a dataset with data is trimmed away.
  - -FLT_MAX is not a -901/-902 marker for `ZRRTSI6`, so HEC (and Monolith) read an
    all -FLT_MAX block as found; the Python layer still maps the values to NaN.

  Comparisons against Monolith count its -2 on such a block as an expected `no_data`
  result, not a read error. Verified: all 542 Monolith -2 reads of one private
  HEC-written DSS6 test file are such blocks (the engine returns the full `KILNDA` count, all raw -901), and on
  `synthetic_all_missing6.dss` Monolith returns -2 for the RTS, RTS-with-quality, RTD,
  -901/-902 and all -902 blocks and reads the data block and the -FLT_MAX block.
- **Opt-in refusal (maintainer decision 2026-10-29).** `no_data="refuse"` (Python
  `read_record`, `read_record_arrays`, `read_timeseries`, `read_arrays`,
  `ras_commander_fast_dss.read_timeseries`; Rust `TsReadOptions::no_data = NoDataPolicy::Refuse`,
  also inside `DatasetOptions::read`) applies HEC's rule; the default `"return"` /
  `NoDataPolicy::Return` keeps the behavior above. Rules, all from hec-dss fork tag
  `7-IU-15`:
  1. **Scope.** Regular records of type 100 and 105 only (`zrrtsi.f` line 290 accepts no
     other type), not patterns (the `TS-` branch, lines 165-185, returns before line 618),
     not irregular data (`zritsi6.f` has no such status), not DSS7 (above). The catalog
     is unchanged. Catalog-index reads: Rust `Dss6File::read_ts_entry` honours the
     option it is given (pinned by `read_ts_entry_follows_the_option`); the Python catalog-index
     route `DssFile.read_record_at` has no `no_data` keyword and always reads with
     `NoDataPolicy::Return`.
  2. **The test.** No value read is other than -901 or -902 (`zrrtsi.f` lines 618-649:
     `EQ.-901.0`, `EQ.-902.0`; floats line 631, doubles line 645). -FLT_MAX and NaN are
     values. Rust `all_hec_no_data`.
  3. **Records.** A record read with the option on raises `DssNoDataError` (Rust
     `TsError::NoData { pathname, status: 4 }`) when its block passes the test.
  4. **Datasets: the request, not each block.** `ZRRTSI6` reads every block of the window
     (lines 255-614) and applies the test once, to all values of the request (lines
     617-651), absent-block -902 fills included. So an all-missing block inside a window
     that holds data is **found**, as without the option: it counts in `blocks_found`, its
     slots are its stored values, and it supplies the units, type, offset and user header
     when it is the last block found (lines 340-342 read the internal header, lines
     345-350 the user header, and lines 575-603 decode the internal header, for every
     block whose `zrdinf6` succeeds, line 288). Only when
     the whole window passes the test does the read raise `DssNoDataError`, whatever
     `trim_missing` is. When no block of the window exists the read raises the plain
     not-found error (`ISTAT = 5`, lines 652-658); this includes a window whose only
     stored block lies in the engine's one-period lookup padding. Treating each
     all-missing block as absent was considered; the source does not, and Monolith agrees.
  5. **Engine-only paths.** `catalog_blocks` datasets and Python one-sided windows apply
     the same test to the values of the request before the trim.
  6. **Status mapping.** `ztsRetrieveReg6.c` lines 174-182 return early only for status
     above 4; for 4 it copies values, units and user header and returns -1 at lines
     255-259. Monolith returns -2 with no values, so nothing of the refused read is
     exposed. `DssNoDataError` subclasses `DssPathnameNotFoundError` (Monolith's -2 is
     "not found"); its `status` is HEC's `ISTAT` 4.

  Verified against Monolith 3.3.27 on fresh copies (hashes unchanged): with
  the option on, the engine's raise/return outcome equals Monolith's -2/0 on every regular
  record of seven private test files (30,028 records; 891 refused) and both DSS6 fixtures
  (patterns and irregular records included), and on every whole-dataset and window read
  compared through `HecTimeSeries.read` except one trim difference that did not depend
  on the option, resolved by the trim rule (next item).
- **The dataset trim (maintainer decision 2026-10-29, "Match HEC behavior").**
  `trim_missing` (Rust `DatasetOptions::trim_missing`, `dataset.rs` `trim_missing`;
  Python `trim_missing=True`, the default) removes leading and trailing values for which
  HEC's `zisMissingFloat` / `zisMissingDouble` is true, as `ztsTrim` does
  (`heclib_c/src/Internal/ztsTrim.c` lines 83-170, hec-dss fork tag `7-IU-15`): -FLT_MAX,
  -901 and -902 (`Utilities/zmissingFlag.c` lines 115-145; `UNDEFINED_FLOAT = -FLT_MAX`
  and `UNDEFINED_DOUBLE = -(double)FLT_MAX`, `headers/heclib.h` lines 21-22, so the
  double flag is the float -FLT_MAX widened, which is the engine's
  `-f64::from(f32::MAX)`). **A stored NaN is a value** and is kept, also as the first or
  last value. Rust `is_hec_missing` is this test; `is_missing` (which also matches NaN)
  stays the marker-to-NaN mapping. `ztsRetrieve.c` lines 618-626 call `ztsTrim` for
  regular data only, so irregular datasets are not trimmed (the engine never trimmed
  them). The same test is used by the DSS7 trim (`dss7/ts.rs` `trim`), by the Python
  one-sided-window and pattern helper `_trim_missing` and by the profile helper
  `_profile_slice`. The `no_data` test above (only -901/-902) and `trim_missing=False`
  are unchanged. Verified against Monolith 3.3.27 `HecTimeSeries.read` on fresh copies:
  it returns the single NaN of a block of -901 and one NaN, and keeps
  leading and trailing NaN while trimming the -901/-902/-FLT_MAX around them (float and
  double blocks, across a trimmed block, windows); values, times and NaN positions equal
  the engine's on 1,464/1,464 whole datasets of private test files (none of which stores a NaN).
- Quality flags are 32-bit integers. HEC may treat a protection bit in them
  (`heclib_f/src/ztstqp6.f`, `LQPBIT`) **[UNVERIFIED bit semantics]**.
  HEC writes and Monolith reads back flags with the sign bit and other high bits
  set (`0x80000000`, `0x7fffffff`, -1, `0x40000011`) unchanged on types 100, 105, 110 and
  115 (`synthetic_hec_variants6.dss`); the engine returns the same words.
- DSS6 has no notes. The DSS6 store routines take no notes argument (`zsrtsc6.f`,
  `zsitsi6.f`), and a Monolith `HecTimeSeries.write` of a container with `cnotes` and
  `inotes` into a DSS6 file stores the values and drops the notes (read back as null).
- The DSS7-era vertical-datum metadata sits in the user header as key/value text
  (`zrrtsi.f` lines 351-366, `Support/get_user_header_param.f`); §14 covers the user
  header text.

### 7.5 HEC's whole-dataset range (`ztsends6`)

**[VERIFIED-CORPUS]** A whole-dataset read through Monolith
(`HecDss.get(path, true)`, the call `RasDss` uses) gets its time range from `ztsends6`:

- The range runs from the catalog's first to last D part.
- When an end block is all missing, HEC steps at most 200 blocks inward and stops when it
  reaches the other end, which yields "no data" (`ztsends.f` lines 61-76). Two datasets
  with an all-missing first block followed by a long gap returned nothing in Monolith; the
  `dss-core` reader returns their 145 and 577 valid values.
- `ztsinfo6` excludes `-FLT_MAX` only when finding the first valid value. It also starts
  counting weekly data from the unaligned D date (`ztsinfo6.f` lines 123-126 and 173), so
  Monolith's first weekly stamp is 6 days late and it drops the first value (our reader
  returns all 150).
- Windowed reads follow `zrrtsx6`: every slot in the window is kept and absent blocks are
  filled with -902. Monolith's `HecDss.get(path, start, end)` additionally trims leading
  and trailing missing values, and narrows type-105 windowed reads to `f32`
  (`ztsRetrieveReg6.c` lines 106-111).
- The `dss-core` reader is bounded: dataset reads are capped at 50 M values and 5 M block
  lookups (`TooLarge`).

A `catalog_blocks` mode that reads the blocks the catalog holds, at any D part, exists for
files whose blocks do not follow HEC's block convention; it is tested on synthetic files
only.

---

## 8. Data compression (regular TS, type 100)

A reader must decode this. Sources: `heclib_f/src/Support/dureal.f:DUREAL`,
`dhinfo.f:DHINFO`, `dhchek.f:DHCHEK`, `durept.f:DUREPT`, `dudltr.f:DUDLTR`,
`dusd3r.f:DUSD3R`, and `sd32r.f:SD32R`. The writer side is in `dhinit.f:DHINIT`,
`dcreal.f:DCREAL`, `dcrept.f:DCREPT`, `dcdltr.f:DCDLTR`, and `dcsd3r.f`. Decoding is
triggered in `zrrtsb6.f:ZRRTSB6` lines 76-121.

**Scheme number** `ISKM` = `KICOMP` (also compression header byte 4) is a bit mask:
1 = repeat, 2 = "delta" (offset-from-base quantization), 4 = 3-significant-digit
packing. The valid values are 1..5, and 2 and 4 together are invalid (`dhinfo.f`
lines 59-105). So 1 is repeat only, 2 is delta, 3 is repeat + delta, 4 is sig-digits,
and 5 is repeat + sig-digits.

**Compression header** (`KINCHE` words at `KIACHE`, bytes 1-based; created
`dhinit.f:DHINIT` lines 35-157, repeat section finished `dcreal.f:DCREAL` lines
145-193; validated `dhchek.f:DHCHEK` lines 56-123):

| Byte | Content |
|---|---|
| 1 | `C` |
| 2 | section version, 16 (required, `DHCHEK` -1602) |
| 3 | section length 8 |
| 4 | `ISKM` |
| 5-8 | `NELMS` as an `INTEGER*4` (copied byte by byte, so little-endian) = number of uncompressed values |
| 9 | `D` |
| 10 | 16 |
| 11 | section length 13 |
| 12 | `ISIZE`: bytes per packed value (1 or 2) |
| 13 | `IPREC + 10` (decimal exponent of the quantum) |
| 14 | `LBASE` flag (1 = caller supplied the base) |
| 15 | `NBASE` = 4 |
| 16-19 | `BASE` as a `REAL*4`. The delta compressor stores the computed minimum here when `LBASE = 0` (`dcreal.f` lines 252-265) |
| 22 | `S` |
| 23 | 16 |
| 24 | section length 4 |
| 25 | `ISIZE` for sig-digits: meant to be 2, but HEC writes the caller's stale argument (0, or 2 after a delta-high write) and its reader ignores it |
| 26 | `R` |
| 27 | 16 |
| 28 | section length, low byte |
| 29 | `NBITS` = `NELMS mod 32` (active bits in the last flag word) |
| 30 | section length, high byte (length = 256×b30 + b28 = 8 + 4×NW) |
| 31-32 | 0 |
| 33 .. 32+4×NW | repeat flag bits, `NW = ceil(NELMS/32)` words (present only if repeat is on) |
| 34+4×NW (34 with no repeat) | `E` |

Header length is 9 words without repeat and `9 + NW` with repeat (`dcreal.f` line 193).
Readers should check that the section letters fall at the positions `DHCHEK` derives
(`C` at 1, then each section start plus its length). Verified on `green_river.dss`
(6-ND, schemes 1 and 3): `NELMS` = `KILNDA` (365 and 744), and `E` sits at byte
`34 + 4×NW`.

**Decode procedure:**

1. `NR2` = number of stored values. With repeat it is the count of `i ∈ 1..NELMS`
   whose flag bit `i` is 0 (`dhinfo.f` lines 136-148). Bit 1 is always 0
   (`dcrept.f:DCREPT` line 59). Without repeat, `NR2 = NELMS`.
2. Unpack `NR2` values from the data area (`KINDAT` words, read as a byte stream from
   byte 1):
   - **Delta (2):** value `v` = unsigned little-endian integer of `ISIZE` bytes, the
     `i`-th starting at byte `(i-1)*ISIZE + 1`. If `v = 2^(8*ISIZE) - 1`, the value is
     `-901.0`. Otherwise it is `BASE + v * 10^IPREC`, computed in `REAL*4` arithmetic
     (`dudltr.f:DUDLTR` lines 121-144) **[VERIFIED-CORPUS; §12 O1 closed]**: the quantum `10^IPREC` is rounded once to `f32`, with no fused
     multiply-add. All 39 compressed records in the private test files are bit-exact (2 scheme 1, 3 scheme 2,
     34 scheme 3). The delta value size must be 1-4 bytes in the decoder
     **[RESOLVED]**: HEC writes only sizes 1 or 2 (`dcdltr.f` lines 195–215;
     size 3 is commented out), which is what the header table states. HEC's decoder accepts
     1–6 (`dudltr.f` lines 35–45), but sizes 5–6 overflow its 32-bit work word, so the
     native reader accepts 1–4. A writer emits only 1 or 2. The
     encoder rounds to `10^IPREC` and maps both -901 and -902 to all-ones
     (`dcdltr.f:DCDLTR` lines 93-120 and 280-306). Data length =
     `ceil(NR2*ISIZE/4)` words.
   - **Sig-digits (4):** each value is a 2-byte little-endian code `c`
     (`dusd3r.f:DUSD3R` lines 110-130; `sd32r.f:SD32R`). `c = 1001` → 0.0. If
     `c > 1023`: `e = c div 1024`, negative if `e > 32` (then `e -= 32`),
     `mant = c mod 1024`, and the value is `±mant × 10^(e-15)`. Codes 1002/1003 →
     ±1.0e-37, and 1004/1005 → ±1.0e+37. Any other code below 1024 leaves the value
     undefined **[UNVERIFIED; reject]**. Data length = `ceil(NR2*2/4)` words.
     **[VERIFIED-HEC-WRITTEN]**: 2,939 values in 7 scheme 4/5 records written by
     Monolith 3.3.27 (`Heclib.zsrtsx`, `JCOMP` 4/5) are bit-equal to heclib's `zrrtsx`
     decode, over every exponent the encoder keeps (`10^(e-15)` for `e` 1..31), both
     signs, the reserved codes, mantissas of two digits and mantissas that round to 1000.
     The encoder (`r2sd3.f:R2SD3`) changes some values: `IP = INT(LOG10|R|) - 2` truncates
     toward zero, so values below 1 keep two digits (0.012345 → 0.012); |R| outside
     about 1e-13 .. 1e19 becomes ±1.0e-37 or ±1.0e+37; -FLT_MAX (HEC's UNDEFINED) is
     stored as 1005 and reads back as -1.0e+37 (no longer a missing marker); -901/-902 keep
     their values; a NaN is stored as 1002 and reads back as 1.0e-37.
   - **Repeat only (1):** `NR2` raw `REAL*4` words.
3. **Repeat expansion** (`durept.f:DUREPT` lines 116-132): `out[1] = v[1]`. For `i = 2
   .. NELMS`, if flag bit `i` is 1 then `out[i] = out[i-1]`, otherwise `out[i] = next
   v`.
4. Require `len(out) = NELMS = KILNDA`.

`dureal.f` decodes delta or sig-digits **in place** and then applies repeat
(`dureal.f` lines 96-186). The result is the same as the procedure above. Of the
locally available files, only one uses compression (`green_river.dss`, 30 records,
schemes 1 and 3). Later corpus work decoded scheme 2 (3 records) and scheme 3 (6
records) in a public `6-LA` file (`sample_vb.dss`), and only schemes 1, 2 and 3 occur; the
sig-digit schemes 4/5 were not observed in any corpus file (§12 O2). HEC reads at most
`KDCH = 300` compression-header words (`zdssdc.h`).

**Which HEC writers compress (hec-dss fork tag `7-IU-15`).** `zsrtsi6.f` lines 588-598
pick the scheme: an explicit `JCOMP > 0` from `zsrtsx`/`zsrtsxd`/`zsrtsi`; with `JCOMP = 0`
the stored record's scheme, else the file default set by `zsetci6` for matching pathname
parts. `zsrtsc6.f` line 31 sets `JCOMP = 0`, so `zsrtsc` (Monolith `HecTimeSeries.write`)
only compresses through a file default or an existing compressed record; Monolith exposes
no `zsetci`, but `Heclib.zsrtsx` passes `JCOMP` through (`javaHeclib/Hec_zsrtsx.c`).
Compression is turned off for quality (`zsrtsi6.f` line 602), doubles (line 605), a block
larger than `KSBUFF` (lines 608-613) or a result no shorter than the data (lines 852-861);
schemes 3/5 fall back to 1 when the combination fails (lines 846-851). The public fixture
`synthetic_hec_compress6.dss` holds all five schemes, a quality and a double record that
asked for scheme 4 (stored uncompressed), and a scheme 4 block partly rewritten with
`JCOMP = 0` (still scheme 4).

**Default.** The reader decodes schemes 4/5 by default (`TsReadOptions.allow_sig_digits
= true`); `false` refuses them as before. The S-section size byte (25) is not checked:
`dhinit.f` line 130 stores the `ISIZE` argument, which `zsrtsi6.f` lines 825-832 set only
for schemes 2/3, and `dureal.f` line 66 (`IBBYTE = IBGELM*ISIZE - ISIZE + 1`, 1 for the
`IBGELM = 1` of `zrrtsb6.f` lines 93/104) with `dusd3r.f`'s fixed 2-byte codes never uses it.

---

## 9. Locking and write ordering

This section supports "refuse to write to a file locked by HEC software". These are
the facts lock detection needs.

### 9.1 HEC's lock protocol

Locks are byte-range locks on the **lock record** at byte `KLOCKB = (lock_record-1)*512`
(§4.4), with 4-byte lock words (`IWORD(2) = 4`):

| Byte range | Purpose | Source |
|---|---|---|
| `KLOCKB+0 .. +4` | **write lock**: held while writing; in advisory mode 3 it is held from the first write until close | `zmultu6.f:ZMULTU6` lines 97-115, 199, 265 |
| `KLOCKB+4 .. +8` | **lock request**: another process asks the holder to release; the holder drops to mode 2 | `zmultu6.f` lines 118-158, 228-239; `heclib_f/src/zckmul6.f:ZCKMUL6` lines 35-49 |
| `KLOCKB+8 .. +12` | **exclusive**: tested at open; if held, the open fails with `-5` "currently exclusively locked". Exclusive mode locks all 12 bytes | `zopen6Int.f:ZOPEN6INT` lines 289-301; `zmultu6.f` lines 98-110, 282-283 |
| `KLOCKB+16+4k`, k = 0..99 | **read / in-use slots**: every opener locks the first free slot at the end of `zopen6`; squeeze uses them | `heclib_f/src/zlockrd.f:ZLOCKRD` lines 51-87; `zopen6Int.f` line 603 |

- Open modes (`KMULT`): 0 exclusive, 1 read-only, 2 multi-user, **3 advisory
  (default)**, 4 exclusive-write (`zopen6Int.f` lines 107-129).
- Implementation (`heclib_c/src/Dss6/lockdss.c:lockdss_`): Windows uses `_locking`
  (mandatory region locks; `_LK_NBLCK` to try, `_LK_LOCK` to wait), lines 233-272.
  POSIX uses `lockf` (`F_TLOCK`, `F_TEST`, `F_LOCK`, `F_ULOCK`), lines 287-327. Modes:
  0 unlock, 1 lock-wait, 2 lock-nowait, 3 test.
- On Windows the locks are **mandatory**: reading locked bytes fails with errno 13.
  HEC then reads the lock record from offset +12 onward (`zgtrec6.f` lines 141-157). A
  reader that never touches the lock record is unaffected. Normal records do not live
  there (§4.4), but pre-`6-K` files can hold live bins in it (verified on a `6-EA` file;
  §12 O6).
- `KFSIZE = -1357` on disk while a program holds `zwlock6 'ON'`, or after it crashed in
  that state (§1.3).

Proposed detection rule **[PROPOSED]**: before any write,
refuse (`DssLockedError`) if `KFSIZE = -1357`, or if a non-blocking lock attempt on
the write word or the exclusive word fails. Also treat any read slot held by another
process (a test lock on `KLOCKB+16+4k` fails) as "open in HEC software" and refuse,
because HEC readers cache the header and bins (`zcheck6.f` lines 99-139 reuse the last
lookup). Reads never take locks.

### 9.2 HEC write ordering (for reference; our writer does not copy it)

- `zwritex6`: lock and re-read the header (`zmultu6(LOCK)`), look up the pathname,
  update the bin, info block, and header in buffers (`znwrit6` or `zowrit6`), buffer
  the headers and data, then unlock and flush (`heclib_f/src/zwritex6.f:ZWRITEX6`
  lines 62-163).
- **New record:** allocated at `KFSIZE` (info, then headers, then data), the bin entry
  written, `KNRECS + 1`, and the header rewritten (`znwrit6.f` lines 79-216).
- **Rewrite, same or smaller size:** **overwritten in place**, with `KIUNUS` recording
  the slack (`zowrit6.f` lines 96-127, 164-173). This is not crash-safe.
- **Rewrite, larger:** the old info block gets status -1, `KDEAD += old size + info
  size`, a new copy goes to EOF, and the bin entry is repointed (`zowrit6.f` lines
  66-95). `KIVER += 1` (capped at 999, lines 180-181).
- **Delete:** bin and info status become 2/12, `KNRECS - 1`, and `KDEAD +=` the whole
  record size (`zdelet6.f` lines 50-102). **Rename:** the old entry becomes 3/13 and a
  new entry is added (`heclib_f/src/zrenam6.f`). **Undelete:** status - 1 (`zundel6.f`
  lines 54-86).
- **Flush order** (`heclib_f/src/zbdump6.f:ZBDUMP6` lines 44-180): first new
  end-of-file records in ascending order, then the root (header) record, then the
  other dirty records in reverse ("so that if a crash occurs ... no pointers will be
  written pointing to a spot beyond the physical end of the file"). There are only 6
  record buffers (`zdssbz.h` `MXBUFF = 6`), so flushes also happen mid-write. HEC
  ordering is best-effort, not transactional.
- **Our rule** (data first, pointers last, never overwrite live data in place) yields
  files with the same layout. HEC readers rely only on the final pointers. The native ordering is §9.3.

### 9.3 Native write ordering

The `dss_core::dss6::write` writer (`crates/dss-core/src/dss6/write/`) never
overwrites a live record in place. Each time-series block is one transaction, applied in
this order (`DSS6_STEP_ORDER`; every step is a fault-injection point):

| Step | Writes | Why here |
|---|---|---|
| Data | the info block, internal header and data at `KFSIZE`, and a new zeroed 32-bin block first when `KBNREM` is 0 (on a record boundary, `znwbin6.f` lines 38-50) | nothing points past `KFSIZE` yet |
| EndOfFile | -9630 at the new `KFSIZE`, zeros to the end of its record | `zptrec6.f` lines 197-210 |
| Allocation | header: new `KFSIZE` and the **in-progress marker** in `KRES` words 67/68 (0-based header words; old and new info addresses) | HEC now allocates past the new space |
| Chain | (new bin block) word 127 of the previous block's last bin → the new block | after Allocation, so no block is chained past `KFSIZE` |
| Counters | every header counter and the bin allocation state (`KNRECS`, `KSEQNO`, `KHUSED`, `KBINS`, `KBOVER`, `KMAXPH`, `KMXPRT`, `KDEAD`, `KBNREM`, `KANBIN`), marker still set | before Link, so HEC's `znwbin6` never hands out a bin a slot will point at; `KNRECS` one high still lists every record in HEC catalogs (they stop after `KNRECS` live entries, `zcatfi6.f` line 196), one low would hide a record; HEC's file checks report one high as an inconsistency (`zckpnb6.f` line 321, `zcklnk6.f` line 167, `zckpat6.f` line 173) until the next native write reconciles it |
| Link | (new bin) the table slot `NPERM + IHASH`, or the full bin's `-1` status and word 126 in one write | the new bin is still empty: in DSS6 an empty bin is valid (a lookup ends at status 0 and HEC fills it next, `zcheck6.f` line 252, `znwrit6.f` lines 93-104) |
| Publish | the whole bin entry, status word included, in one write (an entry lies inside one 127-word bin = one 512-byte record); for a rewrite the entry's `KBAINF..KBTYPE` in one write | the record becomes visible |
| Retire | (rewrite) the old info block's status → -1, as `zowrit6.f` line 77 marks a relocated block | HEC never tests info status on read (`zrdinf6.f` lines 57-62), so order relative to Publish only matters for tidiness |
| Finish | the marker cleared | |

**The marker.** `KRES` is "unused area - 14 words" (`zinit6.f` line 100; lines 566-569).
HEC reads and writes the permanent section whole (`zrdprm6.f` line 51; `znwrit6.f` line
216), so it keeps the marker: verified, HEC Monolith 3.3.27 writing to 36 interrupted
files left words 67/68 as they were. The words are 0 in all 34 private DSS6
test copies (`6-JE` to `6-YO`).

**Reconciliation.** The next native write on a file whose marker is set retires the old
copy of a rewrite once the live entry of its pathname names another info block (the
published native copy, or a copy HEC wrote after the interruption), after checking that
the old block names the same pathname; it rebuilds `KNRECS` (live entries of the bin walk),
`KHUSED` (nonzero table slots), `KBOVER` (bins with an overflow pointer) and `KBINS`
(bins handed out in the blocks up to the one holding `KANBIN`). It never clears a bin or
moves a pointer: an empty linked bin, an orphan record copy or an unused chained bin
block (left by a crash between Chain and Counters) is valid DSS6 and is reused or
ignored by HEC and by the writer.

**When HEC writes first after an interrupted native write** (crash tests with
Monolith): every store succeeds and every record reads; until the next native write the
counters can be one off (`KNRECS` one high after Counters/Link of a new record, `KHUSED`
or `KBOVER` one high after Counters of a new bin). Reads and writes are unaffected, but
HEC's check routines (`zckpnb6`, `zcklnk6`, `zckpat6`) count the `KNRECS` window as an
error until the next native write. The crash model is process death. The flush policy
is `WriteOptions.sync` (Python `sync=`), the same three modes as for DSS7 (`dss7.md`).
`SyncMode::End`, the default since 0.1.2, calls `sync_all`
once at the end of the write call and nowhere else. `SyncMode::Step`, the default up to
0.1.1, also flushes at the transaction's flush points: the record area together with the
EOF flag (the Data step has no flush of its own), the allocation header, the chain word,
the counters, the link, the publication, the retirement and the cleared marker; a step
with nothing to write flushes nothing. `SyncMode::None` never flushes. All three
make the same writes in the same order and set the same marker, so the killed-process
matrix finds the same states in each. Every step that must be atomic is one `write`
inside one 512-byte record. No power-loss claim is made in any mode.

**Differences from HEC's stores** (deliberate): relocation instead of in-place rewrites
(`KIUNUS` 0, `KDEAD` by HEC's relocation formula, `zowrit6.f` line 82); `KILNDA`/`KBLNDA`
current after an irregular rewrite (HEC's in-place rewrite leaves them stale, `zowrit6.f`
lines 155-160 run only when the record moves); equal rewrites skipped; compressed blocks
rewritten uncompressed; a rewrite keeps the stored time-zone, coordinate and `KIPREC`
words; irregular blocks keep a stored double width; program `DSSCMD`, UTC dates, NUL
eighth bytes in date and program fields (HEC leaves buffer bytes there).

**HEC defect seen (7-IU, Monolith 3.3.27; recorded here only).** A store that changes
the width of an existing irregular block damages it. `LFILDOB` (the stored width) is set
once (`zsitsi6.f` lines 545/547); the pairs are converted to the new width (loops
150-180, lines 550-583), but `IMULT` is computed from the stored width
(`IF (LFILDOB) IMULT = IMULT + 1`, lines 615-616) and the type from the new one.
Doubles into a float block are written two words per pair as type 115 (Monolith reads
one value back; found with a synthetic record written by Monolith); floats into a
double block are written three words per pair as type 110 (Monolith reads three values
with garbage bits). The native writer keeps or widens to double.

**Also in the native writer**: a write is built and validated in full before
the first block is stored, so a refused write changes nothing; files with a record-tag
scheme (`KTAGS` > 0) are refused; `KLWDAT`/`KLWTIM` are stamped as `zptrec6.f` lines
279-283 stamp them.

**Paired data.** `zpdStore6.c` → `zspdi6.f:ZSPDI6` (`IPLAN` 0) → `zwritex6`; the
native store is one record transaction in this order. Internal header words 1-3 `NORD`,
`NCURVE`, `IHORIZ` (1 or 2), 4-11 the four 8-character strings, `NIHEAD` 11 or, with
labels, `3·NCURVE+11` (12-character short labels) plus `NWDS` and the long labels when a
label is longer than 12 characters; data the ordinates then the curves; `KILNDA = NDATA`;
`KIPREC` 10x+y or −1. `zpdStore6()` stores −1 unless `xprecision > 0` (lines 165-171), and
Monolith 3.3.27 reads a stored 3 as `(3, 3)` and a stored 0 as `(-1, -1)`, where the C
rule (`zpdRetrieve6.c` lines 129-136) gives `(0, 3)` and `(0, 0)`
(found by writing both values with Monolith and reading them back); the native writer refuses an x precision of 0. Monolith's `PairedDataContainer` path always passes doubles
(`Hec_zpdStore.c`), and `zpdStore6()` picks the width from the array type, so
`HecPairedData` writes DSS6 paired data as PDD (205) even for a float request; the float
path is `Heclib.Hec_zspd` (`ZSPDI6` with REAL*4). Writer:
`crates/dss-core/src/dss6/write/pd.rs`.

### 9.4 Regular merge and extend into existing blocks

DSS6 regular time-series writes merge into and extend existing records as HEC does with
its default store flag. The native rule is a port of
`zsrtsi6.f:ZSRTSI6` with `IPLAN = 0` ("Always replace data", lines 25-35) and
`zmovbk6.f:ZMOVBK6` with `IFLAG = 1` (hec-dss tag `7-IU-15`):

- **Window.** The write's window runs from the cleaned start, moved to its standard
  boundary with the offset kept (`zofset6.f`, `zsrtsi6.f` lines 447-457), for `NVALS`
  periods (line 459). Blocks are visited from the block holding the start to the block
  holding the end (`zbegdt6.f`, `zincbk6.f`; weekly blocks Sunday-aligned, lines 517-524).
- **Each block.** An absent block is created full (`NDATA = NOPERS(block, next block)`,
  line 677) with every unwritten slot -901 (lines 699-727). An existing block is read and
  the new values replace the stored ones at positions `NPOS..NPOS+NSTORE-1`
  (`zmovbk6.f` lines 60-130); every other position keeps its stored value bit for bit.
  Values are stored as given: a -901, -902, NaN or -FLT_MAX in the input replaces a
  valid stored value (IPLAN 0 never tests for missing; `IFLAG` -1 and -4 do, for IPLAN 1
  and 4, which are not offered). A block that ends up all missing is still written (only
  IPLAN 3 deletes, lines 911 and 1021).
- **Width and quality** (lines 728-808): double wins (a float block written with doubles
  is widened with `DBLE`, line 768; floats into a double block are widened, `zmovbk6.f`
  loop 140); quality is kept (written slots without quality get 0, `zmovbk6.f` lines
  183 and 217) or added to a float block (loop 310); the other combinations are refused with
  HEC's status 511.
- **Metadata.** Units, type and the offset word are the write's (`IIHEAD(1) = IOFSET`,
  line 871): a merge at another offset relabels the stored values without converting
  them, as HEC does. The native writer keeps the stored time-zone, coordinate and
  `KIPREC` words.
- **Crash ordering.** Each block is one §9.3 transaction; a merge that spans blocks is
  atomic per block, never across blocks, as HEC's own store is.

**Verification.** `fixtures/public/p66_hec_ts6_merge_ops.tsv` (60 stores: overlap at
the head, the tail, inside and covering one block; extension before, after and on both
sides across block boundaries for 1MIN to 1YEAR; offsets; missing values both ways; every
width combination; quality kept, added and double; compressed stored blocks) was run by
HEC Monolith 3.3.27 (`p66_hec_ts6_merge.dss`) and replayed natively
(`crates/dss-core/tests/dss6_merge.rs`): 42 of 44 records are word-identical (bin entry,
info block apart from addresses and stamps, internal header, data); the two records HEC
stores compressed decode equal (scheme 1) or differ only as below (scheme 3). A
replacement-only model of the rule above predicts every value and quality flag of the
native file, and of HEC's file except its delta-compressed (scheme 3) record, which that
model does not cover. HEC's re-quantization is checked separately. In this file
(`hec_requantizes_a_merged_delta_block`), HEC's scheme-3 values up to the end of the
written window are the native values rounded half away from zero to the caller's precision
(`NPREC` 0), and the values after it are equal. In the same-block merges below
(`same_block_compressed_merges`), each value HEC stored is within one unit in the last
place of the native value re-quantized to the caller's precision. The DSS6 C library of
hec-dss 7-IV-1 (`ztsStore` → `zsrtsi6`, storage flag 0) made the same merges for the 38
records without quality or compression: equal to the native result. Monolith and the C
library read every native result as they read HEC's.

**Merges into the same HEC-compressed block.** `p66_hec_ts6_comp_base.dss`
holds five blocks HEC stored compressed: scheme 1 (repeats), and scheme 3 (repeats and
deltas) at `NPREC` -2. Identical copies were merged by HEC Monolith 3.3.27 (`zsrtsx`,
`p66_hec_ts6_comp_merged.dss`), by the HEC 7-IV-1 C library (`ztsStore`,
`p66_c_ts6_comp_merged.dss`) and by the native writer (`dss6_merge.rs`,
`same_block_compressed_merges`). Monolith and the C library read all four files as the
native reader does.

- **Scheme 1:** both HEC paths and the native writer agree in every value of the block.
  The internal header and the info block are word-identical, apart from addresses, stamps
  and the three words of the stored representation (`KINDAT`, `KICOMP`, `KINCHE`). HEC
  keeps the block compressed; the native writer stores it uncompressed.
- **Scheme 3: a documented difference from HEC, accepted by the maintainer.** The
  native writer stores the input values in the written window and leaves every stored
  value outside it exactly as stored. HEC re-quantizes the whole block to the caller's
  precision, written values included, so some values inside and outside the window differ
  from the native result. That whole-block re-quantization is the accepted
  exception.
  - *What HEC does.* `zsrtsi6` keeps the stored scheme (`IACOMP = IRCOMP`, line 594), but
    compresses the merged block with the caller's `NPREC` and base, not the stored ones.
    Line 659 reads: "NEED TO GET OTHER COMPRESSION INFORMATION HERE (EG BASE NPREC)".
  - *The whole block is re-quantized.* Every value of the block, outside the written
    window as well as inside it, is quantized to `10^NPREC` of the caller. A value already
    on that grid often keeps its stored value, and a missing value is preserved, so many
    positions are unchanged (see *Observed*). Delta reconstruction can still move an
    on-grid value by one unit in the last place. The model is: scale in single precision, round half away
    from zero, scale back. HEC's values are within one unit in the last place of that model
    at every position.
  - *Which precision applies.* The caller's precision depends on the store path:
    - `zsrtsx` passes the caller's `NPREC`. The Fortran default is 0.
    - The C API (`ztsStore` → `zsrtsc6`) always passes `NPREC` -1 (`zsrtsc6.f` sets it).
      The same Fortran routine serves HEC's Java store path.
  - *Observed.* Stored at -2 (hundredths), with positions 26-35 written:
    - At caller `NPREC` 0, stored 7.5 becomes 8.0 and stored 15.45 becomes 15.0, outside
      the window. The written values become whole numbers.
    - At caller `NPREC` -1, as both the C library and `zsrtsx` with -1 store it, stored
      15.45, 20.02, 27.12 and 31.13 outside the window become 15.5, 20.0, 27.1 and 31.1.
      The written values become tenths.
    - At caller `NPREC` -2, the stored precision, with two-decimal input, stored values
      are unchanged. Two of the ten written values differ from the input by one unit in
      the last place, from HEC's delta reconstruction.
  - *What the native writer does.* It decodes the stored block (§8) and changes only the
    written positions: values outside the window are bit-identical to the decoded starting
    block. It stores the block uncompressed and does not quantize the
    input. This difference is accepted.
- **Rarity.** DSS6 compression is rare in the private test corpus: schemes 1-3, sampled
  records in 4 files. A merge census of 34 HEC-written DSS6 test files found 37 compressed
  blocks in 2 files, one of them writable.

---

## 10. Pathname rules and limits

| Rule | Value | Source |
|---|---|---|
| Maximum pathname length | 392 characters (`dss_maxpath`, 6 × 64 + 7 slashes + 1), writes reject `npath ≤ 3` or `> 392`, and a first character below `/` | `heclib_f/src/headers/dss_parameters.h`; `zwritex6.f` lines 46-49 |
| Lister limit | entries with `npath > 390` are skipped by `zplist6` | `zplist6.f` line 149 |
| Bin fit | `P + 11 + 2 ≤ KBNSIZ` (≤ 456 characters with 127-word bins) | `zcheck6.f` lines 57-64 |
| Case | stored upper case; characters below 32 become `?` | `zcheck6.f` lines 70-82 |
| Native writer: printable ASCII only | a pathname byte below 32 or above 126 is refused before anything is written (DSS6 and DSS7 writers). HEC accepts such a pathname and stores it changed, so a native write cannot add to an existing record whose pathname has such bytes; reads are unaffected | `crates/dss-core/src/dss6/write/path.rs`, `crates/dss-core/src/dss7/write/path.rs` |
| Long path flag | status 11 if `npath > 80` or any part > 32 characters | `znwrit6.f` lines 40-43, 60-72 |
| Part maximums | header `KMXPRT` bytes 1..6 (catalog display treats < 4 as 4, and D/E < 6 as 6) | `znwrit6.f` lines 59-72; `zcat6.f` lines 102-110 |
| Pattern TS | D part starting `TS-` (types 101/111) has no dates; value count is `KINDAT`, not `KILNDA` (§7.3a) | `zrrtsi.f` lines 161-187 |
| Collections | F part `C:nnnnnn|...` (6 characters, then `|`) | `zcolpath6.f` |

---

## 11. DSS6 compared with DSS7 (for a shared reader architecture)

The DSS7 facts below come only from the key names and order in
`heclib_c/src/Internal/zinit.c` and `heclib_c/src/Internal/zhash.c`. The authoritative
description is `dss7.md`, and those facts are marked as such.

| Aspect | DSS6 (this document) | DSS7 (see `dss7.md`) |
|---|---|---|
| Word | 4 bytes; 127 words per 512-byte record with a 4-byte pad (§1.1) | 8-byte words; record padding not expected **[see dss7.md]** |
| Address | 1-based `INTEGER*4` word address (about 8 GB maximum) | 64-bit word address **[see dss7.md]** |
| Signature/version | `ZDSS` bytes 0-3; version at bytes 16-19 (word 4) | `ZDSS` bytes 0-3; version at bytes 16-19 (word 2 of 8 bytes) (`heclib_c/src/Public/zgetFileVersion.c`) |
| Header | fixed 69 words (§3) | variable, starting with a header-size word (`zinit.c` `kfileHeaderSize`), and including counts of aliases, renames, deletes, expansions, collections |
| Hash table | implicit at address 70; 1-based codes from `zhash6` | explicit start address (`kaddHashTableStart`); `zhash` returns a 0-based table hash **and** a 64-bit pathname hash stored in bins (`zhash.c` header comment) |
| Hash algorithm | Pabst bit-group XOR (§4.2) | the same family (the `zhash.c` header credits Pabst, 1979), plus a second "pathname hash" **[see dss7.md]** |
| Bin entry | status, length, path, info address, sizes, type, tag, hash, pseudo-interval (§4.5) | `zdssBinKeys`: hash, status, path length, info address, type+catalog sort, last write, dates, path (`zinit.c` around line 742) |
| Info block | flag -9753, 28 + P words (§5.1) | different layout and flag **[see dss7.md]** |
| Time | julian day + minutes (1..1440) | julian day + seconds with time granularity (SPEC §7) |
| Catalog | bin-block walk only; no stored sorted catalog; external `.dsc` files | bin walk plus an internal catalog-sort table (`kcatSortAddress`) and aliases |
| Free space | dead space only (`KDEAD`); reclaimed by squeeze | reclaim tables (`kreclaim*`) |
| Locking | byte ranges in the lock record (§9) | lock arrays and pid array addresses in the header (`klockWriteArrayAddress`, `kpidArrayAddress`) |
| Endianness | detected by `KTABLE > 10000` | explicit `kendian` header word |
| TS compression | repeat / delta / sig-digits (§8) | repeat compression (`heclib_c/src/Internal/ztsRegRepeatUncompress.c`) **[see dss7.md]** |

Implications for `dss-core`:

- Use a `WordSource` per version. The v6 implementation maps an address to a byte
  offset with pad skipping (§1.2) and returns logical `u32` words. Decode doubles and
  strings only from logical words.
- The shared catalog model should carry (pathname, record type, status, info address,
  pseudo interval). DSS6 fills it from §4.5; DSS7 from its bin keys.
- TS block math (block start, D part, value positions) is version-specific: DSS6 uses
  minutes, DSS7 uses seconds. Keep it in `v6/` and `v7/` and convert to the shared
  model at the boundary.
- Header validation, chain-cycle guards, and bounds checks belong in both
  implementations. DSS6 has no checksum.

---

## 12. Open items for oracle/corpus verification

Each item has a concrete check. "Corpus census" means a read-only pass over
the private test corpus.

| ID | Item | Check |
|---|---|---|
| O1 | **CLOSED.** Is the delta decode bit-exact (`REAL*4` `BASE + v*10.**IPREC`, where Fortran `10.**REAL(IPREC)` may differ from `powf`)? | Bit-exact on all 39 compressed records of the private test files (2 scheme 1, 3 scheme 2, 34 scheme 3); quantum rounded once to f32, no FMA (§8) |
| O2 | **Closed for HEC-written data.** Scheme 2 observed and bit-exact (3 records, `6-LA`). Schemes 4 and 5 never observed in the corpus; written by Monolith 3.3.27 (`Heclib.zsrtsx`) and bit-equal to heclib's decode on 2,939 values (§8). `SD32R` codes below 1024 other than 1001-1005 are never written by `R2SD3` and stay rejected | Census `KICOMP` histogram: schemes 1, 2, 3 only. Schemes 4/5 decode by default |
| O3 | Type-2 (`KTABLE = 2`) files: incidence, and `zplist6` (`nbin = KBNBLK`) versus `zcatfi6` (`nbin = KHASH`) | Census `KTABLE`. If any exist, compare the oracle catalog with both walks. If none, have the oracle create one (`zset 'STABLE'`) |
| O4 | Byte-swapped (big-endian) files | Census `KTABLE > 10000` and the `byte_order` column; if zero, reject with a typed error |
| O5 | `KBNSIZ` other than 127 in pre-`6-M` files (`6-JE`, `6-JF`, `6-KE`, `6-LD`) | Census `KBNSIZ` by version |
| O6 | **CLOSED: the lock record can hold live bins before `6-K`.** The `6-EA` file has `KAFBIN` inside it and HEC reads it normally; the loose `6-JE`/`6-JF` files have nothing pointing into it (§4.4) | Readers must not exclude the lock record for files before `6-K`; fixed in `dss6/mod.rs` |
| O7 | Short final record (508 bytes) | Census `size mod 512` by version; the reader must accept 0 or 508 |
| O8 | Date fields: `*******` creation dates (sample.dss, Minimum_Facility), 2-digit years, NUL eighth byte | Census non-`DDMMMYY` values; informational only |
| O9 | `KBLNDA` (bin) differs from `KILNDA` (info) in 148 records of a RasExamples `BaldEagle.dss`; PD `KILNDA` is stale. **PD part verified:** sample6 365 vs 62 stored words, castro 144 vs 20; stale in 10,161 of 403,000 sampled PD records; pattern `KILNDA` also stale | Oracle-read those records and confirm the reader sizes data from the info block and the PD header (PD: from `NORD`/`NCURVE`; patterns: `KINDAT`) |
| O10 | `KIPREC` values such as 21 and 54 in old HMS files | Informational; confirm the oracle ignores them |
| O11 | `KNRECS` versus live entries | Census: full walk count against `KNRECS`; list mismatches (0 of 34 locally) |
| O12 | Collection hash mapping (`XXXXXX`, `strcpy` padding). The source reading is confirmed (§4.1 step 3), but the corpus survey found no collections | Find files with `KCOLL = 1`; verify `zhash6` on the mapped path equals `KBHASH` |
| O13 | Double word order in files written on big-endian hosts before `6-O` | Only relevant if O4 finds swapped files |
| O14 | **CLOSED.** RTS internal header word 6 / ITS word 10 (time-zone offset): minutes, west negative (-480 for `GMT-08:00` and `PST8PDT`; 612 records in 2 corpus files) | RTS verified against Monolith; ITS word 10 not separately evidenced (§5.2) |
| O15 | Pseudo-regular display (`KBPINTL > 0`, `~` E part). `~5MIN`, `~1HOUR`, `~1WEEK` and `~1YEAR` written by HEC (stored `IR-DAY`, `IR-MONTH`, `IR-DECADE`, `IR-CENTURY`, `zpseudorts6.f`) list and read as Monolith does (catalog, records, datasets, windows) | Census `KBPINTL`; compare the catalog with the oracle (which uses `LPSEUDO = true`) |
| O16 | Handling `KFSIZE = -1357` | Census; reader opens read-only and treats the physical size as the bound |
| O17 | Windows mandatory locks while HEC-RAS or DSSVue has the file open: confirm that header and bin reads succeed and that the lock test detects the holder | Hold a file open in DSSVue, then run the reader and the lock detector |
| O18 | **Verified.** ITS `KINDAT` versus `NPAIRS*IMULT` slack, and values after `NPAIRS` | Spare words exist (two private test files and `synth6`) and are never read; 111 slack records in the corpus run; counts agree with `NPAIRS` |
| O19 | **CLOSED.** Calendar-interval RTS (`1MON`, semi-month, tri-month, `1YEAR`) value times | Match Monolith on HEC-written `synth6` (stamps included); weekly also verified (§7.2). Still synthetic-only: no calendar or pseudo-regular data in the corpus |
| O20 | **CLOSED for values.** Pattern TS (101/111) layout: value count `KINDAT`, RTS internal header, no compression or quality (§7.3a) | Values match Monolith; times are a reader convention (Monolith's pattern times are overflowed) |
| O21 | **CLOSED.** Very old 15/30-minute data in daily blocks: files before `6-I` (`KVERNO < 67300`) use daily blocks; Monolith 3.3.27 still applies the rule (§7.2) | Verified on a `6-EA` file: counts and stamps match once the rule is applied |
| O22 | **CLOSED.** PD label layout after word `3*NCURVE+11` | Short and long labels verified; see §5.2 |


## 13. Location data in DSS6

DSS6 has **no location records**: record type 20 does not exist there, and
`zlocationRetrieve()`/`zlocationStore()` return `INCOMPATIBLE_VERSION` for version 6
files (`Public/zlocationRetrieve.c`, `Public/zlocationStore.c`). The only location data
is in time-series internal headers (§5.2):

| Record | Words (1-based) | Content | Source |
|---|---|---|---|
| Regular TS (100/105), `KINIHE >= 24` | 13–18, 19–24 | x, y, z (`REAL*8`); six descriptor codes in `zStructLocation` order (coordinate system, coordinate ID, horizontal units, horizontal datum, vertical units, vertical datum) | `heclib_f/src/zrrtsi.f` lines 588–601; written `zsrtsi6.f` lines 878–887 |
| Irregular TS (110/115), `KINIHE >= 28` | 17–22, 23–28 | the same | `heclib_f/src/zritsi6.f` lines 737–749 |

HEC treats the coordinates as present only when one of x, y, z is non-zero (`LCOORDS`);
`Dss6/ztsRetrieveReg6.c` (lines 239–248) copies them into the location struct only then.
There is no time-zone name for the location (the TS time-zone fields are the data's), and
no location supplemental field in the internal header (the supplemental text comes from
the user header, below). Paired data, grids and text carry no coordinates (`zspdi6.f`
"coordinates" are curve ordinates).

A dataset read without a D part spans several blocks, each with its own header. The Python layer returns a location for it only when every matching block stores the same coordinates and codes, and raises `DssLocationConflictError` otherwise. The repository's synthetic DSS6 builder zeroes words 13-24 when no coordinates are given, as `zsrtsi6.f` lines 878-881 do.

**Supplemental text.** Monolith's Java layer fills `supplementalInfo` of a DSS6 time
series from the record's user header (§14; `javaHeclib/src/Hec_ztsRetrieve.c`, "User
header (supplemental)"). HEC's C library does not: `ztsRetrieveReg6.c` copies only the
coordinates and codes into the location struct, and its supplemental text stays empty.
The engine follows Monolith for the text only (it had earlier followed the C library):

| Item | Rule |
|---|---|
| `supplemental`, `supplemental_bytes` | The user-header text of the same record (§14.2-14.3): up to the first NUL, trailing blanks removed, then decoded by the same dss-core rule as `user_header_text` (UTF-8, else modified UTF-8, else UTF-8 with U+FFFD per malformed sequence; §14.2). `supplemental_bytes` is undecoded. `None` when that text is empty |
| `supplemental_encoding` | `"utf-8"`, `"modified-utf-8"` or `"utf-8-replaced"`, the decoding of `supplemental` (§14.2); `None` exactly when `supplemental` is `None`. DSS7 location text is always `"latin-1"` (unchanged) |
| `supplemental_source` | `"dss6_user_header"`; `None` exactly when `supplemental` is `None`. DSS7 location records give `"dss7_location_record"` |
| `supplemental_items`, `supplemental_vertical_datum` | From the text with the location rules (`parse_supplemental`, `find_supplemental_keyword`), as for DSS7. A `VerticalDatum:` item lands only in `supplemental_vertical_datum` |
| Presence | Unchanged: a location exists only when a coordinate is non-zero (`LCOORDS`). Header text without coordinates gives no location (Monolith returns the text with zero coordinates) |
| Typed fields | Unchanged: coordinates, the six codes and `vertical_datum*` come from the internal header only |
| Dataset read (no D part, or a D part naming no stored block) | The block HEC reports for the whole dataset (§14.4): the last block by D-part date, as `DatasetIndex` orders blocks (types 100-119; a D part that is not a date sorts first). The location's `pathname` and text are that block's; its coordinates and codes equal every other block's (checked). Headers that differ between blocks are neither merged nor an error; only coordinates that differ raise `DssLocationConflictError` |
| Not a window read | A location read always uses the whole-dataset rule. A D part that names an absent block (before, between or after the stored ones) gives the last stored block's text, also for irregular data, where a §14.4 window read ending in an absent block reports no header |

The text is informational (maintainer decision): it may be stale or wrong, and a datum named in it
is never applied to values or merged into the typed fields (§14.5).

**Verification:** `sample6.dss` (public) and three private HEC-DSS6 test files: on the 1,293
time-series records Monolith reads, presence of coordinates agrees on all of them, and the
coordinate bits and all six codes are equal on the 883 that store coordinates (149 in
sample6).

**Verification:** the same four files on disposable copies (hashes unchanged), with
a fresh `HecTimeSeries` per record (§14.4 oracle note), which raises the reads to 3,412
(542 reads of one private file still fail in Monolith with status -2; all 542 are all-missing
blocks, HEC's expected "no data" result, §7.4). Coordinate presence agrees on all
3,412, and coordinates and codes are equal on all 1,874 that store coordinates.
Supplemental text agrees on 1,866 of the 1,874 (before the Monolith rule was adopted: 1,676), counting Monolith's
trailing blank padding as equal. The 8 remaining differences are all in one private file and Monolith-side:
6 over-reads past a header with no NUL and 2 non-ASCII decodes (§14.2-14.3).

**Verification:** after the UTF-8 decode (§14.2), on the same four files and fresh
copies, the 2 non-ASCII differences agree; only Monolith over-reads remain (7 in that run:
the bytes past the array are heap content, so one record that agreed in the earlier run
over-read in this one).
### 13.x Writing

`write_location6` rewrites every live block of a dataset: coordinates and codes at 0-based
internal-header words 12..24 (regular) or 16..28 (irregular), text in the user header
(`stringToUserHeader()`: four bytes per word, zero filled; `KINUHE` and `KBNHEA` hold the word
count). A native time-series rewrite deliberately keeps the stored user header, unlike `zsrtsi6.f`.

## 14. Time-series user header

**In plain language.** A DSS6 time-series record can carry a *user header*: a small
free-form text area that the program writing the record fills and HEC never checks.
HEC-DSSVue's USGS import, for example, wrote gage metadata there as `Keyword: value;`
items (State, Stream, County, Location, DataLocation, USGS No, DrainageArea, GageDatum,
VerticalDatum, HUC, Operator). Monolith shows the same text as a record's
`supplementalInfo`; HEC's C library returns the words but does not turn them into a
location's supplemental text. The text is informational: it may be stale or wrong, and
a datum named in it is never applied to values or merged into typed location fields
(maintainer decision).

### 14.1 Where it is stored

| Item | Rule | Source |
|---|---|---|
| Address, length | Info-block words `KIAUHE` (24+P, word address) and `KINUHE` (25+P, length in 4-byte words); the bin's `KBNHEA` repeats the length | §5.1, §4.5; `zinit6.f:ZINIT6` lines 699-774 |
| Position | `info, internal header, compression header, user header, data` when written; readers follow the address | `znwrit6.f:ZNWRIT6` lines 154-186 |
| Regular read | `NUHEAD = MIN(KINUHE, KUHEAD)` words read with `zgtrec6`, only for a block that is found | `zrrtsi.f:ZRRTSI6` lines 345-349 |
| Irregular read | `zreadx6` returns `NUHEAD = KINUHE` and reads `MIN(KINUHE, KUHEAD)` words; an absent record sets `NUHEAD = 0` | `zreadx6.f:ZREADX6` lines 53-58 and 72; called from `zritsi6.f:ZRITSI6` lines 200 and 372 |
| C buffer | `int userHeader[500]`, `kuserHeader = 500`: HEC truncates at 500 words (2,000 bytes) | `Dss6/ztsRetrieveReg6.c` lines 66-68, 145; `Dss6/ztsRetrieveIrreg6.c` lines 50-52, 124-125 |
| Copy to `tss` | Regular: `calloc(n, 4)` plus `convertIntArray`, **no terminator word**. Irregular: `calloc(n + 1, 4)`, zero terminator | `ztsRetrieveReg6.c` lines 222-231; `ztsRetrieveIrreg6.c` lines 193-205 |

The engine reads all `KINUHE` words (bounded at 100,000, `dss6/ts.rs`) and keeps them
raw (`Dss6TsRecord::user_header`). It does not truncate at 500 words as HEC does; no
corpus header comes near that.

### 14.2 Characters

- Four characters per word, in file byte order, as every DSS6 character field
  (`HOLCHR`). HEC's Fortran character interfaces copy `NUHEAD * 4` characters
  (`zrrtsc6.f:ZRRTSC6` lines 86-92 and `zritsc6.f:ZRITSC6` lines 32-38, `CSUPP`).
- There is no length in characters and no terminator rule on disk. The text ends at the
  first NUL byte, or at the end of the words. Records written by Monolith 3.3.27 to DSS6
  and the records of one private USGS-imported test file are blank-padded to a whole word
  with no NUL; most records of another private test file end in NUL bytes.
- No character set is recorded. **Decisions (maintainer): the text is "probably UTF-8"
  (2026-10-28), and invalid text is decoded as UTF-8 too ("use UTF-8",
  2026-10-30).** The text bytes, after the NUL cut and the trailing-blank trim
  (§14.3), are decoded in this order:
  1. **standard UTF-8**, when the bytes are valid UTF-8 (`"utf-8"`);
  2. else **Java modified UTF-8**, when valid (`"modified-utf-8"`): what Java writes
     through JNI and `java.io.DataOutput.writeUTF`, which encodes a character outside the
     BMP as a 6-byte surrogate pair (two 3-byte sequences `ED A0-AF xx ED B0-BF xx`) and
     U+0000 as `C0 80` (not a NUL byte, so the cut does not stop there). An unpaired
     surrogate or any other overlong form is not accepted;
  3. else **UTF-8 with replacement** (`"utf-8-replaced"`): valid sequences keep their
     characters and each malformed sequence becomes one U+FFFD, exactly as Java's own
     decoder does (`new String(bytes, StandardCharsets.UTF_8)` on Java 17). The engine's
     decoder is a clean-room implementation written from the Unicode Standard
     (chapter 3, Table 3-7 and the U+FFFD substitution of maximal subparts) and black-box
     tests of the JDK; no JDK source was consulted. That is the Unicode "maximal subpart" practice (`String::from_utf8_lossy`)
     except at an encoded surrogate: Java gives one U+FFFD for `ED A0-BF 80-BF` (and for
     `ED A0-BF` before a non-continuation byte or the end), where maximal subparts give
     one per byte. The engine matches Java on every byte string of length 1-5 over 20
     edge bytes (3,368,420 strings; 66,238 of them differ from `from_utf8_lossy`, all
     containing `ED`). In this step a modified-UTF-8 surrogate pair or `C0 80` is not
     joined; each byte group is replaced as Java replaces it.

  Earlier, step 3 was whole-text Latin-1. A multi-byte sequence cut by the NUL, by the
  end of the words or by the trailing-blank trim is invalid in both UTF-8 forms, so the
  text ends in one U+FFFD (the trim runs first, so the blanks are not part of the text).
  Rust: `user_header::decode_user_header_text`, `decode_utf8_replacing`,
  `user_header_text_encoding`, `UserHeaderEncoding` (`Utf8`, `ModifiedUtf8`,
  `Utf8Replaced`; re-exported at the crate root), `Dss6TsRecord::user_header_encoding`.
- The decoding used is reported: Python `user_header_encoding` (`"utf-8"`,
  `"modified-utf-8"` or `"utf-8-replaced"`; **`None` when there is no text**: no words, a
  leading NUL, or blanks only; `None` for DSS7 reads), the `user_header_encoding`
  dataframe attr (only with `user_header_text`, i.e. when there is text), and the DSS6
  location's `supplemental_encoding` (§13, also `None` without text). The raw bytes are
  unchanged (`user_header_bytes`, `user_header_words`, `supplemental_bytes`).
- Why: Monolith 3.3.27 hands the bytes to JNI `NewStringUTF`
  (`javaHeclib/src/jni_utility.c`, `hec_dss_jni_setStringField`, called for
  `supplementalInfo` from `javaHeclib/src/Hec_ztsRetrieve.c`, "User header
  (supplemental)"), i.e. reads them as modified UTF-8. With the earlier Latin-1 rule, the
  fixture's `Café` (stored by Monolith as `43 61 66 C3 83 C2 A9`, valid UTF-8 for
  `CafÃ©`, which Monolith reads back) and the private-file records with byte pair `C3 BD` (`ý` in
  UTF-8) disagreed with Monolith; with UTF-8 they agree (§14.7). On the public fixture
  `synthetic_userheader_utf8_6.dss`
  Monolith also reads the surrogate-pair header as U+1F30A and `C0 80` as U+0000, as the
  engine does.
- **Monolith and invalid text.** JNI defines `NewStringUTF` only for modified
  UTF-8. HotSpot 17 (`prims/jni.cpp` `jni_NewStringUTF`, `classfile/javaClasses.cpp`
  `java_lang_String::create_from_str`, `utilities/utf8.cpp` `UTF8::unicode_length`,
  `UTF8::next` and `UTF8::convert_to_unicode`, openjdk/jdk17u `jdk-17.0.11+9`) never
  substitutes U+FFFD. It decodes 1-, 2- and 3-byte forms without range checks (overlong
  forms and lone surrogates are accepted), turns any other byte into its Latin-1
  character (`*value = ptr[0]`, "default bad result"), and sizes the string as the
  count of bytes that are not `10xxxxxx`. When a continuation byte is not consumed by a
  sequence the string is shorter than the text, so the last characters are dropped;
  valid 4-byte UTF-8 (`F0 9F 8C 8A`) also becomes `ð`, U+009F and the rest is lost. On
  the public fixture `synthetic_userheader_lossy6.dss` (24 headers written by Monolith
  through heclib `ZSRTSX`), Monolith's
  text equals HotSpot's rule in 24 of 24 and the engine's in 0 of 24.
  The engine does not copy this loss: it follows the maintainer's UTF-8 decision, and
  the earlier "deliberate difference" note is withdrawn. No corpus header is invalid
  UTF-8, none holds a 4-byte sequence, and none holds a modified-UTF-8-only form (checked
  twice), so the corpus comparison (§14.7) is unchanged.
- Monolith reads DSS6 pathname parts, units and type through the same `NewStringUTF`
  (`Hec_ztsRetrieve.c`, `hec_dss_jni_setStringField` for `units`, `type`, `watershed`,
  `location`, `parameter`, `version`), so it would decode them as UTF-8 too. The engine
  keeps them Latin-1 (out of scope; reported only).
- **Scope.** Only the DSS6 time-series user-header text (and the DSS6 location
  supplemental text taken from it) is decoded this way. DSS6 pathnames, units, types
  and other DSS6 character fields stay Latin-1, and DSS7 location supplemental text is
  unchanged (Latin-1).

### 14.3 Text and items

| Rule | Engine | Source |
| DSS6 text for Java | `headerString = (char*)tss->userHeader`: a C string, up to the first NUL | `javaHeclib/src/Hec_ztsRetrieve.c` (`Java_hec_heclib_util_Heclib_Hec_1ztsRetrieve`, "User header (supplemental)") |
| Trailing blanks | Removed (padding). Monolith keeps them | engine choice; `HOLCHR` blank fill |
| No NUL in a regular record | Text is every stored byte. Monolith reads past the array (no terminator word, §14.1) and appended 2-4 heap bytes on 7 records of one private test file | `ztsRetrieveReg6.c` line 223 |
| Items | Split at `;` (`StringTokenizer`: empty segments skipped); a segment is an item only if `TextUtil.split(seg, ":")` gives exactly two parts; trailing `' '` removed from keyword and value, leading blanks kept; `HashMap`: last repeat wins | `hec.io.DataContainer.getSupplementalInfo()`, `hec.util.TextUtil.split(String, String, int)` (Monolith 3.3.27, `javap -c`) |
| Empty keyword or value | Engine keeps the item with `""`: `":v"` gives keyword `""` (value `"v"`), `"K:"` gives value `""`, and an all-blank part trims to `""`. Monolith's trailing-blank loop calls `charAt(-1)` on such a part and throws `StringIndexOutOfBoundsException`, so the whole parse fails in Monolith | same method, bytecode offsets 75-95 |
| HEC keyed lookup | `index(cuhead, 'name:')`, first match, value up to the next `;`, no trimming, case-sensitive; used only for `verticalDatumInfo` / `verticalDatum` | `Support/get_user_header_param.f`; `zrrtsi.f` lines 350-366; C `Utilities/verticalDatum.c:extractFromDelimitedString` |

**Keywords without a colon.** In two private USGS-imported test files the `Stream` and `Location` items are written
as `StreamSample Creek`-style segments: the keyword runs straight into the value. The
lengths vary from record to record, so these are not fixed-width fields. The program that
wrote them is not on this host: HEC-DSSVue 3.4.15's `dssplugin-usgs` and HEC-HMS 4.8-4.10's
`usgsPlugin-1.0.jar` (`hec.plugins.usgs.UsgsStation`) contain no such keyword strings, so
the writer is an older USGS import. HEC's own rule is what matters for reading: neither
Monolith's parser (exactly one `:` per item) nor the Fortran/C keyed lookups (`name:`)
can see them. The engine follows that rule. These segments are not items, and they stay
in `user_header_text` (Rust `user_header_unkeyed_segments` lists them).

### 14.4 Dataset reads: which block's header

A dataset read spans several blocks, each with its own user header. HEC returns one:

- **Regular data:** the last block found in the window. `IUHEAD` is re-read for every
  block found and left alone for absent ones (`zrrtsi.f` lines 345-349 are inside the
  "record found" branch). This is the same rule as for units, type and time zone (§7).
- **Irregular data:** the last block looked up. `zritsi6.f` lines 318-372 call `zreadx6` for
  each block from the window start through the block holding the end, and `zreadx6` sets
  `NUHEAD = 0` for an absent block. A window that ends in an absent block therefore
  returns **no** header. Whole-dataset reads end at a stored block.

The engine reports that block (`DatasetReport::user_header`, `user_header_block`). It
counts the blocks read whose text differs from the reported one
(`user_header_conflicts`; Python `user_header_conflict`). When HEC reports no header
(an irregular window ending in an absent block), `user_header_block` is `None` and the
conflict count is the number of blocks read that do carry text: `user_header_conflict`
with `user_header_block is None` means "no header reported although blocks have one",
not "two headers disagree". Monolith agrees on the fixture
cases: a regular dataset with January/February headers reports February, also for a
window that runs into the absent March block (the regular rule keeps the last block
found); an irregular
window ending in an absent February block gives a null `supplementalInfo`. A pattern
record is its own block.

**Oracle note.** A `hec.heclib.dss.HecTimeSeries` reused across blocks of one dataset
returns the first block's cached data. A comparison must use a fresh instance per record.

### 14.5 Vertical datum text

A `VerticalDatum: NAVD88` item (USGS style, capital `V`) is just text. HEC's datum logic
looks only for the case-sensitive keys `verticalDatumInfo:` and `verticalDatum:`
(`VERTICAL_DATUM_INFO_PARAM`, `VERTICAL_DATUM_PARAM`, `headers/verticalDatumFortran.h`;
`zrrtsi.f` lines 350-366 and 399-472, used only when a datum is requested with
`zset("VDTM")`). The engine never converts values and never fills typed location fields
from the user header. In one private USGS-imported test file the vertical datum appears only in this text.

**On a DSS6 location.** The user-header text is also a DSS6 location's
`supplemental` text (`supplemental_source="dss6_user_header"`, §13), which is Monolith's
`supplementalInfo` behavior; HEC's C library leaves a DSS6 location's supplemental text
empty. Such an item then shows up only in `supplemental_vertical_datum`, the informational
keyword lookup shared with DSS7 location text (`find_supplemental_keyword`, ASCII
case-insensitive like `dss_strcasestr`, so `VerticalDatum: NAVD88` gives `" NAVD88"`, value
untrimmed). `vertical_datum_code`, `vertical_datum`, `vertical_datum_unknown` and
`vertical_datum_named` still come only from the internal-header code: a record with typed
code 0 stays unknown, and a typed NGVD-29 is not overridden by NAVD88 text.

### 14.6 DSS7

DSS7 time-series records also have a user header (`USER_HEAD_ADDRESS` /
`USER_HEAD_NUMBER`, dss7.md §5.2), which Monolith maps the same way (dss7.md §12). It is not
decoded for Python: DSS7 reads return `None` for every `user_header_*` key.

### 14.7 Verification

The comparisons are against Monolith 3.3.27 on disposable copies, and every copy's hash
was unchanged after the reads. In brief:
on the 33 openable private DSS6 test files, every record and whole dataset that both readers
return has the same text (after trailing blanks are trimmed) and the same items, except
for the Monolith-side heap over-read (§14.3). With the earlier Latin-1 decode the non-ASCII
`C3 BD` reads of one private file also differed; with the UTF-8 decode (§14.2) they agree. On the
Monolith-written fixture `synthetic_userheader6.dss`, the text equals Monolith's on every
record, `Café` included. Dataset and window reads also match, including the
irregular empty case. A later run repeated the record comparison on the 33 files: the counts are
unchanged, because no corpus header with text is invalid UTF-8.
