# mkext2 specification

mkext2 builds ext2 filesystem images from directories, tar archives and device
tables, without root privileges. It is meant to replace `xgenext2fs` (cartesi's
genext2fs) in the Cartesi CLI, and accepts the same command line, but it is an
independent design: its images do not have to be identical to xgenext2fs', only
**valid**, with the **same content**, **reproducible**, and produced **fast**.

This document says *what* mkext2 must do. *How* (data structures, allocation
strategy, layout) is the implementer's choice, within the requirements below.

---

## 1. Requirements

1. **Valid ext2.** Every image passes `e2fsck -fn` with no problem reported, and
   the repository's checker (`reader::Image::check`) finds nothing. Linux can
   mount it (ext2 or ext4 driver).
2. **Same content as xgenext2fs.** For the same inputs and options, the files,
   directories, links, devices, permissions, owners, times and data in the image
   are the ones xgenext2fs would store (see `reader::compare_content`; the
   intentional differences are in section 9). Block size, label and creator OS
   are as requested.
3. **Reproducible.** The image depends only on the inputs (their content, names
   and metadata) and the options: never on the host, the source location, the
   order in which the host lists directories, the time of the build (when a
   timestamp option is given), the number of threads, or whether the output is a
   file, memory or a pipe. Unused bytes are zero; nothing random is written.
4. **Fast.** At least as fast as the previous implementation: building a 1 GiB
   image from a directory or an uncompressed tar takes about 0.5 s on an Apple M1
   Max with 4 KiB blocks (xgenext2fs: about 2.4 s). `scripts/bench.sh` measures
   it.
5. **Never runs out of space by mistake.** When the size is computed (no `-b`),
   the content always fits: sizing is exact, not an estimate that can fail.

Kept parts of the repository (not to be rewritten, but they may be adapted):
storage devices (`src/device.rs`), errors (`src/error.rs`), hashing
(`src/hash.rs`), read-ahead (`src/prefetch.rs`), image reader and checker
(`src/reader.rs`), host metadata helpers (`src/source/dir.rs`), the tar reader
(`src/source/tar.rs`), the Node binding (`crates/mkext2-node`), tests, examples
and scripts. The rest of the library is to be written: on-disk structures,
engine, input processing, sizing, builder API, command line, verbose output and
crate root.

---

## 2. Filesystem format

Target the classic ext2 format as documented publicly (Linux kernel
`Documentation/filesystems/ext2.rst`, Dave Poirier's "The Second Extended File
System", e2fsprogs):

- revision 1 (dynamic), first non-reserved inode 11, 128-byte inodes;
- features: `sparse_super` (superblock and descriptor backups only in groups 0,
  1 and powers of 3, 5 and 7, all written and consistent), `filetype` (directory
  entries carry the file type), and `large_file` when a file is 2 GiB or larger.
  No other feature (no journal, extents, `dir_index`, `resize_inode`);
- the root directory is inode 2, mode `0755`, owned by 0:0 unless an input
  changes it; `lost+found` exists (inode 11 is customary) when the reserved
  block count is not zero, and is empty;
- superblock: state clean, sensible error behavior and mount counts, the volume
  label (at most 16 bytes, zero padded), the creator OS, UUID all zero unless a
  UUID option is added later, write and check times = the filesystem time
  (section 5);
- uid and gid use all 32 bits (the high 16 bits in the Linux "high" inode
  fields); device numbers use the old 16-bit encoding when major and minor are
  below 256, the new 32-bit encoding otherwise (as Linux does);
- regular files of any size up to the format's limit (triple indirection),
  sparse files with `-z` (blocks that are entirely zero are not allocated);
- symbolic links shorter than 60 bytes are stored in the inode, longer ones in
  one block; a target that does not fit in one block (length ≥ block size) is
  left out with a warning containing the word "symlink";
- names longer than 255 bytes are left out with a warning.

The layout (group geometry, inode numbering, where blocks go) is free, but must
be deterministic. Suggested: give each file contiguous data blocks, keep
directories and pointer blocks close to the data, fill groups in order. Images
are often stored compressed or hashed, so avoid scattering data needlessly.

---

## 3. Size

`blocks` is the image's block count, `reserved` its reserved block count.

- **Reserve.** `reserved = floor(blocks × m / 100)`, with `m` from `-m`
  (default 5; decimals allowed; negative is an error). With `reserved = 0` there
  is no `lost+found`.
- **With `-b n`:** `blocks = n`. If the content (plus metadata and reserve) does
  not fit, it is raised to the smallest size that fits, with an informational
  message.
- **Without `-b`:** the smallest size such that, after adding all the content,
  the free blocks are at least `reserved + extra`, where `extra` comes from the
  readjustment (`-r`):
  - `+N[suffix]`: `extra` = N × suffix multiplier (blocks);
  - `+N%`: `extra` = N% of the size needed without extra;
  - `N[suffix]` or `N%` without `+`: an absolute size: `blocks` = that value
    (N% of the needed size), raised to the needed size if smaller;
  - suffixes: none (×1), `k` (×1000), `ki` (×1024), `M` (×10^6), `Mi` (×2^20),
    `G` (×10^9), `Gi` (×2^30); anything else is an error that mentions
    "readjustment". The default is `+0`.
- **Inodes.** Enough for the content (including `lost+found` and the 10
  reserved inodes), plus at least one free inode per 16 KiB of free unreserved
  space (per `-i bytes` when given), and at least `-N` in total. Rounding up to
  fill whole inode table blocks or groups is fine.
- The sizing must account exactly for data blocks, pointer blocks, directory
  blocks, holes (`-z`) and all metadata. Inputs read from standard input cannot
  be measured beforehand: build in memory, or give `-b`/`-N` (an informational
  message says so).

---

## 4. Inputs

Layers are added in order; each goes into its target directory (default `/`;
`dir:/path` on the command line, `Layer::at` in the API). A target that does not
exist is an error mentioning the path and "not found". When a later layer
provides a name that already exists:

- **directory layers:** the existing entry stays (a warning names it); when both
  are directories, the new one's content is merged into the existing one;
- **archive layers:** the existing entry gets the member's permissions, owner
  and times when both have the same type (a member `./` or `.` updates the
  layer's target directory); a member of another type is skipped with a
  warning;
- **device tables:** the existing entry gets the line's permissions and owner.

Ownership and permission options apply to directory and archive layers:
`-U uid` makes every owner `uid:uid` (`-1` means no change); `-P` clears the
group and other permission bits (`077`); `-q` is both (`-q` alone means uid 0;
`--squash uid` gives the uid).

### 4.1 Directory layers

The source must be a directory (a symbolic link to one is followed); otherwise
an error naming the path. Content is processed recursively; entries are taken in
byte order of their names (never host order). For each entry, its own metadata
(not following symbolic links) gives the type, permissions (`07777`), owner,
times, size and device numbers. Entries whose metadata cannot be read, or that
cannot be opened, are skipped with a warning naming them.

- Regular files: content read up to their size at the time they are examined.
- Symbolic links: their target.
- Devices, FIFOs, sockets: recreated as such.
- Hard links: files (not directories or links) with more than one link that
  share their identity (device and inode number) with a file already added from
  any directory layer become hard links to it, instead of copies.

### 4.2 Archive layers

A tar archive from a path (must be a regular file, otherwise an error naming
it), memory or standard input, plain or compressed. The kept tar reader
(`src/source/tar.rs`) decodes members (including pax, GNU and sparse members);
its `read_data` method returns member content. Member paths are relative to the
target directory (a leading `/` means the image root); `./` prefixes are
allowed. A member whose parent directory does not exist (yet) is skipped with a
warning. Member types: directories, regular files, symbolic links, hard links
(to an earlier member; resolved from the target directory, an error mentioning
"not found" if missing), devices, FIFOs, sockets. Unknown members are skipped
with a warning. Owners are the member's numeric uid/gid; times its mtime (and
ctime when the archive has one, else 0).

### 4.3 Device tables

The mkfs.jffs2 device table format, from a path (a regular file), memory or
standard input:

    <path> <type> <mode> <uid> <gid> <major> <minor> <start> <inc> <count>

- `#` starts a comment; blank lines are ignored; fields are separated by
  whitespace; `mode` is octal, the others decimal; a `-` (or any field that is not
  a number) ends the line's fields, the rest keep their defaults (0, and 1 for
  `inc`). Lines with fewer than three fields are skipped (with a warning unless
  empty).
- Types: `d` directory, `f` regular file (empty), `l` symbolic link (empty
  target), `p` FIFO, `s` socket, `c` character device, `b` block device; anything
  else is a warning.
- The path `/` (type `d`) changes the target directory's permissions and owner.
- A `count` greater than 0 creates `count - start` entries named `<path><n>` for
  `n` from `start` to `count - 1`, with minor `minor + (n - start) × inc`.
- An entry that exists gets the permissions and owner; otherwise it is created,
  its parent directory must exist (else a warning). Created entries get the
  table file's times (0 for tables from memory).

---

## 5. Time

The filesystem time `T` (superblock times, root, `lost+found`):

| option | `T` | inode times |
|---|---|---|
| none | `SOURCE_DATE_EPOCH` if set, else the current time | from the inputs |
| `-f`, `--faketime` | 0 | 0 |
| `--timestamp t` | `t` | all `t` |
| `--clamp-timestamps t` | `t` | from the inputs, at most `t` |

Without `-f`, an input time of 0 stays 0. atime is always the mtime. When the
filesystem time is 0 (for example `SOURCE_DATE_EPOCH=0`), inode times are 0.
With a starting image (`-x`), its times are not changed.

---

## 6. Other options

- `-x image`: start from an existing ext2 image (revision 0 or 1, 128-byte
  inodes, features limited to `filetype`, `sparse_super`, `large_file`) instead
  of an empty one: it is copied to the output (or modified in place when it is the
  output) and the layers are added; its size does not change (`-b`, `-r`, `-N`,
  `-i`, `-m` are ignored); the block size is the image's (`-B` must agree). The
  result must stay valid. Images made by mkext2 and by xgenext2fs must work.
- `-L label`, `-o os` (`linux`, `hurd`, `gnu`, `freebsd`, `lites` or a number;
  case-insensitive; unknown names mean Linux; negative numbers are an error).
- `-e value`: unallocated blocks are filled with this byte (low 8 bits; 0 means
  no fill).
- `-z`: sparse files (section 2).
- `-g path`: write `<path with / replaced by _>.blk` (in the current directory)
  containing `<size>:` followed by ` <block>` for each data block of the file in
  order (0 for holes) and a newline. A missing path is an error mentioning "not
  found".
- `-v`: print `mkext2 <version>`, and after the build a description of the
  filesystem (format of mkext2's choosing; it lists at least directory entries).
- `-B`: 1024, 2048 or 4096 (default 1024); anything else is an error that
  mentions the value.

---

## 7. Command line

`mkext2 [options] image` (`image` = `-` writes to standard output, pipes
included). Options are xgenext2fs', parsed like GNU `getopt_long`: options and
the image can come in any order, `--` ends the options, short options group
(`-fz`) and take attached or separate arguments (`-B4096`, `-B 4096`), long
options take `--name=value` or `--name value` and can be abbreviated to a unique
prefix. Exactly one image argument.

| short | long | argument |
|---|---|---|
| `-x` | `--starting-image` | image |
| `-d` | `--root` | `dir[:path]` |
| `-D` | `--devtable` | `file[:path]` |
| `-a` | `--tarball` | `file[:path]` |
| `-B` | `--block-size` | bytes |
| `-b` | `--size-in-blocks` | blocks |
| `-r` | `--readjustment` | `[+]N[suffix]` or `[+]N%` |
| `-i` | `--bytes-per-inode` | bytes |
| `-N` | `--number-of-inodes` | count |
| `-L` | `--volume-label` | label |
| `-m` | `--reserved-percentage` | percent |
| `-o` | `--creator-os` | os |
| `-g` | `--block-map` | path (repeatable) |
| `-e` | `--fill-value` | value |
| `-z` | `--allow-holes` | |
| `-f` | `--faketime` | |
| `-q` | `--squash` | uid (long form only) |
| `-U` | `--squash-uids` | uid |
| `-P` | `--squash-perms` | |
| | `--timestamp` | seconds |
| | `--clamp-timestamps` | seconds |
| `-h` | `--help` | |
| `-V` | `--version` | |
| `-v` | `--verbose` | |

- A layer argument is split at its first `:` into source and target; a source
  of `-` is standard input (at most one input may be).
- Numbers: `-B`, `-b`, `-i`, `-N`, `-m` accept decimals and the multiplier
  suffixes `k`, `M`, `G` (powers of 1000) and `Ki`, `Mi`, `Gi` (powers of 1024),
  e.g. `-b 1Mi`; other trailing text is ignored. `-e`, `-U`, `--squash` take
  integers. `--timestamp` and `--clamp-timestamps` take a number of seconds that
  fits in 32 bits; anything else is an error.
- `-h` prints usage on standard error and exits 0; `-V` prints
  `mkext2 <version>` on standard output and exits 0.
- Messages go to standard error: informational ones (which layer is being added,
  sizes raised) as plain lines, warnings as `<program>: <message>` naming the
  entry, path or value involved. Errors print `<program>: <message>` and exit
  with status 1.

---

## 8. Library API

Crate `mkext2`. The tests, examples and the Node binding use:

- `Options` (with `Default`) and its public fields: `block_size: u32`
  (default 1024), `block_size_set: bool`, `size_in_blocks: Option<i64>`,
  `readjustment: Readjustment`, `bytes_per_inode: Option<f32>`,
  `number_of_inodes: Option<i32>`, `volume_label: Option<Vec<u8>>`,
  `reserved_fraction: Option<f32>` (`-m` / 100), `creator_os: i32`,
  `fill_value: i32`, `allow_holes: bool`, `timestamps: Timestamps`,
  `source_date_epoch: Option<i64>` (overrides the environment variable),
  `squash_uids: Option<u32>`, `squash_perms: bool`,
  `starting_image: Option<Source>`, `block_maps: Vec<Vec<u8>>`,
  `block_map_dir: Option<PathBuf>`, `verbose: bool`, `layers: Vec<Layer>`,
  `message_sink: Option<MessageSink>`, `threads: Option<usize>` (read-ahead
  threads, never affects the output).
- `Layer { kind: LayerKind, source: Source, target: Option<Vec<u8>> }` with
  `Layer::dir`, `tar`, `tar_bytes`, `devtable`, `devtable_bytes` and `at`;
  `LayerKind::{Dir, Tar, DevTable}`;
  `Source::{Path(PathBuf), Stdin, Bytes(Arc<[u8]>)}`.
- `Timestamps::{Source, Zero, Fixed(u32), Clamp(u32)}` (default `Source`).
- `Readjustment::{new (unchecked), parse (checked), apply, as_str}`.
- `build_file(&Options, path) -> Result<Report>`,
  `build_vec(&Options) -> Result<(Vec<u8>, Report)>`,
  `build(&Options, Output) -> Result<Report>` with
  `Output::{Path(&Path), Writer(&mut dyn Write)}`, `build_on_device`.
- `Report { info: ImageInfo, messages: Vec<Message>, verbose_output: Option<String> }`;
  `ImageInfo { block_size, blocks, free_blocks, reserved_blocks, inodes,
  free_inodes, groups }` (u32); `Message { level: Level, text: String }`;
  `Level::{Info, Warning}`; `MessageSink = Arc<dyn Fn(&Message) + Send + Sync>`.
- `Error`, `Result`, `lookup_creator_os`, `si_atof`.
- `cli::{parse_args, Command::{Help, Version, Build { options, output, print_version }},
  help_text, main_with_args, VERSION}`.
- `reader` (kept) and the `ondisk` definitions it and the tests use.

With the environment variable `MKEXT2_TIMING` set, phase durations may be printed
on standard error.

---

## 9. Differences from xgenext2fs

Content is the same as xgenext2fs', except where xgenext2fs is wrong or limited:

- layer targets (`dir:/path`) are honored (xgenext2fs ignores them unless `-x`);
- an archive member naming an existing entry updates that entry (xgenext2fs
  changes its parent directory instead);
- symbolic links that do not fit in one block are left out (xgenext2fs writes
  them in a form Linux rejects);
- device numbers keep all their bits (xgenext2fs keeps 8 bits of each);
- uid and gid keep all 32 bits (xgenext2fs keeps 16);
- sizing is exact (xgenext2fs can fail with "no space" on its own estimate) and
  `-r +N` guarantees N free blocks;
- `-q` without an argument works (xgenext2fs crashes), and the image can be
  written to a pipe;
- `-e` fills exactly the free blocks;
- AppleDouble `._*` archive members are kept as ordinary members everywhere;
- the layout, the verbose output and message wording are mkext2's own.
