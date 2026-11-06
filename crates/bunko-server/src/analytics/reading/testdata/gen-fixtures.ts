// Golden fixtures for bunko's port of Mokuro Reader's history segment codec.
// Encoded and decoded by the reader's own segment-codec.ts (develop), bundled with
// esbuild. Synthetic data only: documentation device ids and made-up volume ids.
import { encodeSegment, decodeSegment } from '<reader>/src/lib/reading-history/segment-codec';
import type { ReadingEvent } from '<reader>/src/lib/reading-history/types';
import { writeFileSync, mkdirSync } from 'node:fs';

const out = process.argv[2];
mkdirSync(out, { recursive: true });
const DEV = 'device-a';
const V1 = '00000000-0000-4000-8000-0000000000a1';
const V2 = '00000000-0000-4000-8000-0000000000a2';
const T = Date.UTC(2026, 9, 3, 12); // 2026-10-03 12:00 UTC
const DAY = 86400000;

const native: ReadingEvent[] = [
  { device: DEV, seq: 4, t: T, kind: 'page', volume: V1, first_page: 3, last_page: 4,
    page_chars: [120, 0], chars_before: 300, dwell_ms: 41000, layout: 'double',
    orientation: 'landscape', viewport: { w: 1920, h: 1080 } },
  { device: DEV, seq: 5, t: T + 41000, kind: 'page', volume: V1, first_page: 5, last_page: 5,
    page_chars: [88], chars_before: 420, dwell_ms: null, layout: 'unknown',
    orientation: 'unknown', viewport: null },
  // A long pause: counted as 15 minutes at most.
  { device: DEV, seq: 6, t: T + 60000, kind: 'page', volume: V1, first_page: 6, last_page: 6,
    page_chars: [50.5], chars_before: 508, dwell_ms: 3600000, layout: 'single',
    orientation: 'portrait', viewport: { w: 800, h: 1280 } },
  { device: DEV, seq: 7, t: T + 3660000, kind: 'adjust', volume: V1, time_delta_ms: -60000, chars_delta: -10 },
  { device: DEV, seq: 9, t: T + 3661000, kind: 'restart', volume: V2 },
  { device: DEV, seq: 10, t: T + 3662000, kind: 'forget', volume: V1, before: T + 3662000 },
  { device: DEV, seq: 11, t: T + 3663000, kind: 'position', volume: V1, answer: 'stay', through: T, page: 6 },
  { device: DEV, seq: 12, t: T + 3664000, kind: 'resolve', volume: V1, target: [DEV, 6], count: 'typical' },
  // The next day, another volume.
  { device: DEV, seq: 13, t: T + DAY, kind: 'page', volume: V2, first_page: 1, last_page: 2,
    page_chars: [10, 20], chars_before: 0, dwell_ms: 90000, layout: 'double',
    orientation: 'landscape', viewport: { w: 1920, h: 1080 } },
  { device: DEV, seq: 14, t: T + DAY + 90000, kind: 'adjust', volume: V2, time_delta_ms: 30000, chars_delta: 5 },
];

const legacy: ReadingEvent[] = [
  { device: 'legacy:' + V2, seq: T + 5, t: T + 5, kind: 'restart', volume: V2 },
  { device: 'legacy:' + V1, seq: T - DAY, t: T - DAY, kind: 'page', volume: V1, first_page: 4, last_page: 4,
    page_chars: [0], chars_before: 400, dwell_ms: 30000, layout: 'unknown', orientation: 'unknown', viewport: null },
  { device: 'legacy:' + V1, seq: T - DAY + 30000, t: T - DAY + 30000, kind: 'page', volume: V1, first_page: 5, last_page: 5,
    page_chars: [0], chars_before: 480, dwell_ms: null, layout: 'unknown', orientation: 'unknown', viewport: null },
];

async function raw(obj: unknown): Promise<Uint8Array> {
  const stream = new Blob([JSON.stringify(obj)]).stream().pipeThrough(new CompressionStream('deflate-raw'));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

async function main() {
  const a = await encodeSegment(DEV, '2026-10', native);
  writeFileSync(`${out}/device-a-2026-10.events`, a);
  writeFileSync(`${out}/device-a-2026-10.json`, JSON.stringify(await decodeSegment(a), null, 1) + '\n');
  const l = await encodeSegment(DEV, 'legacy', legacy, { legacy: true });
  writeFileSync(`${out}/device-a-legacy.events`, l);
  writeFileSync(`${out}/device-a-legacy.json`, JSON.stringify(await decodeSegment(l), null, 1) + '\n');
  // A second converting device uploads the same legacy page events on its own path.
  const b = await encodeSegment('device-b', 'legacy', legacy.slice(1), { legacy: true });
  writeFileSync(`${out}/device-b-legacy.events`, b);
  writeFileSync(`${out}/device-b-legacy.json`, JSON.stringify(await decodeSegment(b), null, 1) + '\n');
  const empty = await encodeSegment(DEV, '2026-11', []);
  writeFileSync(`${out}/device-a-2026-11-empty.events`, empty);
  writeFileSync(`${out}/device-a-2026-11-empty.json`, JSON.stringify(await decodeSegment(empty), null, 1) + '\n');
  // A newer format the port must refuse (the reader does too).
  writeFileSync(`${out}/format-2.events`, await raw({ format: 2, device: DEV, month: '2026-10',
    first_seq: 1, last_seq: 1, count: 0, volumes: [], rows: [] }));
  let refused = '';
  try { await decodeSegment(await raw({ format: 2 })); } catch (e) { refused = String(e); }
  console.log('format 2 refused by the reader:', refused);
}
main();
