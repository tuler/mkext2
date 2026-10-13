# @tuler/mkext2

Reproducible ext2 filesystem images from Node.js, without root privileges.
This is the native binding of [mkext2](https://github.com/tuler/mkext2): it takes the
same arguments as cartesi's `xgenext2fs` 1.5.6 and stores the same content, in valid,
reproducible images, faster.

```ts
import { build, buildBuffer, xgenext2fs } from '@tuler/mkext2'

// drop-in replacement for spawning xgenext2fs
await xgenext2fs(['--block-size', '4096', '--faketime', '--readjustment', '+1000', '--root', 'rootfs', 'out.ext2'])

// typed options (builds run on the libuv thread pool)
const info = await build('out.ext2', {
  blockSize: 4096,
  faketime: true,
  readjustment: '+1000',
  layers: [{ type: 'tar', path: 'rootfs.tar' }, { type: 'dir', path: 'extra', target: '/opt' }],
})
const image: Buffer = await buildBuffer({ blockSize: 4096, sizeInBlocks: 2560, faketime: true })
```

Prebuilt binaries: macOS (x64, arm64), Linux glibc ≥ 2.17 (x64, arm64), Windows x64.
See the [repository](https://github.com/tuler/mkext2) for all options.
