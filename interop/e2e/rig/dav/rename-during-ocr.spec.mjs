// S6 (B8.6) on Bunko's own disk, the reader signed in to Bunko: a volume renamed while
// Bunko's processor holds its OCR gets its sidecar under the new name, and never a
// numbered one (rename-ocr.mjs).
import { test, expect } from '../fixtures.mjs';
import { renameDuringOcr } from '../rename-ocr.mjs';

test.use({ preflightsReachServers: true });

renameDuringOcr(test, expect, { inCloud: false });
