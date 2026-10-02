# Maintainer: Gerhard Schwanzer <geri@sdf.org>
pkgname=pw-duck-smooth
pkgver=0.2.5
pkgrel=1
pkgdesc="PipeWire tray app that ducks non-voice audio with a smooth volume release"
arch=('x86_64')
url="https://github.com/geri1701/pw-duck"
license=('MIT')
depends=(
  'coreutils'
  'gtk4'
  'hicolor-icon-theme'
  'libpulse'
  'pipewire'
  'pipewire-pulse'
  'timeout'
)
makedepends=(
  'cargo'
  'clang'
  'desktop-file-utils'
  'pkgconf'
)
optdepends=(
  'wireplumber: recommended PipeWire session manager'
  'gnome-shell-extension-appindicator: tray support on GNOME Shell'
)
conflicts=('pw-duck' 'pw-duck-git')
options=('!lto' '!debug')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('9f0c5d4ccecd66b5afae7b3dbbfdd52a0b332e6625189ff4eae10ce0c0a62c40')

# NOTE: this fork has no public remote yet, so `url`/`source` still point at the
# upstream project. Switch both to the fork repository before publishing, then
# run scripts/update-aur-checksum.sh to refresh the checksum.

prepare() {
  cd "$pkgname-$pkgver"
  cargo fetch --locked --target "$CARCH-unknown-linux-gnu"
}

build() {
  cd "$pkgname-$pkgver"
  CARGO_TARGET_DIR=target cargo build --release --locked --features gui
}

check() {
  cd "$pkgname-$pkgver"
  CARGO_TARGET_DIR=target cargo test --release --locked --features gui
  desktop-file-validate assets/applications/pw-duck-smooth.desktop
}

package() {
  cd "$pkgname-$pkgver"

  install -Dm755 target/release/pw-duck-smooth "$pkgdir/usr/bin/pw-duck-smooth"
  install -Dm644 assets/applications/pw-duck-smooth.desktop \
    "$pkgdir/usr/share/applications/pw-duck-smooth.desktop"

  mkdir -p "$pkgdir/usr/share/icons"
  cp -r assets/icons/hicolor "$pkgdir/usr/share/icons/hicolor"

  install -Dm644 README.md "$pkgdir/usr/share/doc/pw-duck-smooth/README.md"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/pw-duck-smooth/LICENSE"
}