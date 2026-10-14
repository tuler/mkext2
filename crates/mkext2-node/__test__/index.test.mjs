import { test } from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, symlinkSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const ext2 = require('../index.js')

// the mkext2 command line tool (cargo build --release) is the reference here:
// the binding must produce exactly the same images
const CLI =
  process.env.MKEXT2 || resolve(dirname(fileURLToPath(import.meta.url)), '../../../target/release/mkext2')

function makeTree() {
  const dir = mkdtempSync(join(tmpdir(), 'mkext2-node-'))
  const root = join(dir, 'root')
  mkdirSync(join(root, 'etc'), { recursive: true })
  mkdirSync(join(root, 'usr/bin'), { recursive: true })
  writeFileSync(join(root, 'etc/hostname'), 'cartesi\n')
  writeFileSync(join(root, 'usr/bin/app'), Buffer.alloc(300_000, 7))
  symlinkSync('usr/bin', join(root, 'bin'))
  return { dir, root }
}

function reference(args, cwd) {
  execFileSync(CLI, args, { cwd, stdio: 'ignore', env: { ...process.env, SOURCE_DATE_EPOCH: '1' } })
}

test('version', () => {
  assert.match(ext2.version(), /^\d+\.\d+\.\d+/)
})

test('mkext2() with Cartesi CLI arguments matches the command line tool', async () => {
  const { dir, root } = makeTree()
  try {
    const args = ['--block-size', '4096', '--faketime', '--readjustment', '+1000', '--root', root]
    reference([...args, join(dir, 'ref.ext2')], dir)
    const r = await ext2.mkext2([...args, join(dir, 'new.ext2')])
    assert.ok(r.result.blocks > 0)
    assert.equal(typeof r.stderr, 'string')
    assert.ok(readFileSync(join(dir, 'ref.ext2')).equals(readFileSync(join(dir, 'new.ext2'))))
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('build() with options matches the command line tool', async () => {
  const { dir, root } = makeTree()
  try {
    reference(['-B', '4096', '-f', '-r', '+500', '-d', root, join(dir, 'ref.ext2')], dir)
    const res = await ext2.build(join(dir, 'new.ext2'), {
      blockSize: 4096,
      faketime: true,
      readjustment: '+500',
      layers: [{ type: 'dir', path: root }],
    })
    assert.equal(res.blockSize, 4096)
    assert.ok(res.groups >= 1)
    assert.ok(res.messages.some((m) => m.level === 'info'))
    assert.ok(readFileSync(join(dir, 'ref.ext2')).equals(readFileSync(join(dir, 'new.ext2'))))

    const sync = ext2.buildSync(join(dir, 'sync.ext2'), {
      blockSize: 4096,
      faketime: true,
      readjustment: '+500',
      layers: [{ type: 'dir', path: root }],
    })
    assert.equal(sync.blocks, res.blocks)
    assert.ok(readFileSync(join(dir, 'sync.ext2')).equals(readFileSync(join(dir, 'new.ext2'))))
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('tar archives from files and buffers', async () => {
  const { dir, root } = makeTree()
  try {
    const tarPath = join(dir, 'root.tar')
    execFileSync('tar', ['-cf', tarPath, '-C', root, '.'], { env: { ...process.env, COPYFILE_DISABLE: '1' } })
    reference(['-B', '4096', '-f', '-r', '+500', '-a', tarPath, join(dir, 'ref.ext2')], dir)
    const fromBuffer = await ext2.buildBuffer({
      blockSize: 4096,
      faketime: true,
      readjustment: '+500',
      layers: [{ type: 'tar', data: readFileSync(tarPath) }],
    })
    assert.ok(Buffer.isBuffer(fromBuffer))
    assert.ok(readFileSync(join(dir, 'ref.ext2')).equals(fromBuffer))
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('layer targets and fixed timestamps', async () => {
  const { dir, root } = makeTree()
  try {
    const img = await ext2.buildBuffer({
      blockSize: 1024,
      timestamp: 1700000000,
      squashUids: 0,
      readjustment: '+300',
      layers: [
        { type: 'devtable', data: Buffer.from('/opt d 755 0 0\n/opt/app d 755 0 0\n') },
        { type: 'dir', path: root, target: '/opt/app' },
      ],
    })
    // superblock s_wtime at offset 1024 + 48
    assert.equal(img.readUInt32LE(1024 + 48), 1700000000)
    // the same build twice is identical
    const again = await ext2.buildBuffer({
      blockSize: 1024,
      timestamp: 1700000000,
      squashUids: 0,
      readjustment: '+300',
      layers: [
        { type: 'devtable', data: Buffer.from('/opt d 755 0 0\n/opt/app d 755 0 0\n') },
        { type: 'dir', path: root, target: '/opt/app' },
      ],
    })
    assert.ok(img.equals(again))
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('errors reject with a message', async () => {
  await assert.rejects(ext2.build('/tmp/x.ext2', { blockSize: 1000, layers: [] }), /1000/)
  await assert.rejects(
    ext2.build('/tmp/x.ext2', { layers: [{ type: 'dir', path: '/does/not/exist' }] }),
    /does\/not\/exist/,
  )
  await assert.rejects(ext2.mkext2(['--bogus', 'x.img']), /bogus/)
  assert.throws(() => ext2.buildSync('/tmp/x.ext2', { readjustment: '+5x' }), /readjustment/)
  assert.throws(() => ext2.buildSync('/tmp/x.ext2', { layers: [{ type: 'zip', path: 'a' }] }), /unknown layer type/)
})

test('builds run off the main thread', async () => {
  const { dir, root } = makeTree()
  try {
    let ticks = 0
    const timer = setInterval(() => ticks++, 1)
    const p = ext2.build(join(dir, 'a.ext2'), {
      blockSize: 1024,
      faketime: true,
      sizeInBlocks: 2_000_000,
      fillValue: 7,
      layers: [{ type: 'dir', path: root }],
    })
    await p
    clearInterval(timer)
    assert.ok(ticks > 0, 'event loop should keep running during the build')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})
