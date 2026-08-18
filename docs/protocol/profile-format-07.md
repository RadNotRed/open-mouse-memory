# Onboard profile format `0x07`

Status: **read, decoded for current controls, and verified for sector writes**.

Target device: Logitech G PRO X SUPERLIGHT 2 through LIGHTSPEED receiver `046D:C54D`, logical receiver index
`0x01`.

The device advertises `ONBOARD_PROFILES` (`0x8100`) at runtime index `0x0D`, feature version `0`, and returns
profile format ID `0x07` from function `0x00`. A Linux read-only hardware session on 2026-08-18 returned:

```text
01 07 01 05 01 05 10 00 FF 0A 04 00 00 00 00 00
```

The CLI decodes the standard 16-byte feature descriptor while retaining its full raw value:

| Offset | Size | Meaning | Confidence |
| ---: | ---: | --- | --- |
| `0x00` | 1 | memory model (`0x01`) | confirmed response, inherited meaning |
| `0x01` | 1 | profile format (`0x07`) | confirmed |
| `0x02` | 1 | macro format (`0x01`) | confirmed response, inherited meaning |
| `0x03` | 1 | writable profile count (`5`) | confirmed response, inherited meaning |
| `0x04` | 1 | ROM profile count (`1`) | confirmed response, inherited meaning |
| `0x05` | 1 | button count (`5`) | confirmed response, inherited meaning |
| `0x06` | 1 | sector count (`16`) | confirmed response, inherited meaning |
| `0x07` | 2 | sector size (`255`), big-endian | confirmed response, inherited meaning |
| `0x09` | 1 | mechanical layout (`0x0A`) | confirmed response, inherited meaning |
| `0x0A` | 1 | various flags (`0x04`) | confirmed response, inherited meaning |
| `0x0B` | 5 | zero on tested device; reserved/unknown | confirmed value, unknown meaning |

Function `0x50` reads 16 bytes using a big-endian sector and offset. Because sectors are 255 bytes, the final
read starts at offset 239 and overlaps the previous chunk by one byte. Reads beginning at offset 240 are
rejected with `INVALID_ARGUMENT`.

Sector 0 is the five-entry directory. Each four-byte entry contains a big-endian sector, enabled byte, and
flags byte. The tested directory points slots 1 through 5 at sectors 1 through 5.

The decoded format-7 profile fields are:

| Offset | Size | Meaning |
| ---: | ---: | --- |
| `0x00` | 1 | zero-based extended report-rate code |
| `0x01` | 1 | default DPI stage index |
| `0x02` | 1 | shift DPI stage index, or `0xFF` |
| `0x03` | 25 | five entries of LOD byte, little-endian X DPI, little-endian Y DPI |
| `0x30` | 20 | five four-byte button mappings |
| final 2 bytes | 2 | CRC-16/CCITT-FALSE, big-endian |

All other bytes are retained unchanged by the encoder.

## Completed checks

1. Read and validate the directory and all 16 sectors.
2. Validate CRC-16/CCITT-FALSE on all five profile sectors.
3. Decode five DPI stages and five standard mouse mappings from the active profile.
4. Compare the active profile report-rate code and DPI index with live feature reads.
5. Save a complete 16-sector JSON backup.
6. Rewrite sector 1 unchanged and verify all 255 bytes and its checksum after the write.
7. Add read-modify-write encoding that preserves every unknown byte.
8. Add automatic readback and best-effort rollback for typed profile writes.

## Write gate

Profile-memory functions `0x60`, `0x70`, and `0x80` remain blocked in the raw-command path. Typed profile
writes use the standard start, 16-byte data, and end sequence. They compute the checksum, compare the complete
readback, and attempt to restore the original sector if verification fails.

Free-form keyboard assignments, macros, alternate button mappings, profile names, and the remaining unknown
format-7 fields are outside the current write surface.
