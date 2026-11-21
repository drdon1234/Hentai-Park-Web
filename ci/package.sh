#!/usr/bin/env bash
set -euo pipefail
root="$PWD"
out="$root/test"
project="$root/candidate/clients/apple/Park.xcodeproj"
mkdir -p "$out/packages" "$out/Payload"
build_app() {
  xcodebuild -project "$project" -scheme HentaiPark -configuration Release \
    -destination "generic/platform=$1" -derivedDataPath "$out/derived" \
    "${@:2}" build
}
build_app iOS CODE_SIGNING_ALLOWED=NO
cp -R "$out/derived/Build/Products/Release-iphoneos/HentaiPark.app" "$out/Payload/"
(cd "$out" && /usr/bin/zip -qry packages/HentaiPark-ios-unsigned.ipa Payload)
build_app macOS CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM=
mac="$out/derived/Build/Products/Release/HentaiPark.app"
codesign --verify --deep --strict "$mac"
ditto -c -k --keepParent "$mac" "$out/packages/HentaiPark-macOS-adhoc.zip"
build_app 'iOS Simulator' CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual CODE_SIGN_ENTITLEMENTS=
ditto -c -k --keepParent "$out/derived/Build/Products/Release-iphonesimulator/HentaiPark.app" "$out/packages/HentaiPark-ios-simulator.zip"
(cd "$out/packages" && shasum -a 256 *.ipa *.zip > SHA256SUMS)
