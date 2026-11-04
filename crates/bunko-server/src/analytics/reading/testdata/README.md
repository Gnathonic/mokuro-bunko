# Reading-history segment fixtures

Golden files for the port of Mokuro Reader's segment codec (`../codec.rs`). Each
`.events` file was written by the reader's own `encodeSegment`, and the `.json` beside
it is the reader's own `decodeSegment` output for it, from the reader's `develop` branch
at 085453c2 (`src/lib/reading-history/segment-codec.ts`). `format-2.events` is a raw
deflated header of a format the reader refuses (`unsupported segment format 2`).

The data is synthetic: devices `device-a` and `device-b` (whose `legacy.events` holds
the same converted events as device-a's, as every converting device uploads them),
made-up volume ids.

To make them again: export the reader's sources without touching its working tree,
`git -C <reader> archive develop src/lib/reading-history | tar -x -C <dir>`, point the
imports of `gen-fixtures.ts` at `<dir>`, bundle it with the reader's esbuild
(`esbuild gen-fixtures.ts --bundle --platform=node --format=esm --outfile=gen.mjs`) and
run `node gen.mjs <out dir>`.
