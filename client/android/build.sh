#!/usr/bin/env bash
# Build the Emersia Quest client APK without Gradle (ADR 0007).
#
#   client/android/build.sh            → client/android/build/emersia.apk
#
# Needs: Android SDK (platforms;android-34, build-tools;35.x, ndk;27.x), a JDK,
# and a Rust toolchain with the aarch64-linux-android target. Locations come
# from ANDROID_HOME / ANDROID_NDK_HOME / CARGO, with fallbacks below.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
out=$here/build
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/.local/share/android-sdk}}
platform=${ANDROID_PLATFORM:-android-34}
android_jar=$sdk/platforms/$platform/android.jar
build_tools=${ANDROID_BUILD_TOOLS:-$(ls -d "$sdk"/build-tools/* 2>/dev/null | sort -V | tail -1)}
ndk=${ANDROID_NDK_HOME:-$(ls -d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -1)}
min_api=29

die() { echo "build.sh: $*" >&2; exit 1; }
[[ -f $android_jar ]] || die "no $android_jar (sdkmanager \"platforms;$platform\")"
[[ -x $build_tools/aapt2 ]] || die "no build-tools under $sdk/build-tools"
[[ -d $ndk ]] || die "no NDK under $sdk/ndk (sdkmanager \"ndk;27.2.12479018\")"

# A cargo whose toolchain has the Android target: $CARGO, else rustup's, else
# a user-local rustup kept apart from a distro Rust.
cargo=${CARGO:-}
if [[ -z $cargo ]]; then
  if command -v rustup >/dev/null && rustup target list --installed | grep -q aarch64-linux-android; then
    cargo=cargo
  elif [[ -x $HOME/.local/share/emersia-cargo/bin/cargo ]]; then
    export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.local/share/rustup}
    export CARGO_HOME=${CARGO_HOME:-$HOME/.local/share/emersia-cargo}
    cargo=$CARGO_HOME/bin/cargo
  else
    die "no Rust toolchain with aarch64-linux-android (rustup target add aarch64-linux-android, or set CARGO)"
  fi
fi

tc=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$tc/aarch64-linux-android${min_api}-clang
export CC_aarch64_linux_android=$tc/aarch64-linux-android${min_api}-clang
export AR_aarch64_linux_android=$tc/llvm-ar

echo "==> native core (Rust, aarch64-linux-android)"
"$cargo" build --release --target aarch64-linux-android --manifest-path "$here/native/Cargo.toml"
so=$here/native/target/aarch64-linux-android/release/libemersia_client.so

rm -rf "$out" && mkdir -p "$out/classes" "$out/apk/lib/arm64-v8a"

echo "==> java"
javac --release 11 -Xlint:-options -classpath "$android_jar" -d "$out/classes" \
  $(find "$here/java" -name '*.java')
"$build_tools/d8" --release --min-api $min_api --lib "$android_jar" \
  --output "$out/apk" $(find "$out/classes" -name '*.class')

echo "==> package"
"$build_tools/aapt2" link -o "$out/base.apk" -I "$android_jar" \
  --manifest "$here/AndroidManifest.xml" --min-sdk-version $min_api --target-sdk-version 32
cp "$so" "$out/apk/lib/arm64-v8a/"
# Append dex + native lib to the aapt2 output (python's zipfile, so no `zip`
# dependency).
python3 - "$out/base.apk" "$out/apk" <<'PY'
import os, sys, zipfile
apk, root = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(apk, "a", zipfile.ZIP_DEFLATED) as z:
    for dirpath, _, files in os.walk(root):
        for f in sorted(files):
            path = os.path.join(dirpath, f)
            z.write(path, os.path.relpath(path, root))
PY
"$build_tools/zipalign" -f -p 4 "$out/base.apk" "$out/aligned.apk"

# Debug signing key, generated once per checkout (never committed).
keystore=${EMERSIA_KEYSTORE:-$here/debug.keystore}
if [[ ! -f $keystore ]]; then
  keytool -genkeypair -keystore "$keystore" -storepass android -keypass android \
    -alias emersia-debug -keyalg RSA -keysize 3072 -validity 10000 \
    -dname "CN=Emersia Debug" >/dev/null 2>&1
fi
"$build_tools/apksigner" sign --ks "$keystore" --ks-pass pass:android \
  --key-pass pass:android --out "$out/emersia.apk" "$out/aligned.apk"
rm -f "$out/base.apk" "$out/aligned.apk" "$out"/*.idsig
echo "==> $out/emersia.apk ($(du -h "$out/emersia.apk" | cut -f1))"
