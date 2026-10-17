# mkext2

## 0.1.2 — 2026-10-17

### Patch changes

- [a529664](https://github.com/tuler/mkext2/commit/a529664e388aca6f397e07827bc7b34b08b5ae36) Bound peak memory when building into a file: about 110 MiB for directory
  trees and 40 MiB for archives, whatever the image size (it grew with the
  image before, up to 900 MiB on macOS for a 1 GiB tree). Buffered writes use a
  fixed pool of 8 MiB buffers, and read-ahead reads whole files in 4 MiB pieces
  into one 64 MiB ring instead of allocating per file. Read-ahead uses up to 4
  threads by default (8 before), which is faster on macOS and Linux because
  more readers mostly wait on each other opening files. `Device` gains
  `write_space`, a provided method that buffering devices implement so that
  file data is read straight into their buffers. Output is unchanged. — Thanks @tuler!

## 0.1.1 — 2026-10-14

### Patch changes

- [f362376](https://github.com/tuler/mkext2/commit/f362376362a73fc7e62fd0beddbb258c3dc92d4e) Preserve overlapping buffered writes and report their size correctly. Avoid
  truncating starting images when the output is a Unix hard link to the input.
  Keep source file timestamps when the filesystem epoch is zero, expose full
  32-bit owners in the reader, and reject inconsistent image geometry before
  using it. Compare full owners by default, with an explicit `owner_low_bits`
  option for reference tools that truncate uid/gid. Speed up bitmap operations
  and bound searches over disjoint buffered writes. Preserve Windows drive
  prefixes when parsing directory, tar, and device-table layer paths. — Thanks @tuler!

