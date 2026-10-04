/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * Parts of this file are derived from SDL 2's Android project template, which
 * has a different license. Please see vendor/SDL/LICENSE.txt for details.
 */
package org.touchhle.android;

import android.content.Context;
import android.content.Intent;
import android.content.IntentSender;
import android.content.res.ColorStateList;
import android.graphics.Color;
import android.graphics.Typeface;
import android.app.AlertDialog;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.LinearLayout;
import android.widget.ProgressBar;
import android.widget.TextView;
import com.google.android.gms.auth.api.identity.AuthorizationRequest;
import com.google.android.gms.auth.api.identity.AuthorizationResult;
import com.google.android.gms.auth.api.identity.Identity;
import com.google.android.gms.common.api.Scope;
import java.util.Collections;
import org.libsdl.app.SDLActivity;

/**
 * A wrapper class over SDLActivity
 */

public class MainActivity extends SDLActivity {
    private static final String GOOGLE_AUTH_TAG = "TouchHLEAuth";
    private static final int GOOGLE_AUTH_REQUEST = 0x5448;
    private static final String GOOGLE_DRIVE_SCOPE = "https://www.googleapis.com/auth/drive.file";
    private static final long DEFAULT_ACCESS_TOKEN_LIFETIME_SECONDS = 3600;

    private static native void setAuthorizationActivity(MainActivity activity);
    private static native void nativeOAuthResult(
            long requestId, String accessToken, long expiresUnixSeconds, String error);
    private static native void nativeOfflineSyncDecision(
            long requestId, boolean continueOffline);

    private long activeAuthorizationRequestId = -1;
    private boolean activeAuthorizationAllowsResolution;
    private LinearLayout cloudSyncOverlay;
    private TextView cloudSyncHeadline;
    private TextView cloudSyncMessage;
    private TextView[] cloudSyncSteps;
    private ProgressBar cloudSyncProgress;

    @Override
    protected void onCreate(android.os.Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        NativeSyncBridge.initializeContext(getApplicationContext());
        setAuthorizationActivity(this);
        createCloudSyncOverlay();
        CloudSyncScheduler.refreshFromDisk(this);
    }

    @Override
    protected void onStop() {
        CloudSyncScheduler.enqueueNow(this);
        super.onStop();
    }

    public void updateCloudSyncSchedule(boolean enabled) {
        CloudSyncScheduler.setEnabled(getApplicationContext(), enabled);
    }

    public void enqueueCloudSyncNow() {
        CloudSyncScheduler.enqueueAfterGuest(getApplicationContext());
    }

    public void updateCloudSyncOverlay(
            String headline, String detail, int activeStep, int trackProgress, boolean visible) {
        runOnUiThread(
                () -> {
                    if (cloudSyncOverlay == null) {
                        return;
                    }
                    cloudSyncHeadline.setText(headline);
                    cloudSyncMessage.setText(detail);
                    cloudSyncProgress.setProgress(Math.max(0, Math.min(526, trackProgress)));
                    int activeColor = Color.rgb(78, 205, 180);
                    int inactiveColor = Color.rgb(172, 184, 188);
                    for (int i = 0; i < cloudSyncSteps.length; i++) {
                        cloudSyncSteps[i].setTextColor(i <= activeStep ? activeColor : inactiveColor);
                    }
                    cloudSyncOverlay.setVisibility(visible ? View.VISIBLE : View.GONE);
                });
    }

    public void requestOfflineSyncDecision(long requestId) {
        runOnUiThread(
                () ->
                        new AlertDialog.Builder(this)
                                .setTitle("Google Drive sync is unavailable")
                                .setMessage(
                                        "Retry the sync, or continue with local files for this"
                                                + " session? Google Drive sync will retry later.")
                                .setPositiveButton(
                                        "Retry",
                                        (dialog, which) ->
                                                nativeOfflineSyncDecision(requestId, false))
                                .setNegativeButton(
                                        "Continue offline",
                                        (dialog, which) ->
                                                nativeOfflineSyncDecision(requestId, true))
                                .setCancelable(false)
                                .show());
    }

    private void createCloudSyncOverlay() {
        cloudSyncOverlay = new LinearLayout(this);
        cloudSyncOverlay.setOrientation(LinearLayout.VERTICAL);
        cloudSyncOverlay.setGravity(Gravity.CENTER);
        cloudSyncOverlay.setPadding(32, 32, 32, 32);
        cloudSyncOverlay.setBackgroundColor(Color.rgb(16, 24, 30));
        cloudSyncOverlay.setClickable(true);
        cloudSyncOverlay.setFocusable(true);
        cloudSyncOverlay.setOnClickListener(view -> {});

        TextView title = new TextView(this);
        title.setText("Google Drive Sync");
        title.setTextColor(Color.WHITE);
        title.setTextSize(24);
        title.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        title.setGravity(Gravity.CENTER);
        cloudSyncOverlay.addView(
                title,
                new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.WRAP_CONTENT));

        cloudSyncHeadline = new TextView(this);
        cloudSyncHeadline.setTextColor(Color.WHITE);
        cloudSyncHeadline.setTextSize(18);
        cloudSyncHeadline.setGravity(Gravity.CENTER);
        LinearLayout.LayoutParams headlineParams =
                new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.WRAP_CONTENT);
        headlineParams.topMargin = 24;
        cloudSyncOverlay.addView(cloudSyncHeadline, headlineParams);

        cloudSyncMessage = new TextView(this);
        cloudSyncMessage.setTextColor(Color.rgb(220, 226, 228));
        cloudSyncMessage.setTextSize(14);
        cloudSyncMessage.setGravity(Gravity.CENTER);
        cloudSyncMessage.setMaxLines(3);
        LinearLayout.LayoutParams messageParams =
                new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.WRAP_CONTENT);
        messageParams.topMargin = 10;
        cloudSyncOverlay.addView(cloudSyncMessage, messageParams);

        cloudSyncProgress =
                new ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal);
        cloudSyncProgress.setIndeterminate(false);
        cloudSyncProgress.setMax(526);
        cloudSyncProgress.setProgress(66);
        cloudSyncProgress.setProgressTintList(
                ColorStateList.valueOf(Color.rgb(78, 205, 180)));
        cloudSyncProgress.setProgressBackgroundTintList(
                ColorStateList.valueOf(Color.rgb(55, 66, 71)));
        LinearLayout.LayoutParams progressParams =
                new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT, 8);
        progressParams.topMargin = 24;
        cloudSyncOverlay.addView(cloudSyncProgress, progressParams);

        LinearLayout steps = new LinearLayout(this);
        steps.setOrientation(LinearLayout.HORIZONTAL);
        steps.setGravity(Gravity.CENTER);
        cloudSyncSteps = new TextView[4];
        String[] stepLabels = {"Local scan", "Drive check", "Transfer", "Apply"};
        for (int i = 0; i < stepLabels.length; i++) {
            TextView step = new TextView(this);
            step.setText(stepLabels[i]);
            step.setTextColor(i == 0 ? Color.rgb(78, 205, 180) : Color.rgb(172, 184, 188));
            step.setTextSize(11);
            step.setGravity(Gravity.CENTER);
            step.setSingleLine(true);
            cloudSyncSteps[i] = step;
            steps.addView(
                    step,
                    new LinearLayout.LayoutParams(
                            0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
        }
        LinearLayout.LayoutParams stepsParams =
                new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.WRAP_CONTENT);
        stepsParams.topMargin = 12;
        cloudSyncOverlay.addView(steps, stepsParams);

        TextView keepOpen = new TextView(this);
        keepOpen.setText("Please keep touchHLE open until sync finishes.");
        keepOpen.setTextColor(Color.rgb(172, 184, 188));
        keepOpen.setTextSize(12);
        keepOpen.setGravity(Gravity.CENTER);
        LinearLayout.LayoutParams keepOpenParams =
                new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.WRAP_CONTENT);
        keepOpenParams.topMargin = 24;
        cloudSyncOverlay.addView(keepOpen, keepOpenParams);

        mLayout.addView(
                cloudSyncOverlay,
                new ViewGroup.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT));
        cloudSyncOverlay.setVisibility(View.GONE);
    }

    @Override
    public void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != GOOGLE_AUTH_REQUEST) {
            return;
        }

        long requestId = activeAuthorizationRequestId;
        activeAuthorizationRequestId = -1;
        Log.i(
                GOOGLE_AUTH_TAG,
                "authorization activity returned: requestId="
                        + requestId
                        + ", resultCode="
                        + resultCode
                        + ", hasData="
                        + (data != null));
        if (resultCode != RESULT_OK || data == null) {
            nativeOAuthResult(requestId, "", 0, "cancelled");
            return;
        }

        try {
            AuthorizationResult result =
                    Identity.getAuthorizationClient(this).getAuthorizationResultFromIntent(data);
            completeAuthorization(requestId, result);
        } catch (Exception exception) {
            finishAuthorization(requestId, "", 0, authorizationError(exception));
        }
    }

    public void requestGoogleAuthorization(long requestId, boolean allowResolution) {
        Log.i(GOOGLE_AUTH_TAG, "requestGoogleAuthorization entered");
        runOnUiThread(
                () -> {
                    Log.i(GOOGLE_AUTH_TAG, "authorization UI task started");
                    try {
                        beginGoogleAuthorization(requestId, allowResolution);
                    } catch (RuntimeException | LinkageError exception) {
                        logAuthorizationFailure("synchronous", exception);
                        finishAuthorization(
                                requestId, "", 0, "Google authorization failed");
                    }
                });
    }

    private void beginGoogleAuthorization(long requestId, boolean allowResolution) {
        Log.i(GOOGLE_AUTH_TAG, "beginGoogleAuthorization started");
        if (activeAuthorizationRequestId != -1) {
            finishAuthorization(requestId, "", 0, "another Google authorization is in progress");
            return;
        }
        activeAuthorizationRequestId = requestId;
        activeAuthorizationAllowsResolution = allowResolution;

        AuthorizationRequest request =
                AuthorizationRequest.builder()
                        .setRequestedScopes(
                                Collections.singletonList(new Scope(GOOGLE_DRIVE_SCOPE)))
                        .build();
        Log.i(GOOGLE_AUTH_TAG, "calling Google Identity authorize");
        Identity.getAuthorizationClient(this)
                .authorize(request)
                .addOnSuccessListener(
                        result -> {
                            Log.i(GOOGLE_AUTH_TAG, "Google Identity authorization succeeded");
                            completeAuthorization(requestId, result);
                        })
                .addOnFailureListener(
                        exception -> {
                            finishAuthorization(
                                    requestId, "", 0, authorizationError(exception));
                        });
        Log.i(GOOGLE_AUTH_TAG, "Google Identity authorize task created");
    }

    private void completeAuthorization(long requestId, AuthorizationResult result) {
        Log.i(
                GOOGLE_AUTH_TAG,
                "authorization result received: requestId="
                        + requestId
                        + ", hasResolution="
                        + result.hasResolution());
        if (result.hasResolution()) {
            if (!activeAuthorizationAllowsResolution) {
                finishAuthorization(
                        requestId,
                        "",
                        0,
                        "Google authorization needs user input; run touchHLE graphically");
                return;
            }
            try {
                IntentSender intentSender = result.getPendingIntent().getIntentSender();
                startAuthorizationResolution(requestId, intentSender);
            } catch (Exception exception) {
                finishAuthorization(requestId, "", 0, authorizationError(exception));
            }
            return;
        }

        long expiresAt =
                System.currentTimeMillis() / 1000 + DEFAULT_ACCESS_TOKEN_LIFETIME_SECONDS;
        String accessToken = result.getAccessToken();
        Log.i(
                GOOGLE_AUTH_TAG,
                "authorization returned access token: requestId="
                        + requestId
                        + ", tokenPresent="
                        + (accessToken != null && !accessToken.isEmpty()));
        finishAuthorization(requestId, accessToken == null ? "" : accessToken, expiresAt, "");
    }

    private void startAuthorizationResolution(long requestId, IntentSender intentSender) {
        try {
            startIntentSenderForResult(
                    intentSender, GOOGLE_AUTH_REQUEST, null, 0, 0, 0);
        } catch (IntentSender.SendIntentException exception) {
            finishAuthorization(requestId, "", 0, authorizationError(exception));
        }
    }

    private void finishAuthorization(
            long requestId, String accessToken, long expiresUnixSeconds, String error) {
        if (requestId == activeAuthorizationRequestId) {
            activeAuthorizationRequestId = -1;
        }
        Log.i(
                GOOGLE_AUTH_TAG,
                "delivering authorization result: requestId="
                        + requestId
                        + ", tokenPresent="
                        + !accessToken.isEmpty()
                        + ", errorPresent="
                        + !error.isEmpty());
        nativeOAuthResult(requestId, accessToken, expiresUnixSeconds, error);
    }

    private String authorizationError(Throwable exception) {
        logAuthorizationFailure("authorization", exception);
        return "Google authorization failed";
    }

    private void logAuthorizationFailure(String stage, Throwable exception) {
        Log.e(
                GOOGLE_AUTH_TAG,
                stage
                        + " failure: "
                        + exception.getClass().getName()
                        + ": "
                        + exception.getMessage());
    }

    @Override
    protected String[] getLibraries() {
        return new String[]{
            "SDL2",
            "touchHLE"
        };
    }
}
