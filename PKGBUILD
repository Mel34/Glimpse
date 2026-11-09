# Maintainer: Mel34
pkgname=glimpse
pkgver=0.1.0
pkgrel=1
pkgdesc="Linux media player using GTK4, libadwaita, FFmpeg, and native PipeWire audio"
arch=('x86_64' 'aarch64')
url="https://github.com/Mel34/Glimpse"
license=('LGPL-2.1-or-later')
depends=(
  'ffmpeg'
  'gtk4'
  'libadwaita'
  'alsa-lib'
  'libpulse'
  'pipewire'
)
makedepends=(
  'cargo'
  'rust'
  'pkgconf'
  'clang'
)
optdepends=(
  'libva: for future hardware accelerated decoding (VA-API on AMD / Intel)'
  'mesa: AMD Radeon VA-API / Gallium drivers'
  'libass: for advanced ASS/SSA subtitle rendering'
)
# Build directly from local repository sources
source=()
sha256sums=()

prepare() {
  cd "$startdir"
  export RUSTUP_TOOLCHAIN=stable
  export BINDGEN_EXTRA_CLANG_ARGS="-I$PWD/include"
  cargo fetch --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$startdir"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR="$startdir/target"
  export BINDGEN_EXTRA_CLANG_ARGS="-I$PWD/include"
  cargo build --release --all-targets
}

check() {
  cd "$startdir"
  export RUSTUP_TOOLCHAIN=stable
  export BINDGEN_EXTRA_CLANG_ARGS="-I$PWD/include"
  cargo test --release
}

package() {
  cd "$startdir"
  install -Dm755 target/release/glimpse "$pkgdir/usr/bin/glimpse"
  if [ -f target/release/glimpse_probe ]; then
    install -Dm755 target/release/glimpse_probe "$pkgdir/usr/bin/glimpse-probe"
  fi
  if [ -f data/com.github.Mel34.Glimpse.desktop ]; then
    install -Dm644 data/com.github.Mel34.Glimpse.desktop "$pkgdir/usr/share/applications/com.github.Mel34.Glimpse.desktop"
  fi
  if [ -f data/com.github.Mel34.Glimpse.svg ]; then
    install -Dm644 data/com.github.Mel34.Glimpse.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/com.github.Mel34.Glimpse.svg"
  fi
  if [ -f data/com.github.Mel34.Glimpse-symbolic.svg ]; then
    install -Dm644 data/com.github.Mel34.Glimpse-symbolic.svg "$pkgdir/usr/share/icons/hicolor/symbolic/apps/com.github.Mel34.Glimpse-symbolic.svg"
  fi
  if [ -f data/com.github.Mel34.Glimpse.metainfo.xml ]; then
    install -Dm644 data/com.github.Mel34.Glimpse.metainfo.xml "$pkgdir/usr/share/metainfo/com.github.Mel34.Glimpse.metainfo.xml"
  fi
  if [ -f LICENSE ]; then
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  fi
}