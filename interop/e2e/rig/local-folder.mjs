// The reader's Local Folder on the `folder` provider's library folder (queue file v2
// §12.4). The reader opens a folder only through the File System Access picker
// (`showDirectoryPicker`), which Playwright cannot answer (Chromium aborts it as
// "Intercepted by Page.setInterceptFileChooserDialog()"). What a rig can do instead:
//
// - drop the folder on the page (CDP `Input.dispatchDragEvent` with its path): the page
//   gets a real handle to that folder on disk (`DataTransferItem.getAsFileSystemHandle`),
//   which the reader's driver hands its Local Folder in place of the picker
//   (`__mokuroInterop.connectFolder`, capability `local-folder`);
// - let the reader's origin write to it without the prompt a user would answer: full
//   Chromium (`channel: 'chromium'`; the headless shell denies it) on a profile whose
//   File System Access write guard allows that origin (`folderProfile`). The headless
//   shell, and a profile without it, answer `requestPermission({mode: 'readwrite'})`
//   with `denied`.
import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { chromium } from '@playwright/test';

/** Chromium's content setting value for "allow". */
const ALLOW = 1;

/**
 * A Chromium profile's `Default/Preferences`: File System Access writes allowed for
 * `origin` (the reader's), with no prompt.
 */
export function folderProfile(origin) {
    const pattern = `${new URL(origin).origin},*`;
    return {
        profile: {
            content_settings: {
                exceptions: { file_system_write_guard: { [pattern]: { setting: ALLOW } } },
            },
        },
    };
}

/**
 * A browser context for the folder provider: full Chromium on a fresh profile in `dir`
 * that lets `readerUrl`'s origin write to a folder it is given, launched with the
 * project's own options (`use`: headless, launch arguments, service workers, viewport).
 */
export async function launchFolderContext(dir, readerUrl, use) {
    mkdirSync(path.join(dir, 'Default'), { recursive: true });
    writeFileSync(path.join(dir, 'Default', 'Preferences'), JSON.stringify(folderProfile(readerUrl)));
    return chromium.launchPersistentContext(dir, {
        channel: 'chromium',
        headless: use.headless ?? true,
        args: use.launchOptions?.args ?? [],
        serviceWorkers: use.serviceWorkers,
        viewport: use.viewport,
    });
}

/** The CDP drag events of dropping the folder at `dir` on the page at (x, y). */
export function folderDrop(dir, x = 10, y = 10) {
    const data = { items: [], files: [dir], dragOperationsMask: 1 };
    return ['dragEnter', 'dragOver', 'drop'].map((type) => ({ type, x, y, data }));
}

/**
 * Sign the reader in to its Local Folder on the folder at `dir`: the driver's
 * `connectFolder` listens for a drop (in place once the first evaluate returns), the
 * folder is dropped on the page, and its connect is awaited. Resolves with the
 * driver's `{status, sync}`.
 */
export async function connectFolder(reader, dir) {
    const page = reader.page;
    await page.evaluate(() => {
        const pending = globalThis.__mokuroInterop.connectFolder();
        pending.catch(() => {});
        globalThis.__rigFolderConnect = pending;
    });
    const cdp = await page.context().newCDPSession(page);
    try {
        for (const event of folderDrop(dir)) await cdp.send('Input.dispatchDragEvent', event);
    } finally {
        await cdp.detach().catch(() => {});
    }
    return page.evaluate(() => globalThis.__rigFolderConnect);
}
