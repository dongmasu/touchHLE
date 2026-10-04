/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
package org.touchhle.android;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertTrue;

import org.junit.Test;

public class CloudSyncWorkerTest {
    @Test
    public void nativeCompletionIsSuccessful() {
        assertEquals(
                CloudSyncWorker.NativeResult.COMPLETE,
                CloudSyncWorker.classifyNativeResult(0));
    }

    @Test
    public void activeGuestKeepsDurableRetryAfterProcessDeath() {
        assertEquals(
                CloudSyncWorker.NativeResult.GUEST_ACTIVE,
                CloudSyncWorker.classifyNativeResult(1));
        assertTrue(CloudSyncWorker.shouldRetry(CloudSyncWorker.classifyNativeResult(1)));
    }

    @Test
    public void missingAuthorizationWaitsForForegroundWithoutRetryStorm() {
        assertEquals(
                CloudSyncWorker.NativeResult.AUTH_NEEDED,
                CloudSyncWorker.classifyNativeResult(3));
        assertFalse(CloudSyncWorker.shouldRetry(CloudSyncWorker.classifyNativeResult(3)));
    }

    @Test
    public void unknownOrTransientNativeResultRetries() {
        assertEquals(
                CloudSyncWorker.NativeResult.RETRY,
                CloudSyncWorker.classifyNativeResult(2));
        assertEquals(
                CloudSyncWorker.NativeResult.RETRY,
                CloudSyncWorker.classifyNativeResult(-1));
        assertTrue(CloudSyncWorker.shouldRetry(CloudSyncWorker.classifyNativeResult(-1)));
    }
}
