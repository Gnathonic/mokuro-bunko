// S4 (B8.6) on a cloud a Bunko holds, the reader on the cloud itself: renamed volumes
// and their series keep the provenance block Bunko sealed, an edited volume its edited
// marker, and Bunko queues nothing for them (rename-ocr.mjs).
import { test, expect } from '../fixtures.mjs';
import { renameKeepsProvenance } from '../rename-ocr.mjs';

renameKeepsProvenance(test, expect, { inCloud: true });
