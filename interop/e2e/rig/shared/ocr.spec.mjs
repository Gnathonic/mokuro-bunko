// shared (§7.1, M7; queue file v2 §12.4): Bunko OCRs a volume the reader uploaded
// straight to the cloud they share. The reader imports an image-only archive and backs
// it up; Bunko, holding the library's lease, finds it in the cloud and queues its OCR,
// which the rig's processor-fake claims and holds until the hold file exists
// (RIG_FAKEPROC_HOLD). The flow itself is shared with the local-folder variant
// (ocr-flow.mjs).
import { test } from '../fixtures.mjs';
import { connect } from './shared.mjs';
import { cloudStore, ocrRoundTrip } from './ocr-flow.mjs';

test.use({ needs: ['series-queue-v2'] });

test('Bunko OCRs a volume the reader uploaded: one series file read, its job on the card, its sidecar pulled', async ({
    reader,
    rig,
}) => {
    test.setTimeout(600_000);
    await ocrRoundTrip({ reader, rig, signIn: () => connect(reader, rig), store: cloudStore(rig), wire: true });
});
