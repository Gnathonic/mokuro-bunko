// S3 (B8.6) on a cloud library Bunko alone talks to: reading history and positions
// across accounts (history-accounts.mjs). Nobody owns this cloud's root, so no
// account's history or positions are ever written into the cloud.
import { test, expect } from '../fixtures.mjs';
import { historyAcrossAccounts } from '../history-accounts.mjs';

historyAcrossAccounts(test, expect, { inCloud: true });
