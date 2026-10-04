/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
package org.touchhle.android;

import android.content.Context;
import com.google.android.gms.auth.api.identity.AuthorizationRequest;
import com.google.android.gms.auth.api.identity.AuthorizationResult;
import com.google.android.gms.auth.api.identity.Identity;
import com.google.android.gms.common.api.Scope;
import org.libsdl.app.SDL;
import java.util.Collections;

final class NativeSyncBridge {
    private static final String GOOGLE_DRIVE_SCOPE =
            "https://www.googleapis.com/auth/drive.file";
    private static final long ACCESS_TOKEN_LIFETIME_SECONDS = 3600;

    private static volatile Context applicationContext;
    private static boolean librariesLoaded;

    private NativeSyncBridge() {}

    static void initializeContext(Context context) {
        applicationContext = context.getApplicationContext();
        initializeKeyringContext(applicationContext);
    }

    static synchronized int runScheduledSync(Context context, String rootPath) {
        try {
            if (!librariesLoaded) {
                SDL.loadLibrary("SDL2");
                SDL.loadLibrary("touchHLE");
                librariesLoaded = true;
            }
            initializeContext(context);
            return nativeRunScheduledSync(rootPath);
        } catch (LinkageError | RuntimeException exception) {
            return 2;
        }
    }

    static void requestGoogleAuthorization(long requestId) {
        Context context = applicationContext;
        if (context == null) {
            nativeOAuthResult(requestId, "", 0, "Android context is unavailable");
            return;
        }

        AuthorizationRequest request =
                AuthorizationRequest.builder()
                        .setRequestedScopes(
                                Collections.singletonList(new Scope(GOOGLE_DRIVE_SCOPE)))
                        .build();
        Identity.getAuthorizationClient(context)
                .authorize(request)
                .addOnSuccessListener(result -> completeAuthorization(requestId, result))
                .addOnFailureListener(
                        exception ->
                                nativeOAuthResult(
                                        requestId, "", 0, "Google authorization failed"));
    }

    private static void completeAuthorization(long requestId, AuthorizationResult result) {
        if (result.hasResolution()) {
            nativeOAuthResult(requestId, "", 0, "foreground_required");
            return;
        }
        long expiresAt =
                System.currentTimeMillis() / 1000 + ACCESS_TOKEN_LIFETIME_SECONDS;
        String accessToken = result.getAccessToken();
        nativeOAuthResult(
                requestId, accessToken == null ? "" : accessToken, expiresAt, "");
    }

    private static native void initializeKeyringContext(Context context);

    private static native int nativeRunScheduledSync(String rootPath);

    private static native void nativeOAuthResult(
            long requestId, String accessToken, long expiresUnixSeconds, String error);
}
