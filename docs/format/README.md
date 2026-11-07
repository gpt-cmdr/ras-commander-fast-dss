# DSS format notes

Written specifications derived from HEC's `heclib` source. Code cites these notes and
the underlying source files.

- `dss7.md` — DSS7 (packet P04, from `heclib_c`, commit `283185bd`)
- `dss6.md` — DSS6 (packet P05, from `heclib_f` and `heclib_c/src/Dss6`, commit `283185bd`)

## Established so far

| Fact | Source |
|---|---|
| `ZDSS` at bytes 0..4; version string at bytes 16..20 (`6-xx` / `7-xx`) | `heclib_c/src/Public/zgetFileVersion.c` |
| DSS7 permanent section uses 8-byte little-endian words: 1 header size, 2 version, 3 records, 4 aliases, 5 EOF address (word index of the EOF flag −97532; file is (value+1)×8 bytes), 6 dead, 7 expansions, 8 collections, 9 renames, 10 deletes, 11 alias deletes, 12 create date, 13 last write; word 99 = −97531. Full map: `dss7.md` §2 | `heclib_c/src/Internal/zinit.c` (`zdssFileKeys`), `Internal/zpermRead.c` |
| DSS7 hash, bins, record info, catalog, locking, TS block layout | `dss7.md` §3–§7 |
| DSS6 permanent section (69 words, 4-byte words; 0-based indexes here): 0 `ZDSS`, 1 records, 2 sequence number, 3 hash size code, 4 version (bytes 16..20), 5 `KFSIZE` = next free 1-based word address (the word there holds −9630), 6 dead, then create/last-write dates. Hash table starts at word address 70. Full map: `dss6.md` | `heclib_f/src/zinit6.f` (lines 446–572), `zcheck6.f` (line 212) |
| DSS6 addressing: 127 data words per 512-byte physical record plus 4 pad bytes. Byte offset of address `a` = `((a-1) div 127)*512 + ((a-1) mod 127)*4`; file bytes = `ceil(KFSIZE/127)*512`, and early files may end with a 508-byte record | `zdsskz.h` (`NBSIZE`), `zrrec6.f`, `zwrec6.f`, `zgetrw6.f` |
