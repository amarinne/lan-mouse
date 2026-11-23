#!/bin/sh
# Build and run a Lan Mouse receiver on an Android device over ADB.
#
# The daemon runs as the `shell` user, which may open /dev/uinput, so Android
# sees a physical keyboard, mouse and touchpad. It stops at reboot; run
# `start` again afterwards. Select the device with ANDROID_SERIAL.
set -eu

DIR=/data/local/tmp/lan-mouse
TARGET=aarch64-linux-android
API=30
TARGET_DIR=${CARGO_TARGET_DIR:-target}
BIN=$TARGET_DIR/$TARGET/release/lan-mouse

usage() {
    cat <<EOF
USAGE: $0 build|install|start|stop|status|log

  build    cross-compile the uinput-only daemon (needs the Android NDK)
  install  push the daemon and a config that authorizes this machine, then start
  start    start the daemon (again after a reboot)
  stop     stop the daemon
  status   show the process and the virtual input device
  log      show the daemon log

ANDROID_NDK_HOME  NDK root (default: newest in ~/android-sdk/ndk or ~/Android/Sdk/ndk)
ANDROID_SERIAL    adb device to use
EOF
}

ndk_root() {
    if [ -n "${ANDROID_NDK_HOME:-}" ]; then
        echo "$ANDROID_NDK_HOME"
        return
    fi
    for base in "$HOME/android-sdk/ndk" "$HOME/Android/Sdk/ndk"; do
        if [ -d "$base" ]; then
            ls -d "$base"/* | sort -V | tail -n 1
            return
        fi
    done
    echo "set ANDROID_NDK_HOME" >&2
    exit 1
}

build() {
    tools=$(ndk_root)/toolchains/llvm/prebuilt/linux-x86_64/bin
    CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$tools/$TARGET$API-clang \
        CC_aarch64_linux_android=$tools/$TARGET$API-clang \
        AR_aarch64_linux_android=$tools/llvm-ar \
        cargo build --release --target "$TARGET" --no-default-features --features uinput_emulation
}

# SHA-256 of this machine's certificate, in the format of `authorized_fingerprints`
local_fingerprint() {
    pem=${XDG_CONFIG_HOME:-$HOME/.config}/lan-mouse/lan-mouse.pem
    openssl x509 -in "$pem" -noout -fingerprint -sha256 | sed 's/.*=//' | tr 'A-F' 'a-f'
}

start() {
    # `&` must apply to the daemon alone; a backgrounded list keeps adb attached
    adb shell "cd $DIR; umask 077; HOME=$DIR RUST_LOG=\${RUST_LOG:-lan_mouse=info,input_emulation=info} setsid nohup ./lan-mouse daemon > $DIR/lan-mouse.log 2>&1 < /dev/null &"
    sleep 1
    status
}

stop() {
    adb shell "pid=\$(pidof lan-mouse) && kill \$pid || true"
}

status() {
    adb shell "echo pid: \$(pidof lan-mouse || echo none); getevent -pl 2>/dev/null | grep -B1 'Lan Mouse' || echo 'no virtual device'"
}

install() {
    [ -x "$BIN" ] || build
    config=$(mktemp)
    trap 'rm -- "$config"' EXIT
    cat > "$config" <<EOF
port = 4242
capture_backend = "android"
emulation_backend = "uinput"

[authorized_fingerprints]
"$(local_fingerprint)" = "$(hostname -s)"
EOF
    stop
    adb shell "umask 077; mkdir -p $DIR/.config/lan-mouse"
    adb push "$BIN" "$DIR/lan-mouse" > /dev/null
    adb push "$config" "$DIR/.config/lan-mouse/config.toml" > /dev/null
    adb shell "chmod 700 $DIR $DIR/lan-mouse; chmod 600 $DIR/.config/lan-mouse/config.toml"
    start
}

case "${1:-}" in
    build) build ;;
    install) install ;;
    start) start ;;
    stop) stop ;;
    status) status ;;
    log) adb shell "cat $DIR/lan-mouse.log" ;;
    *) usage; exit 1 ;;
esac
