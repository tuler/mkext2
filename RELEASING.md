# Releasing

mkext2 ships as:

| registry | package | contents |
|---|---|---|
| crates.io | [`mkext2`](https://crates.io/crates/mkext2) | library and CLI |
| npm | [`@tuler/mkext2`](https://www.npmjs.com/package/@tuler/mkext2) | JS loader and types |
| npm | `@tuler/mkext2-{darwin-x64,darwin-arm64,linux-x64-gnu,linux-arm64-gnu,win32-x64-msvc}` | native binaries, pulled in as `optionalDependencies` |

All seven always share one version (a Sampo `fixed` group in `.sampo/config.toml`).
The `mkext2-node` crate is internal and never published.

## Routine releases (with Sampo)

Versions and changelogs come from [Sampo](https://github.com/bruits/sampo) changesets.

1. With your change, add a changeset and commit it:

   ```bash
   npx sampo add            # or: cargo install sampo && sampo add
   ```

   Pick `cargo/mkext2` (the others follow automatically) and the bump (`patch`,
   `minor`, `major`). This writes `.sampo/changesets/<name>.md`:

   ```markdown
   ---
   cargo/mkext2: minor
   ---

   Add a `--foo` option.
   ```

2. Merge to `main`. The **Release** workflow opens (or refreshes) a
   **Release (main)** pull request that bumps the versions and updates the
   changelogs.
3. Merge the release PR. The workflow then builds the native binaries for every
   platform and publishes:
   - the platform packages to npm (`scripts/publish-npm.sh`);
   - then, through Sampo, the crate to crates.io and `@tuler/mkext2` to npm;
   - then tags (`cargo-mkext2-vX.Y.Z`, `npm-@tuler/mkext2-vX.Y.Z`, …) and GitHub
     releases.

Pull requests opened with the workflow token don't trigger other workflows, so
CI doesn't run on the release PR by itself. Close and reopen it (or push to it)
if you want CI there.

## How publishing stays safe

- **No long-lived tokens.** crates.io and npm use OIDC
  [trusted publishing](https://docs.npmjs.com/trusted-publishers). The registries
  only accept tokens minted for `tuler/mkext2`, workflow `release.yml`,
  environment `release`.
- **The `release` environment only accepts the `main` branch.** Workflows from
  other branches or forks can't obtain publish tokens.
- **Least privilege.** Builds run in read-only jobs. Only the `release` job has
  `id-token: write`, and it requests registry tokens only when a release is due.
  The crates.io token expires within 30 minutes and is revoked when the job ends.
- **Pinned code.** Every action in the release job is pinned to a commit SHA.
  Sampo is installed with `cargo install --locked` at a fixed version instead of
  through its action's `curl | bash` installer.
- **Guarded packages.** A platform package can't be published without its binary
  (`prepublishOnly` check). The publish script checks all versions agree before
  publishing anything, and skips versions already on npm, so a failed run can
  be re-run.

## First release (manual)

Trusted publishing can only be configured for packages that already exist, so
0.1.0 is published by hand once. CI publishing stays off until the repository
variable `PUBLISH_ENABLED` is `true`.

You need a crates.io account logged in with `cargo login`, and an npm account with
2FA logged in with `npm login`. You also need npm ≥ 11.15 (`npm --version`) for
`npm trust`.

### 1. Build the native binaries

The Linux and Windows binaries are built by CI. Running the workflow manually
only builds; it never publishes.

```bash
gh workflow run release.yml -R tuler/mkext2
gh run watch -R tuler/mkext2 $(gh run list -R tuler/mkext2 -w release.yml -L 1 --json databaseId -q '.[0].databaseId')
rm -rf dist && gh run download -R tuler/mkext2 $(gh run list -R tuler/mkext2 -w release.yml -L 1 --json databaseId -q '.[0].databaseId') -D dist
ls -R dist   # binding-<platform>/mkext2.<platform>.node and js-binding/index.{js,d.ts}
```

### 2. Publish the crate

From an up-to-date clean checkout of `main`:

```bash
cargo publish -p mkext2 --dry-run
cargo publish -p mkext2
```

### 3. Publish the npm packages

```bash
NPM_PUBLISH_DRY_RUN=1 scripts/publish-npm.sh dist --main   # check first
scripts/publish-npm.sh dist --main
```

This publishes the five platform packages, then `@tuler/mkext2`. npm asks for 2FA
on each publish; in the browser prompt you can allow skipping it for the next
5 minutes.

### 4. Trust the release workflow

npm, for all six packages:

```bash
for p in @tuler/mkext2 @tuler/mkext2-darwin-x64 @tuler/mkext2-darwin-arm64 \
         @tuler/mkext2-linux-x64-gnu @tuler/mkext2-linux-arm64-gnu @tuler/mkext2-win32-x64-msvc; do
  npm trust github "$p" --repo tuler/mkext2 --file release.yml --env release --allow-publish --yes
  sleep 2
done
npm trust list @tuler/mkext2
```

Then, for extra safety, open each package's settings on npmjs.com and set
**Publishing access** to *Require two-factor authentication and disallow tokens*.
Trusted publishing keeps working, and stolen npm tokens can no longer publish.

crates.io: go to <https://crates.io/crates/mkext2/settings> → **Trusted
Publishing** → **Add**, and enter:

| field | value |
|---|---|
| Publisher | GitHub |
| Repository owner | `tuler` |
| Repository name | `mkext2` |
| Workflow filename | `release.yml` |
| Environment | `release` |

### 5. Turn on CI publishing

```bash
gh variable set PUBLISH_ENABLED --body true -R tuler/mkext2
```

Optionally, tag 0.1.0 the way Sampo tags later releases. Sampo only tags what it
publishes itself, so it won't do this for you:

```bash
git tag cargo-mkext2-v0.1.0 && git tag npm-@tuler/mkext2-v0.1.0
git push origin cargo-mkext2-v0.1.0 npm-@tuler/mkext2-v0.1.0
gh release create cargo-mkext2-v0.1.0 -R tuler/mkext2 --title "mkext2 0.1.0" --notes "First release."
```

From now on, follow [Routine releases](#routine-releases-with-sampo).
