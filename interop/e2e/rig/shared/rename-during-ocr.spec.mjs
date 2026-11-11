// S6 (B8.6) on a cloud a Bunko holds, the reader on the cloud itself: a volume renamed
// while Bunko's processor holds its OCR gets its sidecar under the new name, and never
// a numbered one (rename-ocr.mjs).
import { test, expect } from '../fixtures.mjs';
import { renameDuringOcr } from '../rename-ocr.mjs';

renameDuringOcr(test, expect, { inCloud: true });
