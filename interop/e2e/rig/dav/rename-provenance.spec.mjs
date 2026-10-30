// S4 (B8.6) on Bunko's own disk, the reader signed in to Bunko: renamed volumes and
// their series keep the provenance block Bunko sealed, an edited volume its edited
// marker, and Bunko queues nothing for them (rename-ocr.mjs).
import { test, expect } from '../fixtures.mjs';
import { renameKeepsProvenance } from '../rename-ocr.mjs';

test.use({ preflightsReachServers: true });

renameKeepsProvenance(test, expect, { inCloud: false });
