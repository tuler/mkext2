# mkext2

Reproducible ext2 filesystem image generation, without root privileges.
mkext2 is a Rust implementation compatible with
[genext2fs](https://github.com/cartesi/genext2fs) (cartesi's `xgenext2fs` 1.5.6). You can use it as:

- a **command line tool** (`mkext2`) that accepts xgenext2fs' options;
- a **Rust library** (`mkext2`);
- a **Node.js native module** (`@tuler/mkext2`), e.g. for the Cartesi CLI.

## Highlights

- **Valid ext2, same content as xgenext2fs.** Images pass `e2fsck` and hold the
  same files, metadata and data that xgenext2fs would store for the same command
  line; the test suite checks this against the real tool. (The bytes differ:
  mkext2 has its own layout.)
- **Reproducible.** The output depends only on the content and the options:
  directory entries are sorted, nothing random is written (UUID stays zero),
  and timestamps can be zeroed (`--faketime`), fixed (`--timestamp`) or
  clamped (`--clamp-timestamps`). Read-ahead threads never change the output.
- **Fast.** Building a 1 GiB image is about 7× faster than xgenext2fs with
  4 KiB blocks and about 90× faster with 1 KiB blocks (see
  [Performance](#performance)).
- **Inputs:** directories, tar archives (ustar, GNU incl. long names and old
  sparse files, pax incl. GNU sparse 0.0/0.1/1.0, v7; gzip, bzip2, xz and zstd
  compression, nested) and device tables. Layers can be combined.

## Command line

```
mkext2 [options] image
```

It takes the same options as xgenext2fs:

```bash
# Cartesi CLI style: 4 KiB blocks, zero timestamps, 1000 extra blocks
mkext2 --block-size 4096 --faketime --readjustment +1000 --root rootfs/ rootfs.ext2
mkext2 --block-size 4096 --faketime --readjustment +1000 --tarball rootfs.tar.gz rootfs.ext2
mkext2 --block-size 4096 --faketime --size-in-blocks 25600 data.ext2   # empty 100 MiB drive
```

| option | |
|---|---|
| `-x, --starting-image <image>` | add to an existing image |
| `-d, --root <directory>[:path]` | copy a directory tree into `path` (default `/`) |
| `-D, --devtable <file>[:path]` | add or fix up nodes from a device table |
| `-a, --tarball <file>[:path]` | copy a (possibly compressed) tar archive |
| `-B, --block-size <bytes>` | 1024 (default), 2048 or 4096 |
| `-b, --size-in-blocks <blocks>` | image size (computed from the inputs otherwise) |
| `-r, --readjustment <[+]N[%,k,ki,M,Mi,G,Gi]>` | free blocks to add over the computed size (`+N`), or an absolute size (`N`) |
| `-i, --bytes-per-inode <n>` | at least one free inode per `n` bytes of free space |
| `-N, --number-of-inodes <n>` | minimum number of inodes |
| `-L, --volume-label <label>` | volume label |
| `-m, --reserved-percentage <n>` | reserved blocks (default 5%) |
| `-o, --creator-os <os>` | `linux`, `hurd`, `freebsd`, `lites` or a number |
| `-g, --block-map <path>` | write `<path>.blk` with the blocks of `path` |
| `-e, --fill-value <n>` | fill unallocated blocks with byte `n` |
| `-z, --allow-holes` | don't allocate all-zero blocks of files |
| `-f, --faketime` | every timestamp is 0 |
| `-q, --squash` | `-U 0 -P` (`--squash <uid>` to choose the owner) |
| `-U, --squash-uids <uid>` | every file owned by `uid:uid` |
| `-P, --squash-perms` | remove group/other permissions |
| `--timestamp <seconds>` | *(new)* every timestamp, filesystem and files, is `seconds` |
| `--clamp-timestamps <seconds>` | *(new)* filesystem time is `seconds`; file times are clamped to it |
| `-v, --verbose` | print the resulting filesystem structure |

`image` can be `-` to write to standard output. Unless `--faketime`, `--timestamp` or
`--clamp-timestamps` is given, the filesystem creation time comes from
`SOURCE_DATE_EPOCH` (or the current time), and file times come from the sources.

### Reproducible builds

For images that depend on file contents only:

```bash
mkext2 -B 4096 --timestamp 0 --squash-uids 0 -r +1000 -d rootfs/ out.ext2   # or --faketime
```

This fixes all timestamps (including `ctime`, which you can't set on source files) and the
file owners. File permissions, contents, names, symlink targets, hard links and device
numbers are what remain.

## Rust library

```toml
[dependencies]
mkext2 = { git = "https://github.com/tuler/mkext2" }
```

```rust
use mkext2::{build_file, build_vec, Layer, Options, Readjustment, Timestamps};

let options = Options {
    block_size: 4096,
    timestamps: Timestamps::Zero,
    readjustment: Readjustment::parse("+1000")?,
    layers: vec![
        Layer::dir("rootfs"),
        Layer::tar("app.tar.gz").at("/opt/app"),
        Layer::devtable_bytes(b"/dev/console c 600 0 0 5 1\n".to_vec()),
    ],
    ..Options::default()
};
let report = build_file(&options, "rootfs.ext2")?;   // or build_vec(&options) for memory
println!("{} blocks, {} inodes", report.info.blocks, report.info.inodes);
```

`mkext2::reader::Image` reads images back, with `walk`, `lookup`, `read_file`,
`read_dir` and a consistency `check` (a small `e2fsck -n`).

Cargo features `gzip`, `bzip2`, `xz` and `zstd` (all on by default) select the
supported compressions.

## Node.js

```bash
npm install @tuler/mkext2      # or, from source: cd crates/mkext2-node && npm run build
```

```ts
import { build, buildBuffer, mkext2 } from '@tuler/mkext2'

// the command line, in process (takes the same arguments as xgenext2fs)
await mkext2(['--block-size', '4096', '--faketime', '--readjustment', '+1000', '--root', 'rootfs', 'out.ext2'])

// typed options (builds run on the libuv thread pool)
const info = await build('out.ext2', {
  blockSize: 4096,
  faketime: true,
  readjustment: '+1000',
  layers: [{ type: 'tar', path: 'rootfs.tar' }, { type: 'dir', path: 'extra', target: '/opt' }],
})
const image: Buffer = await buildBuffer({ blockSize: 4096, sizeInBlocks: 2560, faketime: true })
```

## Differences from xgenext2fs

Same command line, same content, except where xgenext2fs is wrong or limited:

| situation | xgenext2fs | mkext2 |
|---|---|---|
| `-d dir:/path` (or `-a`, `-D`) | ignores `:/path` unless `-x` | adds the content to `/path` |
| archive member naming an existing entry | changes the parent directory | updates that entry |
| symlink target ≥ block size | stored in a form Linux rejects | left out, with a warning |
| device numbers, uid/gid | truncated to 8 and 16 bits | kept in full |
| size computation | an estimate that can fail with "no space" | exact; `-r +N` guarantees N free blocks |
| short option `-q` | crashes | `-U 0 -P` |
| `-` as the image with stdout piped | fails | streams the image |
| `-e` with 2 or 4 KiB blocks | fills the blocks after free ones | fills the free blocks |
| AppleDouble `._*` tar members | platform dependent | always regular members |

The layout, the verbose (`-v`) output and the message wording are mkext2's own, so
images (and their hashes) differ from xgenext2fs'.

## Testing

The tests need the reference `xgenext2fs` 1.5.6 on the `PATH`, or `XGENEXT2FS`
pointing to it:

- Linux: install the `.deb` from https://github.com/cartesi/genext2fs/releases/tag/v1.5.6
- macOS: `brew install cartesi/tap/xgenext2fs` (and `gnu-tar` for the tar format tests)

```bash
cargo test --release                       # everything
STRESS_SEEDS=500 cargo test --release --test compat stress -- --ignored
(cd crates/mkext2-node && npm test)       # Node binding
```

- `compat`: builds the same inputs with xgenext2fs and mkext2; mkext2's image must
  be valid and hold the same content (`reader::compare_content`). It covers random trees (every indirection level, holes, hard links,
  FIFOs, sockets, and devices when run as root), every option, multiple block
  groups, wide directories, layering, starting images, device tables, stdin/stdout,
  failure cases, and tar archives in all formats and compressions (including sparse
  and hand-crafted ones).
- `roundtrip`: reads images back with the independent reader and checks content
  and metadata against the sources, plus the consistency check.
- `fsck`: `e2fsck -fn` on generated images (when e2fsck is installed; set `E2FSCK`
  if it isn't on the `PATH`).
- `reproducible`: same content gives the same image whatever the paths, creation
  order, file times or thread count; the timestamp policies.
- `cli`: command line behavior.

## Performance

`scripts/bench.sh [MiB]` generates a dataset shaped like a root filesystem and times
both tools (best of three runs), checking that both images are valid and hold the
same content. `scripts/bench.sh 1024` on an Apple M1 Max (macOS, APFS; 13,301 files
in 415 directories, 1 GiB):

| case | xgenext2fs 1.5.6 | mkext2 | speedup | same content |
|---|---:|---:|---:|---|
| directory (4 KiB blocks) | 2.39 s | 0.33 s | 7.3× | yes |
| directory (1 KiB blocks) | 43.40 s | 0.47 s | 91.6× | yes |
| tar (4 KiB blocks) | 2.24 s | 0.26 s | 8.6× | yes |
| tar.gz (4 KiB blocks) | 2.70 s | 1.17 s | 2.3× | yes |
| empty 1 GiB image | 0.01 s | 0.01 s | 1.1× | yes |

Compressed archives are decompressed twice (once to size the image, once to
copy the data), which dominates the `tar.gz` case.

## Releasing

Releases are automated with [Sampo](https://github.com/bruits/sampo) changesets and
published from CI with OIDC trusted publishing; see [RELEASING.md](RELEASING.md).

## License

Apache-2.0. mkext2 contains no code from genext2fs: its engine was written in a
clean room from a behavioral specification, and checked against xgenext2fs as a
black box. See [docs/CLEANROOM.md](docs/CLEANROOM.md) and [NOTICE](NOTICE).

Contributions are welcome under the same license; please read
[CONTRIBUTING.md](CONTRIBUTING.md) first.
