# Contributing

Thanks for helping with mkext2! A few rules keep the project healthy, and one
of them keeps its license valid.

## Do not use genext2fs code

mkext2 is licensed under Apache-2.0 and was written in a clean room, without
genext2fs' source (see [docs/CLEANROOM.md](docs/CLEANROOM.md)). genext2fs and its
forks, including cartesi's xgenext2fs, are GPL-licensed, so their code must stay
out of this project:

- do not copy, translate or paraphrase code, comments or messages from
  genext2fs, xgenext2fs or their forks, and do not consult their source while
  working on mkext2;
- running the `xgenext2fs` program as a black box (to compare its output, as
  the tests do) is fine;
- public format documentation (Linux kernel docs, e2fsprogs manuals, "The
  Second Extended File System") is fine.

If you have studied genext2fs' source, say so in your pull request, and keep
your contribution to areas unrelated to how it builds images (tooling,
bindings, documentation), or describe the change in an issue so someone else
can implement it.

By contributing, you agree that your contribution is licensed under the
Apache License 2.0 (see [LICENSE](LICENSE)).

## Development

```bash
cargo build --release
cargo test --release --workspace        # needs xgenext2fs, see below
STRESS_SEEDS=500 cargo test --release --test compat stress -- --ignored
(cd crates/mkext2-node && npm run build && npm test)
scripts/bench.sh 1024                    # speed against xgenext2fs
```

The tests compare mkext2 with the reference `xgenext2fs` 1.5.6, which must be
installed (or pointed to with `XGENEXT2FS`):

- Linux: the `.deb` from https://github.com/cartesi/genext2fs/releases/tag/v1.5.6
- macOS: `brew install cartesi/tap/xgenext2fs` (and `gnu-tar`)

`e2fsck` is used when available (`E2FSCK` overrides its path); CI always runs it
on Linux. Before sending a pull request, run `cargo fmt --all` and
`cargo clippy --workspace --all-targets -- -D warnings`.

## Changesets

User-visible changes need a changeset, so they get a version bump and a
changelog entry: run `npx sampo add`, pick `cargo/mkext2` and the bump, and
commit the file it creates. See [RELEASING.md](RELEASING.md).
