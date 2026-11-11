// S2 (B8.6) on a cloud library Bunko alone talks to: edit layers across accounts, their
// rename and delete following the volume (layer-accounts.mjs). The curated layer lands
// in the cloud and follows the volume there; no account's own edit ever does.
import { test, expect } from '../fixtures.mjs';
import { editLayersAcrossAccounts } from '../layer-accounts.mjs';

editLayersAcrossAccounts(test, expect, { inCloud: true });
