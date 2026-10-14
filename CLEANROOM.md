# Clean-room record

mkext2 accepts the same command line as genext2fs / xgenext2fs (GPL-2.0-only)
and stores the same content, but it is an independent design, licensed under
Apache-2.0, and contains no genext2fs code. This file records how that was
ensured.

## Roles

- **Specification author.** Studied xgenext2fs, wrote an earlier port that is
  not part of this repository, and wrote the requirements
  [`docs/SPEC.md`](docs/SPEC.md). The specification describes requirements and
  user-visible behavior (the command line interface, the content stored); it
  does not describe genext2fs' internal algorithms, and contains no genext2fs
  code or comments. Images do not need to match xgenext2fs' byte for byte.
- **Implementer.** Writes the image generation engine from `docs/SPEC.md`, the
  public ext2 documentation and black-box experiments with the `xgenext2fs`
  program. The implementer has never seen genext2fs' source code or the earlier
  port.

## Rules for the implementer

1. Work only inside this repository.
2. Do not read, search for or download the source code of genext2fs, xgenext2fs
   or forks of them (for example github.com/cartesi/genext2fs,
   github.com/bestouff/genext2fs, Debian or Buildroot copies). Do not read other
   repositories or directories on this machine that might contain them (in
   particular `~/Documents/workspace/cartesi/genext2fs` and
   `~/Documents/workspace/personal/mkext2`).
3. The `xgenext2fs` program may be run as a black box (inputs in, image out) to
   compare results; its output may be inspected (for example with the image
   reader, `e2fsck`, `debugfs` or a hex dump).
4. Public documentation of the ext2 format may be used (for example the Linux
   kernel's ext2 documentation, "The Second Extended File System" by Dave Poirier,
   e2fsprogs manuals).
5. Questions about the specification go back to the specification author, who
   answers by clarifying the specification, never with code.

## What was kept

Written independently of genext2fs (by the specification author) and kept: the
storage devices, the read-ahead, the hasher, the error type, the image reader and
checker, the host metadata helpers, the tar reader (which follows libarchive's
behavior, see [NOTICE](NOTICE)), the Node.js binding, the tests, the examples, the
scripts, the CI and release workflows and the documentation.

## Log

- The first draft of the specification described xgenext2fs' layout rules in
  detail, to reproduce its images byte for byte. It was replaced, before any
  implementation started, by the current specification, once byte-for-byte
  identity was dropped as a requirement; the implementer only receives the
  current one.

- Repository created with the kept parts, the specification and this record;
  license changed to Apache-2.0. The genext2fs device table test fixtures were
  replaced with new tables, and the tests check validity and content equivalence
  with xgenext2fs instead of identical bytes, and do not depend on the exact
  wording of messages.

- 2026-10-13, implementer (a Claude Opus 5.5 session that had never seen
  genext2fs' source or the earlier port): wrote the engine from
  `docs/SPEC.md`, the kept modules and the public ext2 format description —
  `src/ondisk.rs` (structures), `src/fs.rs` (geometry, bitmaps, cursor
  allocation, file/directory/pointer block writing, metadata), `src/tree.rs`
  (content model), `src/size.rs` (exact sizing), `src/layers.rs` (directory,
  archive and device table layers, ownership/permission/time policies),
  `src/build.rs` (builder API, starting images, block maps, verbose output),
  `src/options.rs`, `src/cli.rs` (getopt_long-compatible parser, own help and
  messages), `src/main.rs` and `src/lib.rs`. Kept modules were adapted only
  in `src/source/dir.rs` (device number in `Meta` for hard-link identity, a
  readability check). The design is mkext2's own: block groups of 8 × block
  size blocks (balanced when the last group cannot hold its metadata),
  `sparse_super` + `filetype`, one forward-only allocation cursor so each file
  is contiguous with its pointer blocks just before the data they map,
  inodes numbered in input order, a 16-block `lost+found`, and a search for
  the smallest size whose geometry holds the counted content.
  Behaviors the specification leaves open were settled by running the
  installed `xgenext2fs` 1.5.6 binary as a black box and reading its images
  with the kept reader (for example: symlinks are always 0777; a tar `./`
  member changes the root's permissions and owner but not its times; device
  table names ending in `.` are skipped; `lost+found` has 16 blocks; tar link
  members that libarchive types as regular files are regular files). Images
  were also checked with e2fsck 1.47.0 from a local Debian container. The
  rules above were followed: no genext2fs/xgenext2fs source, forks or
  packages were read, searched for or downloaded, no web searches were made,
  and nothing outside this repository was opened besides the installed
  binaries, the container and a private scratch directory.
