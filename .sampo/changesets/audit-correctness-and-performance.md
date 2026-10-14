---
cargo/mkext2: patch
npm/@tuler/mkext2: patch
---

Preserve overlapping buffered writes and report their size correctly. Avoid
truncating starting images when the output is a Unix hard link to the input.
Keep source file timestamps when the filesystem epoch is zero, expose full
32-bit owners in the reader, and reject inconsistent image geometry before
using it. Compare full owners by default, with an explicit `owner_low_bits`
option for reference tools that truncate uid/gid. Speed up bitmap operations
and bound searches over disjoint buffered writes. Preserve Windows drive
prefixes when parsing directory, tar, and device-table layer paths.
