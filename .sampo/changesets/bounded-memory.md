---
cargo/mkext2: patch
npm/@tuler/mkext2: patch
---

Bound peak memory when building into a file: about 110 MiB for directory
trees and 40 MiB for archives, whatever the image size (it grew with the
image before, up to 900 MiB on macOS for a 1 GiB tree). Buffered writes use a
fixed pool of 8 MiB buffers, and read-ahead reads whole files in 4 MiB pieces
into one 64 MiB ring instead of allocating per file. Read-ahead uses up to 4
threads by default (8 before), which is faster on macOS and Linux because
more readers mostly wait on each other opening files. `Device` gains
`write_space`, a provided method that buffering devices implement so that
file data is read straight into their buffers. Output is unchanged.
