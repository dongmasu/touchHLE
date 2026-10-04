#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)
TOUCHHLE_ROOT=$(CDPATH= cd "$REPO_ROOT/../touchHLE" && pwd)
ANDROID_BUILD_ROOT="$TOUCHHLE_ROOT/.android-build"
JAVA_HOME="$ANDROID_BUILD_ROOT/jdk-17.0.20.1+1/Contents/Home"
ANDROID_SDK_ROOT="$ANDROID_BUILD_ROOT/android-sdk"
ANDROID_NDK_HOME="$ANDROID_SDK_ROOT/ndk/25.2.9519653"
GRADLE="$ANDROID_BUILD_ROOT/gradle/gradle-8.11.1/bin/gradle"
APK="$REPO_ROOT/android/app/build/outputs/apk/release/app-release.apk"

for required_path in \
    "$JAVA_HOME/bin/java" \
    "$ANDROID_SDK_ROOT" \
    "$ANDROID_NDK_HOME" \
    "$GRADLE"; do
    if [ ! -e "$required_path" ]; then
        printf 'Required Android build tool not found: %s\n' "$required_path" >&2
        exit 1
    fi
done

export JAVA_HOME
export ANDROID_HOME="$ANDROID_SDK_ROOT"
export ANDROID_SDK_ROOT
export ANDROID_NDK_HOME
export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"
export GRADLE_USER_HOME="${GRADLE_USER_HOME:-/private/tmp/touchHLE-android-gradle-home}"
export CMAKE_POLICY_VERSION_MINIMUM=3.5
PATH="$JAVA_HOME/bin:$PATH"
export PATH

cd "$REPO_ROOT/android"
"$GRADLE" :app:assembleRelease

if [ ! -s "$APK" ]; then
    printf 'Build completed but APK was not created: %s\n' "$APK" >&2
    exit 1
fi

printf 'Release APK: %s\n' "$APK"
ls -lh "$APK"
