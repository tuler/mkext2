# Code audit — 2026-10-14

Reviewed the Rust image engine, input scanning, tar handling, reader, CLI,
read-ahead and file devices, Node binding, and existing tests. Reference
comparisons used xgenext2fs only as a black box.

## Correctness findings addressed

| Priority | Finding | Change and regression coverage |
| --- | --- | --- |
| P1 | A hard link from the output to the starting image bypassed the same-file check. Copying onto that alias truncated the input image. | On Unix, compare device and inode identity. A roundtrip test updates through a hard link and checks the resulting filesystem. |
| P1 | Appending to one buffered write could overlap another pending run. Flushing then restored stale bytes over the newer write. | Flush older overlapping runs before merging. Test the reproducer and compare 1,200 interleaved writes with a memory device. |
| P2 | `FileDevice::size()` returned the file's old size while writes were buffered. | Synchronize writes before reading file metadata. Test extension and subsequent truncation. |
| P2 | A filesystem epoch of zero also erased source file timestamps with `Timestamps::Source`. | Apply the timestamp policy independently of the filesystem time. Extend the epoch test to cover zero. |
| P2 | `Image::walk()` truncated uid/gid to 16 bits; content comparison consequently missed differences in their high bits. | Read full owners. Compare full owners by default and explicitly opt into `owner_low_bits` for xgenext2fs comparisons. Independently test full owners and comparison behavior. |
| P2 | Inconsistent group geometry and inode counts reached bitmap allocation or indexing; descriptor allocation also preceded checking whether the table existed. | Validate geometry, inode counts, and descriptor bounds first. Add four malformed-image tests and run them in both debug and release profiles. |
| P2 | CLI layer parsing split Windows paths such as `C:\root` at the drive colon, interpreting the remainder as an image target. | Preserve Windows drive prefixes, add a Windows-specific parser test, and run Rust library tests in Windows CI. |

Added 11 tests exercised locally, one Windows-only test, and expanded one
existing timestamp test. The fixes include a release changeset.

## Performance

Bitmap range allocation and free-run scanning now work a word at a time;
serialization works a byte at a time. An exhaustive comparison against
individual bits covers alignment, word boundaries, limits, and padding.

The file backend now buffers at most 64 disjoint runs as well as limiting
their total bytes. This bounds the linear searches that previously grew
with every small disjoint write.

Local macOS measurements with optimized Rust builds, seven runs per case:

| Case | Median before | Median after |
| --- | ---: | ---: |
| 10,000 disjoint 32-byte writes, including flush and close | 64.62 ms | 29.68 ms |
| Empty 16 GiB image, 4 KiB blocks | 20.64 ms | 17.16 ms |
| Empty 16 GiB image, 1 KiB blocks | 805.25 ms | 797.22 ms |

The write benchmark verifies every output byte. The first result is a
focused backend measurement, not a general image-build speedup. The 1 KiB
image measurement shows no meaningful improvement; filesystem costs dominate.

Two larger follow-ups remain worth benchmarking:

- Compressed tar layers are decompressed during scanning and again during
  writing (`Scanner::add_tar` and `DataSrc::Tar`). Spooling the decompressed
  archive during the first pass could exchange that repeated CPU work for
  temporary storage and extra I/O.
- `Output::Writer` builds the entire image through `build_vec` before writing
  it. A temporary file backend followed by a bounded copy could reduce peak
  memory for stdout and other stream outputs.

## Validation

- Release workspace tests, including compatibility, roundtrip,
  reproducibility, CLI, and malformed-input tests.
- Randomized compatibility run with 200 seeds.
- Debug malformed-image tests.
- Native Node binding build and all seven Node tests.
- Workspace Clippy with warnings denied, formatting, and diff whitespace checks.

`e2fsck` is unavailable locally, so that optional test skipped its external
check. Windows-specific behavior requires the Windows CI run; it was not
executed on this macOS host. Unix hard-link identity detection does not add
Windows hard-link detection.
