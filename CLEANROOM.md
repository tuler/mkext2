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
