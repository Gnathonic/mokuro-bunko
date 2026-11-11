// `npm run selftest`: the fixtures' self-test (caps.spec.mjs), then a check of what
// Playwright reported. Playwright exits 0 when a test is skipped, so a gate that
// started skipping specs it should fail or run would pass unnoticed; here every
// skipped test must be one of EXPECTED_SKIPS, and each of those must have skipped.
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const EXPECTED_SKIPS = [
    'caps.spec.mjs › a missing capability the pending list names › is skipped, with the capability reported',
];

const interop = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const dir = mkdtempSync(path.join(tmpdir(), 'rig-selftest-'));
const json = path.join(dir, 'report.json');
try {
    const args = ['playwright', 'test', '--config', 'e2e/rig/playwright.config.mjs', ...process.argv.slice(2)];
    const run = spawnSync('npx', args, {
        cwd: interop,
        stdio: 'inherit',
        env: { ...process.env, RIG_SUITE: 'selftest', RIG_PW_JSON: json },
    });
    if (run.status !== 0) process.exit(run.status ?? 1);
    const skipped = [];
    const walk = (suite, titles) => {
        for (const spec of suite.specs ?? []) {
            for (const t of spec.tests) {
                if (t.status === 'skipped') skipped.push([...titles, spec.title].join(' › '));
            }
        }
        for (const child of suite.suites ?? []) walk(child, [...titles, child.title]);
    };
    for (const file of JSON.parse(readFileSync(json, 'utf8')).suites) walk(file, [path.basename(file.file)]);
    const unexpected = skipped.filter((t) => !EXPECTED_SKIPS.includes(t));
    const missing = EXPECTED_SKIPS.filter((t) => !skipped.includes(t));
    if (unexpected.length || missing.length) {
        console.error(
            `selftest: skipped unexpectedly: ${JSON.stringify(unexpected)}; did not skip: ${JSON.stringify(missing)}`,
        );
        process.exit(1);
    }
} finally {
    rmSync(dir, { recursive: true, force: true });
}
