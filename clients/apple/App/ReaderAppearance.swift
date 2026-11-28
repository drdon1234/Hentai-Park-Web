import ParkCore
import SwiftUI
import ImageIO
import Photos
import UniformTypeIdentifiers
#if os(iOS)
import UIKit
#else
import AppKit
#endif

struct ReaderTaps: ViewModifier {
    let tap: (UnitPoint) -> Void
    let doubleTap: (UnitPoint) -> Void
    @State private var size: CGSize = .zero
    @State private var pending: Task<Void, Never>?
    @State private var lastLocation: CGPoint?
    @State private var started: Date?

    private func point(_ location: CGPoint) -> UnitPoint {
        UnitPoint(x: min(1, max(0, location.x / max(1, size.width))),
                  y: min(1, max(0, location.y / max(1, size.height))))
    }

    @ViewBuilder func body(content: Content) -> some View {
        let measured = content.background(GeometryReader { area in
            Color.clear.onAppear { size = area.size }.onChange(of: area.size) { size = $0 }
        })
        if #available(iOS 16, macOS 13, *) {
            measured.simultaneousGesture(
                SpatialTapGesture(count: 2).onEnded { doubleTap(point($0.location)) }
                    .exclusively(before: SpatialTapGesture().onEnded { tap(point($0.location)) })
            )
        } else {
            measured.simultaneousGesture(DragGesture(minimumDistance: 0).onChanged { event in
                if started == nil { started = event.time }
            }.onEnded { drag in
                defer { started = nil }
                guard let started, drag.time.timeIntervalSince(started) < 0.45,
                      hypot(drag.translation.width, drag.translation.height) < 8 else { return }
                let location = drag.location
                let anchor = point(location)
                if let previous = lastLocation, hypot(location.x - previous.x, location.y - previous.y) < 24 {
                    pending?.cancel()
                    lastLocation = nil
                    doubleTap(anchor)
                } else {
                    pending?.cancel()
                    lastLocation = location
                    pending = Task { @MainActor in
                        do { try await Task.sleep(nanoseconds: 280_000_000) } catch { return }
                        lastLocation = nil
                        tap(anchor)
                    }
                }
            }).onDisappear { pending?.cancel(); lastLocation = nil }
        }
    }
}

#if os(macOS)
@MainActor struct ReaderKeyboard: NSViewRepresentable {
    let handle: (UInt16) -> Bool
    func makeNSView(context: Context) -> Monitor { Monitor() }
    func updateNSView(_ view: Monitor, context: Context) { view.handle = handle }
    static func dismantleNSView(_ view: Monitor, coordinator: ()) { view.stop() }
    final class Monitor: NSView {
        var handle: ((UInt16) -> Bool)?
        private var token: Any?
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            stop()
            guard window != nil else { return }
            token = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
                guard let self, let window = self.window, window === event.window, window.isKeyWindow,
                      window.attachedSheet == nil, !(window.firstResponder is NSTextView),
                      event.modifierFlags.intersection([.command,.control,.option,.shift]).isEmpty else { return event }
                return self.handle?(event.keyCode) == true ? nil : event
            }
        }
        func stop() { if let token { NSEvent.removeMonitor(token); self.token = nil } }
    }
}
#endif

#if os(iOS)
@MainActor struct ReaderSceneOrientation: UIViewRepresentable {
    let preference: String
    let report: (Bool) -> Void

    final class Monitor: UIView {
        static let owners = NSMapTable<UIWindowScene, Monitor>.weakToWeakObjects()
        weak var scene: UIWindowScene?
        var mask: UIInterfaceOrientationMask?
        var preferred = "auto"
        var report: ((Bool) -> Void)?
        private var revision = 0

        override func didMoveToWindow() {
            super.didMoveToWindow()
            if scene !== window?.windowScene { release(); scene = window?.windowScene; apply() }
        }
        func apply() {
            guard let scene else { return }
            let next: UIInterfaceOrientationMask? = preferred == "landscape" ? .landscape : preferred == "portrait" ? .portrait : nil
            guard next != mask else { return }
            guard let next else { release(); return }
            mask = next
            Self.owners.setObject(self, forKey: scene)
            revision += 1
            let attempt = revision
            notify(false, attempt: attempt)
            if #available(iOS 16, *) {
                invalidate()
                scene.requestGeometryUpdate(.iOS(interfaceOrientations: next)) { [weak self] _ in
                    self?.notify(true, attempt: attempt)
                }
            } else { UIViewController.attemptRotationToDeviceOrientation() }
        }
        func release() {
            guard let scene, Self.owners.object(forKey: scene) === self else { mask = nil; return }
            Self.owners.removeObject(forKey: scene);mask = nil;revision += 1
            notify(false, attempt: revision)
            if #available(iOS 16, *) {
                invalidate()
                scene.requestGeometryUpdate(.iOS(interfaceOrientations: Self.systemMask(scene))) { _ in }
            } else { UIViewController.attemptRotationToDeviceOrientation() }
        }
        static func systemMask(_ scene: UIWindowScene?) -> UIInterfaceOrientationMask {
            scene?.traitCollection.userInterfaceIdiom == .pad ? .all : .allButUpsideDown
        }
        @available(iOS 16, *) private func invalidate() {
            var controller = scene?.keyWindow?.rootViewController
            while let current = controller {
                current.setNeedsUpdateOfSupportedInterfaceOrientations()
                controller = current.presentedViewController
            }
        }
        private func notify(_ denied: Bool, attempt: Int) {
            Task { @MainActor [weak self] in
                guard let self, revision == attempt else { return }
                report?(denied)
            }
        }
    }
    func makeUIView(context: Context) -> Monitor {
        let monitor = Monitor()
        monitor.isUserInteractionEnabled = false
        return monitor
    }
    func updateUIView(_ view: Monitor, context: Context) {
        view.report = report
        view.preferred = preference
        view.apply()
    }
    static func dismantleUIView(_ view: Monitor, coordinator: ()) {
        view.report = nil;view.release()
    }
}
#endif

@MainActor final class ReaderDisplayActivity: ObservableObject {
    #if os(iOS)
    private static var readers = Set<UUID>()
    private static var previous = false
    private let id = UUID()
    #else
    private var activity: NSObjectProtocol?
    #endif

    func set(_ enabled: Bool) {
        #if os(iOS)
        if enabled {
            guard !Self.readers.contains(id) else { return }
            if Self.readers.isEmpty { Self.previous = UIApplication.shared.isIdleTimerDisabled }
            Self.readers.insert(id)
            UIApplication.shared.isIdleTimerDisabled = true
        } else if Self.readers.remove(id) != nil && Self.readers.isEmpty {
            UIApplication.shared.isIdleTimerDisabled = Self.previous
        }
        #else
        if enabled && activity == nil {
            activity = ProcessInfo.processInfo.beginActivity(options: .idleDisplaySleepDisabled, reason: "Reading")
        } else if !enabled, let activity {
            ProcessInfo.processInfo.endActivity(activity)
            self.activity = nil
        }
        #endif
    }
}

struct ReaderArtwork: View {
    let image: CGImage
    let appearance: ComicAppearance?

    var body: some View {
        let original = Image(decorative: image, scale: 1).resizable().scaledToFit()
        Group {
            if appearance?.inverted == true { original.colorInvert() }
            else { original }
        }
        .saturation(appearance?.gray == true ? 0 : 1)
        .overlay(Color.orange.opacity(appearance?.warmth ?? 0).allowsHitTesting(false))
        .overlay(Color.black.opacity(appearance?.dim ?? 0).allowsHitTesting(false))
    }
}

@MainActor struct ReaderAppearanceForm: View {
    let original: ComicAppearance
    let session: ProgressSession
    let tr: (String) -> String
    let vertical: Bool
    @Environment(\.dismiss) private var dismiss
    @State private var draft: ComicAppearance
    @State private var saving = false
    @State private var failed = false

    init(original: ComicAppearance, vertical: Bool, session: ProgressSession, tr: @escaping (String) -> String) {
        self.original = original
        self.vertical = vertical
        self.session = session
        self.tr = tr
        self._draft = State(initialValue: original)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(tr("外观")).font(.headline)
            Form {
                Section(header: Text(tr("阅读模式"))) {
                    if vertical {
                        Slider(value: $draft.gap, in: 0...100, step: 1) { Text(tr("页间距")) }
                            .accessibilityIdentifier("reader-gap")
                        Text(tr("页间距") + " · " + String(Int(draft.gap))).font(.caption)
                    } else {
                        Picker(tr("双页显示"), selection: $draft.dual) {
                            Text(tr("关闭")).tag("off")
                            Text(tr("横屏时")).tag("landscape")
                            Text(tr("总是")).tag("always")
                        }
                        Toggle(tr("首页单独显示"), isOn: $draft.coverSingle).disabled(draft.dual == "off")
                        Toggle(tr("拆分宽页"), isOn: $draft.splitWide)
                        Toggle(tr("翻页动画"), isOn: $draft.animated)
                    }
                    Toggle(tr("裁剪白边"), isOn: $draft.crop)
                }
                Section(header: Text(tr("点击翻页"))) {
                    Toggle(tr("点击翻页"), isOn: $draft.tapEnabled)
                        .accessibilityIdentifier("reader-tap-enabled")
                    Picker(tr("点击区域"), selection: $draft.tapLayout) {
                        ForEach(Array(["自动（随阅读模式）", "左右分区", "L 形", "Kindle 式", "边缘", "已关闭"].enumerated()), id: \.offset) { index, key in
                            Text(tr(key)).tag(index)
                        }
                    }.accessibilityIdentifier("reader-tap-layout").disabled(!draft.tapEnabled)
                    Toggle(tr("左右反转点击区域"), isOn: $draft.invertH).disabled(!draft.tapEnabled)
                    Toggle(tr("上下反转点击区域"), isOn: $draft.invertV).disabled(!draft.tapEnabled)
                }
                Section(header: Text(tr("外观"))) {
                #if os(iOS)
                Picker(tr("屏幕方向"), selection: $draft.orientation) {
                    Text(tr("跟随系统")).tag("auto")
                    Text(tr("竖屏")).tag("portrait")
                    Text(tr("横屏")).tag("landscape")
                }
                #endif
                Picker(tr("背景颜色"), selection: $draft.background) {
                    Text(tr("黑")).tag("black")
                    Text(tr("灰")).tag("gray")
                    Text(tr("白")).tag("white")
                }.accessibilityIdentifier("reader-background")
                Toggle(tr("显示页码"), isOn: $draft.showPageNumber)
                Toggle(tr("保持屏幕常亮"), isOn: $draft.keepAwake)
                VStack(alignment: .leading) {
                    Text(tr("调暗"))
                    Slider(value: $draft.dim, in: 0...0.8, step: 0.05) { Text(tr("调暗")) }
                        .accessibilityIdentifier("reader-dim")
                }
                VStack(alignment: .leading) {
                    Text(tr("护眼（暖色）"))
                    Slider(value: $draft.warmth, in: 0...0.5, step: 0.05) { Text(tr("护眼（暖色）")) }
                        .accessibilityIdentifier("reader-warmth")
                }
                Toggle(tr("灰度"), isOn: $draft.gray)
                Toggle(tr("反色"), isOn: $draft.inverted)
                Button(tr("重置滤镜")) { draft.resetFilters() }
                }
            }.modifier(ReaderFormScrolling())
            if failed { Text(tr("保存失败，请重试")) }
            HStack {
                Button(tr("取消")) { dismiss() }.keyboardShortcut(.cancelAction)
                Spacer()
                Button(tr("保存")) {
                    saving = true
                    Task {
                        defer { saving = false }
                        do { try await session.saveComicAppearance(draft, from: original); dismiss() }
                        catch { failed = true }
                    }
                }.keyboardShortcut(.defaultAction)
            }
        }
        .padding().disabled(saving).interactiveDismissDisabled(saving)
        #if os(macOS)
        .frame(minWidth: 360, minHeight: 340)
        #endif
    }
}

private struct ReaderFormScrolling: ViewModifier {
    func body(content: Content) -> some View {
        #if os(macOS)
        ScrollView { content }
        #else
        content
        #endif
    }
}

private final class OriginalPageFile: @unchecked Sendable {
    let url: URL
    private let folder: URL

    init(asset: PageImage, index: Int32) throws {
        let original = URL(fileURLWithPath: try asset.path())
        guard let image = CGImageSourceCreateWithURL(original as CFURL, nil),
              let type = CGImageSourceGetType(image),
              let ext = UTType(type as String)?.preferredFilenameExtension else { throw CocoaError(.fileReadCorruptFile) }
        folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        url = folder.appendingPathComponent("page-\(index + 1)").appendingPathExtension(ext)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
        do {
            try withExtendedLifetime(asset) { try FileManager.default.copyItem(at: original, to: url) }
            try FileManager.default.setAttributes([.posixPermissions: 0o400], ofItemAtPath: url.path)
        }
        catch { try? FileManager.default.removeItem(at: folder); throw error }
    }

    deinit { try? FileManager.default.removeItem(at: folder) }
}

@MainActor struct OriginalPageActions: View {
    let session: ProgressSession
    let images: ChapterImages
    let index: Int32
    let tr: (String) -> String
    @Environment(\.dismiss) private var dismiss
    @State private var file: OriginalPageFile?
    @State private var retry = 0
    @State private var message: String?
    @State private var saving = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(tr("第 {value0} 页").replacingOccurrences(of: "{value0}", with: String(index + 1))).font(.headline)
            if let file {
                Button(tr("保存到相册")) { savePhoto(file) }
                SavedFileExportButton(url: file.url, filename: file.url.lastPathComponent, label: tr("另存为副本"), tr: tr)
                FileShareButton(url: file.url, label: tr("分享"))
            } else if message == nil { ProgressView() }
            if let message {
                Text(tr(message))
                if file == nil { Button(tr("重试")) { retry += 1 } }
            }
            Button(tr("完成")) { dismiss() }.keyboardShortcut(.cancelAction)
        }.padding()
        .disabled(saving).interactiveDismissDisabled(saving)
        .task(id: retry) {
            message = nil
            do {
                let asset = try await session.pageImage(images, index: index)
                let copy = try await Task.detached { try OriginalPageFile(asset: asset, index: index) }.value
                try Task.checkCancellation()
                file = copy
            } catch { if !Task.isCancelled { message = "加载失败" } }
        }
    }

    private func savePhoto(_ file: OriginalPageFile) {
        saving = true
        Task {
            defer { saving = false }
            let access = await PHPhotoLibrary.requestAuthorization(for: .addOnly)
            guard access == .authorized || access == .limited else { message = "需要相册权限才能保存图片"; return }
            do {
                try await PHPhotoLibrary.shared().performChanges {
                    PHAssetCreationRequest.forAsset().addResource(with: .photo, fileURL: file.url, options: nil)
                }
                message = "已保存到相册"
            } catch { message = "保存失败，请重试" }
        }
    }
}
