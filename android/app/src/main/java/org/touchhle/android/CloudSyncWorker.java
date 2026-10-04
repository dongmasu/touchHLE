/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
package org.touchhle.android;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.content.Context;
import android.content.pm.ServiceInfo;
import android.os.Build;
import androidx.annotation.NonNull;
import androidx.core.app.NotificationCompat;
import androidx.work.ForegroundInfo;
import androidx.work.Worker;
import androidx.work.WorkerParameters;
import java.io.File;
import java.util.concurrent.ExecutionException;

public final class CloudSyncWorker extends Worker {
    private static final String CHANNEL_ID = "touchhle-cloud-sync";
    private static final int NOTIFICATION_ID = 0x5448;

    enum NativeResult {
        COMPLETE,
        GUEST_ACTIVE,
        AUTH_NEEDED,
        RETRY
    }

    static NativeResult classifyNativeResult(int result) {
        if (result == 0) {
            return NativeResult.COMPLETE;
        }
        if (result == 1) {
            return NativeResult.GUEST_ACTIVE;
        }
        if (result == 3) {
            return NativeResult.AUTH_NEEDED;
        }
        return NativeResult.RETRY;
    }

    static boolean shouldRetry(NativeResult result) {
        return result == NativeResult.GUEST_ACTIVE || result == NativeResult.RETRY;
    }

    public CloudSyncWorker(@NonNull Context context, @NonNull WorkerParameters parameters) {
        super(context, parameters);
    }

    @NonNull
    @Override
    public Result doWork() {
        Context context = getApplicationContext();
        File root = context.getExternalFilesDir(null);
        if (root == null || !CloudSyncScheduler.isEnabled(context)) {
            return Result.success();
        }

        try {
            setForegroundAsync(createForegroundInfo(context)).get();
            NativeResult result = classifyNativeResult(
                    NativeSyncBridge.runScheduledSync(context, root.getAbsolutePath()));
            return shouldRetry(result) ? Result.retry() : Result.success();
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            return Result.retry();
        } catch (ExecutionException | RuntimeException exception) {
            return Result.retry();
        }
    }

    private ForegroundInfo createForegroundInfo(Context context) {
        NotificationManager manager =
                (NotificationManager) context.getSystemService(Context.NOTIFICATION_SERVICE);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager.createNotificationChannel(
                    new NotificationChannel(
                            CHANNEL_ID,
                            "Google Drive sync",
                            NotificationManager.IMPORTANCE_LOW));
        }
        Notification notification =
                new NotificationCompat.Builder(context, CHANNEL_ID)
                        .setSmallIcon(android.R.drawable.stat_sys_upload)
                        .setContentTitle("touchHLE Google Drive sync")
                        .setContentText("Syncing saved game data")
                        .setOngoing(true)
                        .setOnlyAlertOnce(true)
                        .build();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            return new ForegroundInfo(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
        }
        return new ForegroundInfo(NOTIFICATION_ID, notification);
    }
}
