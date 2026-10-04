/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
package org.touchhle.android;

import android.content.Context;
import androidx.work.Constraints;
import androidx.work.ExistingPeriodicWorkPolicy;
import androidx.work.ExistingWorkPolicy;
import androidx.work.NetworkType;
import androidx.work.PeriodicWorkRequest;
import androidx.work.OneTimeWorkRequest;
import androidx.work.WorkManager;
import java.io.BufferedReader;
import java.io.File;
import java.io.FileInputStream;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.TimeUnit;
import org.json.JSONObject;

final class CloudSyncScheduler {
    private static final String PERIODIC_WORK = "touchhle-cloud-sync-periodic";
    private static final String IMMEDIATE_WORK = "touchhle-cloud-sync-now";

    private CloudSyncScheduler() {}

    static void refreshFromDisk(Context context) {
        setEnabled(context, isEnabled(context));
    }

    static boolean isEnabled(Context context) {
        File root = context.getExternalFilesDir(null);
        if (root == null) {
            return false;
        }
        File settings = new File(root, ".touchHLE_sync/settings.json");
        try (BufferedReader reader =
                new BufferedReader(
                        new InputStreamReader(
                                new FileInputStream(settings), StandardCharsets.UTF_8))) {
            StringBuilder json = new StringBuilder();
            String line;
            while ((line = reader.readLine()) != null) {
                json.append(line);
            }
            return new JSONObject(json.toString()).optBoolean("enabled", false);
        } catch (Exception exception) {
            return false;
        }
    }

    static void setEnabled(Context context, boolean enabled) {
        WorkManager manager = WorkManager.getInstance(context.getApplicationContext());
        if (!enabled) {
            manager.cancelUniqueWork(IMMEDIATE_WORK);
            manager.cancelUniqueWork(PERIODIC_WORK);
            return;
        }

        Constraints constraints =
                new Constraints.Builder()
                        .setRequiredNetworkType(NetworkType.CONNECTED)
                        .build();
        PeriodicWorkRequest periodic =
                new PeriodicWorkRequest.Builder(
                                CloudSyncWorker.class, 15, TimeUnit.MINUTES)
                        .setConstraints(constraints)
                        .build();
        manager.enqueueUniquePeriodicWork(
                PERIODIC_WORK, ExistingPeriodicWorkPolicy.KEEP, periodic);
    }

    static void enqueueNow(Context context) {
        enqueueNow(context, ExistingWorkPolicy.KEEP);
    }

    static void enqueueAfterGuest(Context context) {
        enqueueNow(context, ExistingWorkPolicy.APPEND_OR_REPLACE);
    }

    private static void enqueueNow(Context context, ExistingWorkPolicy policy) {
        if (!isEnabled(context)) {
            return;
        }
        Constraints constraints =
                new Constraints.Builder()
                        .setRequiredNetworkType(NetworkType.CONNECTED)
                        .build();
        OneTimeWorkRequest request =
                new OneTimeWorkRequest.Builder(CloudSyncWorker.class)
                        .setConstraints(constraints)
                        .setBackoffCriteria(
                                androidx.work.BackoffPolicy.EXPONENTIAL,
                                30,
                                TimeUnit.SECONDS)
                        .build();
        WorkManager.getInstance(context.getApplicationContext())
                .enqueueUniqueWork(IMMEDIATE_WORK, policy, request);
    }
}
