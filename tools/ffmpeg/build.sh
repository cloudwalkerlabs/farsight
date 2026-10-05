#!/usr/bin/env bash
# Builds the small, static FFmpeg (and dav1d) that release builds link:
# libavcodec and libavutil with only the codecs and hardware APIs farsight
# uses, LGPL only (never --enable-gpl or --enable-nonfree). On Linux, also
# a static libxkbcommon for the server.
#
#   tools/ffmpeg/build.sh PREFIX
#
# Then build with PKG_CONFIG_PATH=PREFIX/lib/pkgconfig and FFMPEG_DIR unset.
# PREFIX/notice/ gets the licences and a notice to ship alongside.
#
# Needs a C compiler, make, pkg-config, nasm (x86_64), meson and ninja. On
# Windows, run it from an MSYS2 shell with the MSVC environment loaded.
set -euo pipefail

FFMPEG_VERSION=9.0.2
DAV1D_VERSION=1.5.4
XKBCOMMON_VERSION=1.13.2

prefix=${1:?usage: build.sh PREFIX}
mkdir -p "$prefix"
prefix=$(cd "$prefix" && pwd)

case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    MINGW* | MSYS*) os=windows ;;
    *) echo "unsupported system: $(uname -s)" >&2; exit 1 ;;
esac

# MSVC's tools and the .pc files that cargo reads want Windows paths.
native_prefix=$prefix
[ "$os" = windows ] && native_prefix=$(cygpath -m "$prefix")

jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

curl -fsSL "https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.xz" | tar xJ
curl -fsSL "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" | tar xJ

# meson would pick MinGW's gcc from the MSYS2 shell over MSVC.
[ "$os" = windows ] && export CC=cl
meson setup dav1d-build "dav1d-$DAV1D_VERSION" \
    --prefix="$native_prefix" --libdir=lib --buildtype=release \
    -Ddefault_library=static -Denable_tools=false -Denable_tests=false
ninja -C dav1d-build install

# MSVC's linker takes `-ldav1d` (FFmpeg's configure) and `-l static=avcodec`
# (rustc) as dav1d.lib and avcodec.lib, while meson's and FFmpeg's static
# libraries are named libdav1d.a and libavcodec.a.
msvc_names() {
    [ "$os" = windows ] || return 0
    for a in "$prefix"/lib/lib*.a; do
        name=$(basename "$a" .a)
        cp "$a" "$prefix/lib/${name#lib}.lib"
    done
}
msvc_names

decoders=h264,hevc,libdav1d
parsers=h264,hevc,av1
platform=()
case $os in
    linux)
        # The server encodes through VA-API, mapping DRM PRIME frames into
        # it; the client decodes with VA-API, AV1 too.
        decoders+=,av1
        platform=(--enable-vaapi --enable-libdrm
            --enable-hwaccel=h264_vaapi,hevc_vaapi,av1_vaapi
            --enable-encoder=h264_vaapi,hevc_vaapi,av1_vaapi)
        ;;
    macos)
        # AV1 stays in software off Linux (see crates/desktop/src/decode.rs).
        platform=(--enable-videotoolbox
            --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox)
        ;;
    windows)
        # -MD: the CRT Rust links (cl's default is the static one).
        platform=(--toolchain=msvc --extra-cflags=-MD --enable-d3d11va
            --enable-hwaccel=h264_d3d11va,hevc_d3d11va,h264_d3d11va2,hevc_d3d11va2)
        ;;
esac

configure=(
    --enable-static --disable-shared --enable-pic
    --disable-autodetect --disable-everything
    --disable-programs --disable-doc --disable-network --disable-debug
    --disable-avformat --disable-avfilter --disable-avdevice
    --disable-swscale --disable-swresample --disable-iconv
    --enable-libdav1d
    --enable-decoder="$decoders" --enable-parser="$parsers"
    "${platform[@]}"
)

# For dav1d. On Windows, pkg-config is a native program and wants
# Windows paths.
if [ "$os" = windows ]; then
    export PKG_CONFIG_PATH="$native_prefix/lib/pkgconfig"
else
    export PKG_CONFIG_PATH="$prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi
mkdir ffmpeg-build
cd ffmpeg-build
"../ffmpeg-$FFMPEG_VERSION/configure" --prefix="$native_prefix" "${configure[@]}" ||
    { tail -n 40 ffbuild/config.log; exit 1; }
make -j"$jobs"
make install
cd ..
msvc_names
# FFmpeg's MSVC toolchain writes -L and -lfoo into its .pc files as
# -libpath: and foo.lib, which the pkg-config crate doesn't understand.
if [ "$os" = windows ]; then
    sed -i -E 's/-libpath:/-L/g; s/ ([A-Za-z0-9_]+)\.lib\b/ -l\1/g' "$prefix"/lib/pkgconfig/libav*.pc
    cat "$prefix"/lib/pkgconfig/libav*.pc
fi


if [ "$os" = linux ]; then
    # farsight-va loads libva, libva-drm and libdrm when first called, for
    # FFmpeg too (crates/va/src/dlopen.c).
    sed -i -E 's/ -l(va-drm|va|drm)\b//g' "$prefix"/lib/pkgconfig/libav*.pc
    grep -H '^Libs' "$prefix"/lib/pkgconfig/libav*.pc

    # The keymaps and compose tables stay the system's.
    curl -fsSL "https://github.com/xkbcommon/libxkbcommon/archive/refs/tags/xkbcommon-$XKBCOMMON_VERSION.tar.gz" | tar xz
    meson setup xkbcommon-build "libxkbcommon-xkbcommon-$XKBCOMMON_VERSION" \
        --prefix="$prefix" --libdir=lib --buildtype=release \
        -Ddefault_library=static -Denable-tools=false -Denable-docs=false \
        -Denable-x11=false -Denable-wayland=false -Denable-xkbregistry=false -Denable-bash-completion=false \
        -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale \
        -Dxkb-config-extra-path=/etc/xkb
    ninja -C xkbcommon-build install
fi

mkdir -p "$prefix/notice"
cp "ffmpeg-$FFMPEG_VERSION/COPYING.LGPLv2.1" "$prefix/notice/"
cp "dav1d-$DAV1D_VERSION/COPYING" "$prefix/notice/COPYING.dav1d"
[ "$os" = linux ] && cp "libxkbcommon-xkbcommon-$XKBCOMMON_VERSION/LICENSE" "$prefix/notice/COPYING.xkbcommon"
cat >"$prefix/notice/FFMPEG.txt" <<EOF
farsight links FFmpeg $FFMPEG_VERSION (https://ffmpeg.org) statically,
under the GNU Lesser General Public License 2.1 (COPYING.LGPLv2.1), and
dav1d $DAV1D_VERSION (https://code.videolan.org/videolan/dav1d), under the
BSD 2-clause licence (COPYING.dav1d).

FFmpeg's source: https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz
It was configured with:

  configure ${configure[*]}

To link farsight against a modified FFmpeg, build it from its source
(https://github.com/cloudwalkerlabs/farsight) with tools/ffmpeg/build.sh changed
to build yours.
EOF
