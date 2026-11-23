"""在 Xcode 主机上构建 iOS 设备、模拟器和 macOS 的匹配绑定及静态库。"""
import os
import json
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
APPLE = ROOT / 'clients/apple'
CARGO = ROOT / 'build/rust'
SLICES = {
    'iphoneos': ('aarch64-apple-ios',),
    'iphonesimulator': ('aarch64-apple-ios-sim', 'x86_64-apple-ios'),
    'macosx': ('aarch64-apple-darwin', 'x86_64-apple-darwin'),
}


def run(*args, env=None, cwd=ROOT):
    subprocess.run(args, cwd=cwd, env=env, check=True)


def apple_identity():
    version = re.search(r'^version:\s*(\d+\.\d+\.\d+)\+(\d+)\s*$', (ROOT / 'pubspec.yaml').read_text(encoding='utf8'), re.M)
    identifiers = set()
    for name in ('Debug', 'Release'):
        config = (ROOT / 'ios/Flutter' / (name + '.xcconfig')).read_text(encoding='utf8')
        match = re.search(r'^PRODUCT_BUNDLE_IDENTIFIER\s*=\s*([A-Za-z0-9.-]+)\s*$', config, re.M)
        if match:
            identifiers.add(match[1])
        else:
            raise ValueError('原 iOS 配置缺少 Bundle ID')
    if not version or len(identifiers) != 1:
        raise ValueError('应用版本或原 iOS 安装身份不一致')
    return f'PRODUCT_BUNDLE_IDENTIFIER = {identifiers.pop()}\nMARKETING_VERSION = {version[1]}\nCURRENT_PROJECT_VERSION = {version[2]}\n'


def source_resources(destination):
    directory = ROOT / 'assets/sources'
    destination.mkdir(parents=True)
    index = directory / 'index.json'
    names = [row['file'] for row in json.loads(index.read_bytes())['sources']]
    if len(set(names)) != len(names) or not all(re.fullmatch(r'[\w-]+\.js', name, re.ASCII) for name in names):
        raise ValueError('内置站点清单含无效文件名')
    for name in [index.name, *names]:
        shutil.copyfile(directory / name, destination / name)
    for pack in sorted((ROOT / 'assets/tag_translations').glob('*/*.json')):
        target = destination / 'tag_translations' / pack.parent.name / pack.name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(pack, target)
    run('node', 'clients/web/scripts/build-polyfills.mjs', destination / 'park_polyfills.js')


def build(stage):
    common = dict(os.environ, CARGO_TARGET_DIR=str(CARGO), IPHONEOS_DEPLOYMENT_TARGET='15.0',
                  MACOSX_DEPLOYMENT_TARGET='12.0')
    bindings = stage / 'bindings'
    headers = stage / 'headers'
    headers.mkdir()
    framework_inputs = []
    for sdk, targets in SLICES.items():
        sysroot = subprocess.check_output(['xcrun', '--sdk', sdk, '--show-sdk-path'], text=True).strip()
        libraries = []
        for target in targets:
            env = dict(common, SDKROOT=sysroot)
            run('cargo', 'build', '--locked', '--release', '-p', 'park-ffi', '--target', target, env=env)
            output = CARGO / target / 'release'
            libraries.append(output / 'libpark_ffi.a')
            generated = stage / target
            run('cargo', 'run', '--locked', '-p', 'park-bindgen', '--', 'generate',
                output / 'libpark_ffi.dylib', '--language', 'swift', '--out-dir', generated,
                '--no-format', env=common)
            if not bindings.exists():
                shutil.copytree(generated, bindings)
                shutil.copyfile(bindings / 'park_ffiFFI.h', headers / 'park_ffiFFI.h')
                shutil.copyfile(bindings / 'park_ffiFFI.modulemap', headers / 'module.modulemap')
            else:
                for path in bindings.iterdir():
                    if path.read_bytes() != (generated / path.name).read_bytes():
                        raise RuntimeError('不同目标的绑定元数据不一致')
        fat = stage / sdk / 'libpark_ffi.a'
        fat.parent.mkdir()
        run('xcrun', 'lipo', '-create', *libraries, '-output', fat)
        framework_inputs += ['-library', fat, '-headers', headers]
    package = stage / 'ParkCore'
    package.mkdir()
    shutil.copyfile(APPLE / 'Package.swift', package / 'Package.swift')
    shutil.copytree(APPLE / 'Sources', package / 'Sources', ignore=shutil.ignore_patterns('park_ffi.swift'))
    swift = package / 'Sources/ParkCore/park_ffi.swift'
    shutil.copyfile(bindings / 'park_ffi.swift', swift)
    (package / 'Generated').mkdir()
    resources = package / 'Generated/ParkSources'
    source_resources(resources)
    version_file = package / 'Generated/BuildVersion.xcconfig'
    version_file.write_text(apple_identity(), encoding='utf8', newline='\n')
    shutil.copytree(APPLE / 'App', package / 'App')
    shutil.copytree(APPLE / 'Park.xcodeproj', package / 'Park.xcodeproj', ignore=shutil.ignore_patterns('xcuserdata'))
    shutil.copyfile(APPLE / 'Park.xcconfig', package / 'Park.xcconfig')
    output = package / 'Generated/park_ffiFFI.xcframework'
    run('xcodebuild', '-create-xcframework', *framework_inputs, '-output', output)
    # 最终应用目标实际链接相同的 Swift 包；任一平台失败时保留原产物。
    identity = dict(line.split(' = ', 1) for line in apple_identity().splitlines())
    for platform, sdk, product_directory in (('macOS', 'macosx', 'Debug'), ('iOS', 'iphoneos', 'Debug-iphoneos'),
                                             ('iOS Simulator', 'iphonesimulator', 'Debug-iphonesimulator')):
        derived = stage / ('derived-' + platform.replace(' ', '-'))
        run('xcodebuild', '-project', package / 'Park.xcodeproj', '-scheme', 'HentaiPark',
            '-destination', 'generic/platform=' + platform,
            '-configuration', 'Debug', '-derivedDataPath', derived,
            'CODE_SIGNING_ALLOWED=NO', 'CODE_SIGN_ENTITLEMENTS=', 'build', cwd=package)
        app = derived / 'Build/Products' / product_directory / 'HentaiPark.app'
        contents = app / 'Contents' if platform == 'macOS' else app
        bundled = (contents / 'Resources' if platform == 'macOS' else contents) / 'ParkSources'
        if not bundled.is_dir() or {p.name for p in bundled.iterdir()} != {p.name for p in resources.iterdir()}:
            raise ValueError('应用产物缺少内置站点资源')
        if any((bundled / source.name).read_bytes() != source.read_bytes() for source in resources.iterdir()):
            raise ValueError('应用产物与站点资源原件不一致')
        with (contents / 'Info.plist').open('rb') as info:
            metadata = plistlib.load(info)
        if (metadata['DTPlatformName'] != sdk or metadata['CFBundleIdentifier'] != identity['PRODUCT_BUNDLE_IDENTIFIER']
                or metadata['CFBundleShortVersionString'] != identity['MARKETING_VERSION']
                or metadata['CFBundleVersion'] != identity['CURRENT_PROJECT_VERSION']):
            raise ValueError('应用目标平台、安装身份或版本与构建配置不一致')
        executable = (contents / 'MacOS' if platform == 'macOS' else contents) / metadata['CFBundleExecutable']
        run('xcrun', 'vtool', '-show-build', executable)
    (APPLE / 'Generated').mkdir(exist_ok=True)
    previous = []
    installed = []
    try:
        for candidate in (output, swift, version_file, resources):
            destination = APPLE / candidate.relative_to(package)
            if destination.exists():
                backup = stage / ('previous-' + candidate.name)
                destination.rename(backup)
                previous.append((backup, destination))
            candidate.rename(destination)
            installed.append((destination, candidate))
    except OSError:
        for destination, candidate in reversed(installed):
            destination.rename(candidate)
        for backup, destination in reversed(previous):
            backup.rename(destination)
        raise


if __name__ == '__main__':
    if platform.system() != 'Darwin':
        raise SystemExit('Apple 目标需要安装 Xcode 的 macOS 主机')
    (ROOT / 'build').mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=ROOT / 'build') as temp:
        build(Path(temp))
