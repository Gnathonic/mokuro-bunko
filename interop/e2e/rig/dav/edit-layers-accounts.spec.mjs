// S2 (B8.6) on Bunko's own disk: edit layers across accounts, their rename and delete
// following the volume (layer-accounts.mjs).
import { test, expect } from '../fixtures.mjs';
import { editLayersAcrossAccounts } from '../layer-accounts.mjs';

test.use({ preflightsReachServers: true });

editLayersAcrossAccounts(test, expect, { inCloud: false });
