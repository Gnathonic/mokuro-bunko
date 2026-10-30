// S3 (B8.6) on Bunko's own disk: reading history and positions across accounts
// (history-accounts.mjs).
import { test, expect } from '../fixtures.mjs';
import { historyAcrossAccounts } from '../history-accounts.mjs';

test.use({ preflightsReachServers: true });

historyAcrossAccounts(test, expect, { inCloud: false });
