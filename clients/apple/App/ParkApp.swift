import SwiftUI
import ParkCore
import ImageIO
import LocalAuthentication
#if os(iOS)
import UIKit
#else
import AppKit
#endif

private struct PrivacyObscuredKey: EnvironmentKey { static let defaultValue = false }
private struct CoverBlurKey: EnvironmentKey { static let defaultValue = false }
#if os(iOS)
private struct WindowSceneIdentifierKey: EnvironmentKey { static let defaultValue: String? = nil }
private extension EnvironmentValues {
    var windowSceneIdentifier: String? {
        get { self[WindowSceneIdentifierKey.self] }
        set { self[WindowSceneIdentifierKey.self] = newValue }
    }
}
#endif
extension EnvironmentValues {
    var blurCovers: Bool {
        get { self[CoverBlurKey.self] }
        set { self[CoverBlurKey.self] = newValue }
    }
    var privacyObscured: Bool {
        get { self[PrivacyObscuredKey.self] }
        set { self[PrivacyObscuredKey.self] = newValue }
    }
}

@main
@MainActor
struct ParkApp: App {
    #if os(iOS)
    @UIApplicationDelegateAdaptor(ParkAppDelegate.self) private var delegate
    #else
    @NSApplicationDelegateAdaptor(DesktopActivation.self) private var desktop
    #endif
    @StateObject private var access = LibraryAccess()
    @StateObject private var downloads = DownloadQueue()
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            ProtectedWindow(access: access).environmentObject(downloads).environmentObject(downloads.recordings)
            #if os(macOS)
                .environmentObject(desktop)
            #endif
        }
            .onChange(of: phase) { downloads.transition($0) }
            #if os(macOS)
            .onChange(of: desktop.active) { access.transition($0 ? .active : .inactive) }
            .commands { LibraryCommands(access: access) }
            #else
            .onChange(of: phase) { access.transition($0) }
            #endif
    }
}

#if os(macOS)
@MainActor private final class DesktopActivation: NSObject, NSApplicationDelegate, ObservableObject {
    @Published private(set) var active = false

    func applicationDidBecomeActive(_ notification: Notification) { active = true }
    func applicationWillResignActive(_ notification: Notification) { active = false }
}

@MainActor private struct DesktopWindowActivity: NSViewRepresentable {
    @Binding var main: Bool

    func makeNSView(context: Context) -> Monitor {
        let monitor = Monitor()
        monitor.changed = { main = $0 }
        return monitor
    }

    func updateNSView(_ view: Monitor, context: Context) {}

    final class Monitor: NSView {
        var changed: ((Bool) -> Void)?

        override func hitTest(_ point: NSPoint) -> NSView? { nil }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            let center = NotificationCenter.default
            center.removeObserver(self)
            guard let window else { return }
            for name in [NSWindow.didBecomeMainNotification, NSWindow.didResignMainNotification] {
                center.addObserver(self, selector: #selector(updateActivity(_:)), name: name, object: window)
            }
            // 等 SwiftUI 安装好观察视图后同步初始状态，再以原生窗口通知更新。
            Task { @MainActor [weak self] in self?.updateActivity() }
        }

        @objc private func updateActivity(_ notification: Notification? = nil) {
            changed?(window?.isMainWindow == true)
        }
    }
}
#endif

@MainActor private final class LibraryAccess: ObservableObject {
    @Published private(set) var privacy: LibraryPrivacy?
    @Published private(set) var locked = true
    @Published private(set) var failed = false
    @Published private(set) var busy = false
    private var away: TimeInterval?
    private var authentication: LAContext?
    private var copy = try? CopyCatalog(locale: Locale.preferredLanguages.first ?? "en", pluginJson: nil)
    func tr(_ key: String) -> String { copy?.text(key: key, params: [:]) ?? key }
    var locale: String { copy?.locale() ?? "en" }

    func load(confirmAge: Bool = false) async {
        guard !busy, privacy == nil || confirmAge else { return }
        busy = true
        failed = false
        defer { busy = false }
        do {
            let session = try await ProgressSession.openApplicationLibrary()
            let settings: LibraryPrivacy
            do {
                if confirmAge { try await session.confirmAge() }
                settings = try await session.privacySettings()
            } catch { await session.close(); throw error }
            await session.close()
            try Task.checkCancellation()
            let language = settings.language.flatMap { $0.isEmpty || $0 == "system" ? nil : $0 }
            copy = try CopyCatalog(locale: language ?? Locale.preferredLanguages.first ?? "en", pluginJson: nil)
            locked = settings.appLock == true
            privacy = settings
            #if os(iOS)
            await SubscriptionScheduler.shared.schedule(hours: settings.subscriptionHours)
            #endif
        } catch { if !Task.isCancelled { failed = true } }
    }

    func transition(_ phase: ScenePhase) {
        guard privacy?.appLock == true else { return }
        if phase == .active {
            if let away, ProcessInfo.processInfo.systemUptime - away >= Double(max(0, privacy?.appLockAfter ?? 0)) {
                locked = true
            }
            away = nil
        } else {
            #if os(macOS)
            if phase == .inactive && authentication != nil { return }
            #else
            if phase != .background { return }
            #endif
            away = away ?? ProcessInfo.processInfo.systemUptime
            authentication?.invalidate()
            authentication = nil
        }
    }

    func setIncognito(_ enabled: Bool) async {
        guard privacy?.adultConfirmed == true, !locked, !busy else { return }
        busy = true
        failed = false
        defer { busy = false }
        do {
            let session = try await ProgressSession.openApplicationLibrary()
            do {
                try await session.setIncognito(enabled)
                privacy = try await session.privacySettings()
            } catch { await session.close(); throw error }
            await session.close()
        } catch { failed = true }
    }

    func updatePrivacy(_ name: String, enabled: Bool) async {
        guard !busy else { return }
        busy = true
        failed = false
        defer { busy = false }
        do {
            if name == "appLock", enabled, privacy?.appLock != true {
                guard try await authenticateOwner() else { failed = true; return }
            }
            let session = try await ProgressSession.openApplicationLibrary()
            do {
                try await session.setPrivacyPreference(name, enabled: enabled)
                privacy = try await session.privacySettings()
            } catch { await session.close(); throw error }
            await session.close()
        } catch { failed = true }
    }

    func updatePreference(_ name: String, json: String) async {
        guard !busy else { return }
        busy = true
        failed = false
        defer { busy = false }
        do {
            let session = try await ProgressSession.openApplicationLibrary()
            do {
                try await session.setPreference(name, json: json)
                let settings = try await session.privacySettings()
                if name == "language" {
                    let language = settings.language.flatMap { $0.isEmpty || $0 == "system" ? nil : $0 }
                    copy = try CopyCatalog(locale: language ?? Locale.preferredLanguages.first ?? "en", pluginJson: nil)
                }
                privacy = settings
            } catch { await session.close(); throw error }
            await session.close()
        } catch { failed = true }
    }

    func finishOnboarding(categories: Set<String>, secureScreen: Bool, appLock: Bool, blurCovers: Bool) async {
        guard !busy else { return }
        busy = true
        failed = false
        defer { busy = false }
        do {
            if appLock, privacy?.appLock != true {
                guard try await authenticateOwner() else { failed = true; return }
            }
            let session = try await ProgressSession.openApplicationLibrary()
            do {
                try await session.completeOnboarding(categories: categories.sorted(), secureScreen: secureScreen, appLock: appLock, blurCovers: blurCovers)
                privacy = try await session.privacySettings()
            } catch { await session.close(); throw error }
            await session.close()
        } catch { failed = true }
    }
    private func authenticateOwner() async throws -> Bool {
        let context = LAContext()
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: nil) else { return false }
        authentication = context
        defer { if authentication === context { authentication = nil } }
        let accepted = try await context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: tr("验证身份以解锁 HentaiPark"))
        return accepted && authentication === context && !Task.isCancelled
    }

    func unlock() async {
        guard locked, !busy else { return }
        failed = false
        busy = true
        defer { busy = false }
        do {
            if try await authenticateOwner() { locked = false; away = nil }
            else { failed = true }
        } catch { failed = true }
    }
}

@MainActor private struct ProtectedWindow: View {
    @ObservedObject var access: LibraryAccess
    @EnvironmentObject private var downloads: DownloadQueue
    @State private var windowID = UUID()
    @Environment(\.scenePhase) private var phase
    #if os(macOS)
    @EnvironmentObject private var desktop: DesktopActivation
    @State private var mainWindow = false
    private var inactive: Bool { !desktop.active || !mainWindow }
    private let captured = false
    #else
    private var inactive: Bool { phase != .active }
    @State private var captured = true
    @State private var sceneIdentifier: String?
    #endif
    var body: some View {
        Group {
            if let settings = access.privacy {
                if settings.adultConfirmed != true {
                    VStack {
                        Text(access.tr("这里的内容来自第三方站点，可能包含成人内容。继续前请确认：你已年满 18 周岁（或所在地区的法定成年年龄），并且在你所在的地区浏览此类内容是合法的。"))
                        Button(access.tr("我已成年，继续")) { Task { await access.load(confirmAge: true) } }.disabled(access.busy)
                        if access.failed { Text(access.tr("保存失败，请重试")) }
                    }.padding()
                } else if access.locked {
                    VStack {
                        Text(access.tr("HentaiPark 已锁定"))
                        Button(access.tr(access.failed ? "重试" : "解锁")) { Task { await access.unlock() } }.disabled(access.busy)
                    }
                } else if settings.onboarded != true {
                    AppleOnboardingPanel(access: access)
                } else {
                    let hide = (inactive && (settings.secureScreen == true || settings.appLock == true))
                        || (captured && settings.secureScreen == true)
                    LibraryWindow(access: access)
                        .environment(\.blurCovers, settings.blurCovers == true)
                        .environment(\.privacyObscured,hide)
                        .opacity(hide ? 0 : 1).accessibilityHidden(hide).allowsHitTesting(!hide)
                }
            } else if access.failed {
                Button(access.tr("重试")) { Task { await access.load() } }
            } else { ProgressView() }
        }
        #if os(macOS)
        .background(DesktopWindowActivity(main: $mainWindow))
        #else
        .environment(\.windowSceneIdentifier, sceneIdentifier)
        .background(ScreenCapture(captured: $captured, sceneIdentifier: $sceneIdentifier)
            .allowsHitTesting(false).accessibilityHidden(true))
        #endif
        .onAppear {
            if access.privacy?.adultConfirmed == true { downloads.attach(windowID) }
            Task { await access.load() }
        }
        .onChange(of: access.privacy?.adultConfirmed) { confirmed in
            if confirmed == true { downloads.attach(windowID) } else { downloads.detach(windowID) }
        }
        .onDisappear { downloads.detach(windowID) }
    }
}

#if os(macOS)
private struct LibraryNavigationKey: FocusedValueKey { typealias Value = (Int) -> Void }
private extension FocusedValues {
    var navigateLibrary: ((Int) -> Void)? {
        get { self[LibraryNavigationKey.self] }
        set { self[LibraryNavigationKey.self] = newValue }
    }
}

@MainActor private struct LibraryCommands: Commands {
    @ObservedObject var access: LibraryAccess
    @FocusedValue(\.navigateLibrary) private var navigate
    var body: some Commands {
        CommandMenu(access.tr("书库")) {
            ForEach(Array(librarySections.enumerated()), id: \.offset) { index, label in
                Button(access.tr(label)) { navigate?(index) }
                    .keyboardShortcut(KeyEquivalent(Character(String(index + 1))), modifiers: .command)
                    .disabled(navigate == nil || access.locked)
            }
        }
    }
}
#else
private struct ScreenCapture: UIViewRepresentable {
    @Binding var captured: Bool
    @Binding var sceneIdentifier: String?
    final class ScreenView: UIView {
        var changed: ((Bool, String?) -> Void)?
        func observeScene() {
            if #available(iOS 17.0, *) {
                registerForTraitChanges([UITraitSceneCaptureState.self]) { (view: ScreenView, _: UITraitCollection) in
                    view.refresh()
                }
            }
        }
        override func didMoveToWindow() {
            super.didMoveToWindow()
            NotificationCenter.default.removeObserver(self, name: UIScreen.capturedDidChangeNotification, object: nil)
            guard let screen = window?.screen else { return }
            if #unavailable(iOS 17.0) {
                NotificationCenter.default.addObserver(self, selector: #selector(refresh), name: UIScreen.capturedDidChangeNotification, object: screen)
            }
            refresh()
        }
        @objc private func refresh() {
            DispatchQueue.main.async { [weak self] in
                guard let self, let screen = self.window?.screen else { return }
                let scene = self.window?.windowScene?.session.persistentIdentifier
                if #available(iOS 17.0, *) { self.changed?(self.traitCollection.sceneCaptureState != .inactive, scene) }
                else { self.changed?(screen.isCaptured, scene) }
            }
        }
        deinit { NotificationCenter.default.removeObserver(self) }
    }
    func makeUIView(context: Context) -> ScreenView {
        let view = ScreenView()
        view.isUserInteractionEnabled = false
        view.accessibilityElementsHidden = true
        view.changed = { captured = $0; sceneIdentifier = $1 }
        view.observeScene()
        return view
    }
    func updateUIView(_ view: ScreenView, context: Context) { view.changed = { captured = $0; sceneIdentifier = $1 } }
}
#endif

private let librarySections = ["发现", "收藏", "下载", "我的"]

@MainActor
private final class LibraryModel: ObservableObject {
    @Published private(set) var works: [LibraryWork]?
    @Published private(set) var failed = false
    private(set) var session: ProgressSession?
    private var generation = UUID()

    func refresh() async {
        let current = generation
        failed = false
        do {
            let opened: ProgressSession
            if let session { opened = session }
            else {
                opened = try await ProgressSession.openApplicationLibrary()
                guard generation == current, !Task.isCancelled else { await opened.close(); return }
                session = opened
            }
            let rows = try await opened.libraryWorks()
            if generation == current && !Task.isCancelled { works = rows }
        } catch {
            if generation == current && !Task.isCancelled { failed = true }
        }
    }

    func close() {
        generation = UUID()
        let opened = session
        session = nil
        works = nil
        Task { await opened?.close() }
    }
}

@MainActor private struct LibraryWindow: View {
    @ObservedObject var access: LibraryAccess
    @EnvironmentObject private var downloads: DownloadQueue
    @StateObject private var model = LibraryModel()
    @StateObject private var shelf = ShelfOptions()
    @StateObject private var downloadSelection = DownloadSelection()
    @StateObject private var siteDisplayOrder = SiteDisplayOrder()
    @State private var group = 0
    @State private var retry = 0
    private enum Destination {
        case sites, manageSites, search, subscriptions, updates, rankings, recordings, settings
        case site(InstalledSite)
        case references(String, [InstalledSite])
        case work(LibraryWork), chapter(ChapterKey)
    }
    @State private var destination: Destination?
    @State private var expandedDownloads = Set<LibraryWorkID>()
    private let labels = librarySections
    private let symbols = ["safari", "heart", "arrow.down.circle", "person.crop.circle"]
    @Environment(\.sizeCategory) private var textSize
    @Environment(\.scenePhase) private var phase
    #if os(iOS)
    @ObservedObject private var notices = SubscriptionNotifications.shared
    @Environment(\.windowSceneIdentifier) private var sceneIdentifier
    @Environment(\.privacyObscured) private var obscured

    private func openNotification() {
        guard phase == .active, !obscured, let sceneIdentifier, notices.take(scene: sceneIdentifier) else { return }
        group = 1
        destination = .subscriptions
    }
    #endif

    var body: some View {
        GeometryReader { geometry in
            let wide = geometry.size.width >= (textSize.isAccessibilityCategory ? 1024 : 740)
            if wide {
                HStack(spacing: 0) {
                    if geometry.size.width >= (textSize.isAccessibilityCategory ? 1280 : 1000) {
                        List {
                            ForEach(labels.indices, id: \.self) { index in
                                Button { choose(index) } label: { rootLabel(index) }
                                    .accessibilityAddTraits(group == index ? [.isSelected] : [])
                            }
                        }.listStyle(.sidebar).frame(width: 180)
                        Divider()
                    }
                    VStack(alignment: .leading) {
                        if geometry.size.width < (textSize.isAccessibilityCategory ? 1280 : 1000) { sectionPicker }
                        section(group)
                    }.frame(width: min(360, geometry.size.width * 0.36))
                    Divider()
                    detail(showBack: true).frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            } else { compact }
        }
        .environmentObject(siteDisplayOrder)
        .task(id: retry) {
            await model.refresh()
            if let session = model.session { await shelf.refresh(session) }
        }
        .onChange(of: downloads.revision) { _ in retry += 1 }
        .onReceive(NotificationCenter.default.publisher(for: .parkFavoritesChanged)) { _ in retry += 1 }
        .onDisappear { model.close() }
        #if os(iOS)
        .onAppear { openNotification() }
        .onChange(of: notices.pending) { _ in openNotification() }
        .onChange(of: sceneIdentifier) { _ in openNotification() }
        .onChange(of: obscured) { _ in openNotification() }
        #endif
        .task(id: phase) {
            guard phase == .active else { return }
            #if os(iOS)
            openNotification()
            #endif
            guard let session = try? await ProgressSession.openApplicationLibrary() else { return }
            #if os(iOS)
            try? await notices.deliver(session)
            #endif
            if let library = InstalledSources.library,
               let check = try? await session.checkFavorites(library, category: nil, force: false),
               (check.chapters ?? 0) > 0 { downloads.didEnqueue() }
            await session.close()
        }
        #if os(macOS)
        .focusedSceneValue(\.navigateLibrary, choose)
        #endif
    }

    private func choose(_ index: Int) {
        if index != group { shelf.finishSelection(); downloadSelection.finish(); destination = nil; group = index }
    }

    private var selection: Binding<Int> { Binding(get: { group }, set: choose) }

    private func rootLabel(_ index: Int) -> some View {
        Label(access.tr(labels[index]), systemImage: symbols[index])
    }

    private var sectionPicker: some View {
        Picker(access.tr("书库"), selection: selection) {
            ForEach(labels.indices, id: \.self) { index in Text(access.tr(labels[index])).tag(index) }
        }.pickerStyle(.segmented).labelsHidden().padding()
    }

    @ViewBuilder private var compact: some View {
        #if os(iOS)
        NavigationView {
            ZStack {
                NavigationLink(isActive: Binding(
                    get: { destination != nil },
                    set: { presented in if !presented { leaveChapter() } }
                )) {
                    if case .chapter? = destination {
                        detail(showBack: false)
                    } else {
                        detail(showBack: true).navigationBarHidden(true)
                    }
                } label: { EmptyView() }
                .accessibilityHidden(true)
                TabView(selection: selection) {
                    ForEach(labels.indices, id: \.self) { index in
                        section(index).tabItem { rootLabel(index) }.tag(index)
                    }
                }
            }
            .navigationBarHidden(true)
        }.navigationViewStyle(.stack)
        #else
        if destination != nil { detail(showBack: true) }
        else { VStack(alignment: .leading) { sectionPicker; section(group) } }
        #endif
    }

    @ViewBuilder private func section(_ index: Int) -> some View {
        if index == 3 {
            List {
                Button(access.tr("应用设置")) { destination = .settings }
                Button(access.tr("站点管理")) { destination = .manageSites }
                Toggle(access.tr("无痕模式"), isOn: Binding(get: { access.privacy?.incognito == true }, set: { enabled in
                    Task { await access.setIncognito(enabled) }
                })).disabled(access.busy)
                if access.failed { Text(access.tr("保存失败，请重试")) }
            }
        } else if model.failed {
            Text(access.tr("加载失败"))
            Button(access.tr("重试")) { retry += 1 }
        } else if let rows = model.works {
            let filtered = index == 1 ? shelf.rows(rows) : index == 2 ? downloadSelection.rows(rows) : rows.filter { $0.historyMs != nil }
            let damaged = index == 2 && (downloadSelection.status == nil || downloadSelection.status == 4)
                ? rows.filter(\.invalidDownload) : []
            List {
                if index == 2 {
                    Button(access.tr("录制分卷")) { destination = .recordings }
                    if let session = model.session {
                        DownloadControls(selection: downloadSelection, works: filtered,
                                         untypedSources: Set(rows.filter { $0.downloaded && !$0.invalidDownload && $0.itemKind == nil }.map(\.source)),
                                         session: session, tr: access.tr)
                        ForEach(damaged, id: \.libraryID) { work in
                            InvalidDownloadRow(work: work, session: session, tr: access.tr)
                        }
                    }
                }
                if index == 2 && downloads.failed {
                    Text(access.tr("下载失败"))
                    Button(access.tr("重试")) { downloads.refresh(); retry += 1 }
                }
                if index == 0 {
                    Button(access.tr("搜索")) { destination = .search }
                    Button(access.tr("排行榜")) { destination = .rankings }
                    Button(access.tr("全部站点")) { destination = .sites }
                    Button(access.tr("订阅")) { destination = .subscriptions }
                }
                if index == 1 {
                    Button(access.tr("书库更新")) { destination = .updates }
                    Button(access.tr("订阅更新")) { destination = .subscriptions }
                    if let session = model.session {
                        ShelfControls(options: shelf, session: session, visible: filtered,
                            sources: Array(Set(rows.filter(\.favorite).map(\.source))).sorted(),
                            incognito: access.privacy?.incognito == true, tr: access.tr)
                    }
                }
                Section(header: Text(access.tr(index == 0 ? "继续看" : labels[index]))) {
                    if index == 2, let session=model.session {
                        GameQueueRows(session:session,media:downloadSelection.kind,status:downloadSelection.status,
                                      showEmpty:filtered.isEmpty && damaged.isEmpty,tr:access.tr)
                    } else if filtered.isEmpty && damaged.isEmpty { Text(access.tr(["没有继续阅读的作品","还没有收藏","没有下载任务"][index])) }
                    if index == 1, let session = model.session {
                        ShelfWorkGrid(options: shelf, works: filtered, session: session, tr: access.tr,
                            details: { destination = .work($0) }, open: open)
                    } else {
                        ForEach(filtered, id: \.libraryID) { work in
                            VStack(alignment: .leading, spacing: 8) {
                                if index == 2, downloadSelection.selecting {
                                    let selected = downloadSelection.selected.contains(work.libraryID)
                                    Button {
                                        if selected { downloadSelection.selected.remove(work.libraryID) }
                                        else { downloadSelection.selected.insert(work.libraryID) }
                                    } label: {
                                        Label(work.title, systemImage: selected ? "checkmark.circle.fill" : "circle")
                                            .frame(maxWidth: .infinity, alignment: .leading).contentShape(Rectangle())
                                    }
                                    .accessibilityLabel(Text(work.title))
                                    .accessibilityAddTraits(selected ? [.isSelected] : [])
                                } else if index == 0, let chapter = work.historyChapter,
                                   ["comic","novel","video","voice","bundle"].contains(work.media), work.contentDomain != "live" {
                                    Button(work.title) { open(work, chapter) }
                                } else if work.downloaded, !work.invalidDownload, !work.chapters.isEmpty,
                                          ["comic","video","novel","bundle"].contains(work.media) {
                                    let expanded = expandedDownloads.contains(work.libraryID)
                                    Button {
                                        if expanded { expandedDownloads.remove(work.libraryID) }
                                        else { expandedDownloads.insert(work.libraryID) }
                                    } label: {
                                        HStack {
                                            Text(work.title)
                                            Spacer()
                                            Image(systemName: expanded ? "chevron.down" : "chevron.forward")
                                                .accessibilityHidden(true)
                                        }.contentShape(Rectangle())
                                    }
                                    .buttonStyle(.plain)
                                    .accessibilityLabel(Text(work.title))
                                    .accessibilityHint(Text(access.tr(expanded ? "收起" : "展开")))
                                    if expanded {
                                        ForEach(work.chapters, id: \.id) { chapter in
                                            Button(chapter.title) { open(work, chapter.id) }
                                                .buttonStyle(.borderless)
                                        }
                                        if index == 2, work.media == "comic", let session = model.session {
                                            ComicExportActions(jobs: downloads.exports, work: work, session: session, tr: access.tr)
                                        }
                                    }
                                } else if index != 2 {
                                    Button(work.title) { destination = .work(work) }
                                } else { Text(work.title) }

                                if index == 2, !downloadSelection.selecting, work.source != "local", work.media != "bundle" {
                                    Button(access.tr("作品详情")) { destination = .work(work) }
                                        .buttonStyle(.borderless)
                                }
                                if index == 2, !downloadSelection.selecting, let state = work.downloadStatus, state != 3,
                                   !work.invalidDownload, work.pendingDeletion == nil, let session = model.session {
                                    let resume = state == 2 || state == 4
                                    HStack {
                                        Text(access.tr(state == 4 ? "下载失败" : state == 2 ? "已暂停" : state == 1 ? "下载中" : "等待下载"))
                                            .foregroundColor(.secondary)
                                        Spacer()
                                        Button(access.tr(resume ? (state == 4 ? "重试" : "继续") : "暂停")) {
                                            Task { await downloads.toggle(work, resume: resume, fallback: session) }
                                        }.buttonStyle(.borderless)
                                    }
                                }
                            }.accessibilityElement(children: .contain)
                        }
                    }
                }
                if index == 0, let session = model.session, let library = InstalledSources.library {
                    HomeSiteGroups(session: session, library: library, interests: access.privacy?.homeCategories ?? [],
                        tr: access.tr, references: { destination = .references($0, $1) }) { destination = .site($0) }
                }
            }
        } else { ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity) }
    }

    @ViewBuilder private func detail(showBack: Bool) -> some View {
        if case .updates? = destination, let session = model.session {
            FavoriteUpdatesPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true,
                                tr: access.tr, back: leaveChapter)
        } else if case .subscriptions? = destination, let session = model.session {
            SubscriptionPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true, tr: access.tr, back: leaveChapter)
        } else if case .rankings? = destination, let session = model.session {
            ChartPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true, tr: access.tr, back: leaveChapter)
        } else if case .search? = destination, let session = model.session {
            AggregateSearchPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true,
                                tr: access.tr, back: leaveChapter)
        } else if case .recordings? = destination, let session = model.session {
            BroadcastRecordings(session: session, tr: access.tr, back: leaveChapter)
        } else if case .settings? = destination {
            AppleSettingsPanel(access: access, session: model.session, back: leaveChapter)
        } else if case .references(let domain, let sources)? = destination, let session = model.session,
                  let library = InstalledSources.library {
            ReferenceDirectory(session: session, library: library, domain: domain, sites: sources, locale: access.locale,
                incognito: access.privacy?.incognito == true, tr: access.tr, back: leaveChapter)
        } else if case .sites? = destination, let session = model.session {
            SourceDirectory(session:session, locale:access.locale, incognito:access.privacy?.incognito==true, tr:access.tr, close:leaveChapter)
        } else if case .manageSites? = destination, let session = model.session, let library = InstalledSources.library {
            SiteOrderView(session: session, library: library, tr: access.tr, back: leaveChapter)
        } else if case .site(let site)? = destination, let session = model.session, let library = InstalledSources.library {
            SourceResults(session: session, library: library, site: site, locale: access.locale,
                incognito: access.privacy?.incognito == true, tr: access.tr, close: leaveChapter).id(site.id)
        } else if case .work(let onlineWork)? = destination, let session = model.session {
            StoredSiteDetails(session:session,source:onlineWork.source,workId:onlineWork.workId,title:onlineWork.title,locale:access.locale,incognito:access.privacy?.incognito==true,tr:access.tr,back:leaveChapter).id(onlineWork)
        } else if case .chapter(let key)? = destination, let session = model.session, let work = model.works?.first(where: {
            $0.source == key.source && $0.workId == key.workId
        }) {
            let offline = work.media != "voice" && !work.invalidDownload && work.chapters.contains { $0.id == key.chapterId } && work.downloaded
            let online = offline ? nil : InstalledSources.library
            if work.media == "bundle" {
                BundleReaderPage(session: session, work: work, chapter: key.chapterId, incognito: access.privacy?.incognito == true,
                    tr: access.tr, back: leaveChapter).id(key)
            } else if !offline && online==nil {
                Text(access.tr("站点加载失败，请重试"))
            } else if work.media == "novel" {
                NovelReader(session: session, key: key, work: work, incognito: access.privacy?.incognito == true,
                             locale: access.locale, showBack: showBack, tr: access.tr, back: leaveChapter, library:online).id(key)
            } else if work.media == "voice", let online {
                StoredVoiceReader(session: session, library: online, key: key,
                    title: work.chapters.first { $0.id == key.chapterId }?.title ?? work.title,
                    incognito: access.privacy?.incognito == true, showBack: showBack,
                    tr: access.tr, back: leaveChapter).id(key)
            } else if work.media == "video" {
                MovieReader(key: key, title: work.chapters.first(where: { $0.id == key.chapterId })?.title ?? work.title,
                             incognito: access.privacy?.incognito == true, showBack: showBack, tr: access.tr, back: leaveChapter, library:online).id(key)
            } else {
                PageReader(incognito:access.privacy?.incognito==true,tr:access.tr,session:session,key:key,work:work,showBack:showBack,back:leaveChapter,library:online).id(key)
            }
        } else { Text(access.tr("选择作品")).foregroundColor(.secondary) }
    }

    private func leaveChapter() {
        if destination != nil { destination = nil; retry += 1 }
    }

    private func open(_ work: LibraryWork, _ chapter: String) {
        destination = .chapter(ChapterKey(source: work.source, workId: work.workId, chapterId: chapter))
    }
}

@MainActor struct PageReader: View {
    let incognito:Bool
    let tr:(String)->String
    let session: ProgressSession
    let key: ChapterKey
    let work: LibraryWork
    let showBack: Bool
    let back: () -> Void
    var library:SourceLibrary? = nil
    var initialPage: Int? = nil
    @State private var images: ChapterImages?
    @State private var image: CGImage?
    @State private var current: Int32 = 0
    @State private var count: Int32 = 0
    @State private var requested: Int32?
    @State private var retry = 0
    @State private var busy = true
    @State private var failed = false

    var body: some View {
        VStack {
            HStack {
                if showBack { Button(tr("返回"), action: back) }
                Text(work.title).lineLimit(2)
                Spacer()
                if count > 0 { Text("\(current + 1) / \(count)").monospacedDigit() }
            }.padding()
            if failed {
                Text(tr("加载失败"))
                Button(tr("重试")) { retry += 1 }
                Spacer()
            } else if let image {
                Image(decorative: image, scale: 1).resizable().scaledToFit()
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else { ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity) }
            HStack {
                Button(tr("上一页")) { turn(current - 1) }.disabled(busy || current == 0)
                Spacer()
                Button(tr("下一页")) { turn(current + 1) }.disabled(busy || current + 1 >= count)
            }.padding()
        }.task(id: retry) { await load() }
    }

    private func turn(_ index: Int32) {
        busy = true
        requested = index
        retry += 1
    }

    @MainActor private func load() async {
        busy = true
        failed = false
        defer { if !Task.isCancelled { busy = false } }
        do {
            let chapter: ChapterImages
            if let images { chapter = images }
            else if let library {
                chapter=try await session.comicPages(library,key:key)
                images=chapter
            }
            else {
                let cache = try FileManager.default.url(for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
                chapter = try await session.chapterImages(key, cache: cache)
                images = chapter
            }
            let total = chapter.count()
            guard total > 0 else { throw CocoaError(.fileReadCorruptFile) }
            let index: Int32
            if let requested { index = requested }
            else if let initialPage, count == 0 { index = Int32(clamping: min(max(initialPage, 0), Int(total - 1))) }
            else {
                let previous = try await session.read(key)
                let history = work.historyChapter == key.chapterId ? work.historyPage : 0
                index = Int32(max(0, min(Int64(total - 1), previous?.pageIndex ?? history)))
            }
            let asset = try await session.pageImage(chapter, index: index)
            let pixels = try await decodedPage(asset,maxSize:4096)
            try Task.checkCancellation()
            if (requested != nil || (initialPage != nil && count == 0)) && !incognito {
                let turn = PageTurn(key: key, historyChapterId: key.chapterId,
                    chapterTitle: work.chapters.first(where: { $0.id == key.chapterId })?.title,
                    pageIndex: Int64(index), pageCount: Int64(total), updatedMs: Int64(Date().timeIntervalSince1970 * 1000))
                _ = try await session.turnPage(turn)
            }
            try Task.checkCancellation()
            image = pixels
            current = index
            count = total
            requested = nil
        } catch { if !Task.isCancelled { failed = true } }
    }
}

func decodedPage(_ asset:PageImage,maxSize:Int) async throws -> CGImage {
    try await Task.detached {
        try withExtendedLifetime(asset) {
            try thumbnail(at: URL(fileURLWithPath: try asset.path()), maxSize: maxSize)
        }
    }.value
}

func thumbnail(at url: URL, maxSize: Int) throws -> CGImage {
    let options: [CFString: Any] = [
        kCGImageSourceCreateThumbnailFromImageAlways: true,
        kCGImageSourceCreateThumbnailWithTransform: true,
        kCGImageSourceShouldCacheImmediately: true,
        kCGImageSourceThumbnailMaxPixelSize: maxSize
    ]
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
          let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
    else { throw CocoaError(.fileReadCorruptFile) }
    return image
}

struct AppleCategoryOption: Identifiable {
    let id: String
    let label: String
}

let appleCategoryOptions: [AppleCategoryOption] = [
    AppleCategoryOption(id: "feed", label: "吃瓜"), AppleCategoryOption(id: "doujin", label: "本子"),
    AppleCategoryOption(id: "anime", label: "里番"), AppleCategoryOption(id: "aiDrama", label: "AI短剧"),
    AppleCategoryOption(id: "jav", label: "JAV"), AppleCategoryOption(id: "adult", label: "成人视频"),
    AppleCategoryOption(id: "shortvideo", label: "短视频"), AppleCategoryOption(id: "recording", label: "录播"),
    AppleCategoryOption(id: "gallery", label: "三次元美图"), AppleCategoryOption(id: "illustration", label: "二次元美图"),
    AppleCategoryOption(id: "game", label: "游戏资源"), AppleCategoryOption(id: "onlineGame", label: "在线游戏"),
    AppleCategoryOption(id: "other", label: "创作社区"), AppleCategoryOption(id: "live", label: "直播"),
    AppleCategoryOption(id: "novel", label: "小说"), AppleCategoryOption(id: "tavern", label: "酒馆"),
    AppleCategoryOption(id: "asmr", label: "音声")
]

@MainActor private struct HomeSiteGroups: View {
    @EnvironmentObject private var displayOrder: SiteDisplayOrder
    let session: ProgressSession
    let library: SourceLibrary
    let interests: [String]
    let tr: (String) -> String
    let references: (String, [InstalledSite]) -> Void
    let open: (InstalledSite) -> Void
    @State private var sites: [InstalledSite] = []
    @State private var failed = false
    @State private var attempt = 0
    @State private var dimension = "all"
    @State private var media = "all"
    @State private var order: [String] = []
    @State private var saving = false

    private var categories: [AppleCategoryOption] {
        discoveryDomains(order: order, dimension: dimension, media: media).compactMap { id in
            appleCategoryOptions.first { $0.id == id }
        }
    }

    private func save(dimension: String, media: String) {
        guard !saving else { return }
        saving = true
        Task {
            defer { saving = false }
            do {
                try await session.setDiscoveryScope(dimension: dimension, media: media)
                self.dimension = dimension
                self.media = media
                failed = false
            } catch { failed = true }
        }
    }

    var body: some View {
        Section(header: Text(tr("首页兴趣"))) {
            Picker(tr("内容范围"), selection: Binding(get: { dimension }, set: { save(dimension: $0, media: media) })) {
                Text(tr("全部")).tag("all")
                Text(tr("二次元")).tag("twoD")
                Text(tr("三次元")).tag("threeD")
            }.pickerStyle(.menu).disabled(saving)
            Picker(tr("媒体类型"), selection: Binding(get: { media }, set: { save(dimension: dimension, media: $0) })) {
                Text(tr("全部")).tag("all")
                Text(tr("图片")).tag("image")
                Text(tr("视频")).tag("video")
                Text(tr("音频")).tag("voice")
                Text(tr("其他")).tag("other")
            }.pickerStyle(.menu).disabled(saving)
            if failed { Button(tr("站点加载失败，请重试")) { attempt += 1 } }
            ForEach(categories) { option in
                let rows = displayOrder.ordered(sites.filter { $0.isBrowsableSite && $0.category == option.id })
                let catalogs = displayOrder.ordered(sites.filter { $0.offersReference(in: option.id) })
                if (!rows.isEmpty || !catalogs.isEmpty) && (interests.isEmpty || interests.contains(option.id)) {
                    DisclosureGroup(tr(option.label)) {
                        if !catalogs.isEmpty {
                            Button(tr("搜索作品资料")) { references(option.id, catalogs) }
                        }
                        ForEach(rows) { site in Button(site.name ?? site.key) { open(site) } }
                    }
                }
            }
        }.onReceive(NotificationCenter.default.publisher(for: .parkSiteOrderChanged)) { _ in
            attempt += 1
        }.task(id: attempt) {
            do {
                let settings = try await session.privacySettings()
                let values = try await session.installedSites(library)
                try Task.checkCancellation()
                dimension = settings.homeDimension ?? "all"
                media = settings.homeMedia ?? "all"
                order = settings.domainOrder ?? []
                sites = values
                failed = false
            } catch { if !Task.isCancelled { failed = true } }
        }
    }
}

@MainActor private struct AppleStorageSettings: View {
    let session: ProgressSession
    let tr: (String) -> String
    @EnvironmentObject private var downloads: DownloadQueue
    @State private var confirming = false
    @State private var choosingSite = false
    @State private var sites: [ProgressSession.ManagedSite] = []
    @State private var chosen = ""
    @State private var busy = false
    @State private var feedback = ""

    private func perform(_ operation: @escaping () async throws -> String) {
        guard !busy else { return }
        busy = true
        feedback = ""
        Task {
            defer { busy = false }
            do { feedback = try await operation() }
            catch { feedback = tr("保存失败，请重试") }
        }
    }

    var body: some View {
        Section(header: Text(tr("存储空间"))) {
            ForEach([true, false], id: \.self) { home in
                Button(tr(home ? "主页数据缓存" : "媒体缓存")) {
                    perform {
                        try await session.clearCache(home: home)
                        return tr("缓存已清除")
                    }
                }
            }
            Button(tr("删除已读章节的下载")) { confirming = true }
                .alert(tr("删除已读章节的下载"), isPresented: $confirming) {
                    Button(tr("取消"), role: .cancel) {}
                    Button(tr("删除"), role: .destructive) {
                        perform {
                            let count = try await downloads.removeRead(fallback: session)
                            return tr("已删除 {value0} 个章节").replacingOccurrences(of: "{value0}", with: String(count))
                        }
                    }
                } message: { Text(tr("只删除标记为已读的漫画章节，本地导入不受影响")) }
            Button(tr("清除某个站点的 Cookie")) {
                perform {
                    guard let library = InstalledSources.library else { throw SiteReadError.malformed }
                    sites = try await session.managedSites(library)
                    chosen = ""
                    choosingSite = true
                    return ""
                }
            }
            .sheet(isPresented: $choosingSite) {
                VStack(alignment: .leading, spacing: 12) {
                    Text(tr("清除哪个站点的 Cookie？")).font(.headline)
                    Picker(tr("站点"), selection: $chosen) {
                        Text(verbatim: "—").tag("")
                        ForEach(sites) { site in Text(site.name).tag(site.id) }
                    }.pickerStyle(.menu)
                    HStack {
                        Button(tr("取消")) { choosingSite = false }
                        Button(tr("清除"), role: .destructive) {
                            let selected = sites.first { $0.id == chosen }
                            guard let selected, let library = InstalledSources.library else { return }
                            choosingSite = false
                            perform {
                                try await session.clearCookies(library, source: selected.id)
                                return tr("已清除 {value0} 的 Cookie").replacingOccurrences(of: "{value0}", with: selected.name)
                            }
                        }.disabled(chosen.isEmpty)
                    }
                }.padding()
            }
            if !feedback.isEmpty { Text(feedback) }
        }.disabled(busy)
    }
}

private struct AppleSettingsForm<Content: View>: View {
    @ViewBuilder let content: () -> Content

    var body: some View {
        #if os(macOS)
        ScrollView {
            Form(content: content).frame(maxWidth: .infinity, alignment: .leading).padding()
        }
        #else
        Form(content: content)
        #endif
    }
}

@MainActor private struct AppleSettingsPanel: View {
    @ObservedObject var access: LibraryAccess
    let session: ProgressSession?
    let back: () -> Void
    @State private var categories = Set(appleCategoryOptions.map { $0.id })
    @State private var editProxy = false
    @State private var dictionaries = false
    @State private var proxy = ""
    @State private var invalidProxy = false

    private func proxySummary(_ value: String) -> String? {
        guard let url = URLComponents(string: value.contains("://") ? value : "http://" + value),
              ["http","https"].contains(url.scheme?.lowercased() ?? ""), let host = url.host, !host.isEmpty,
              let port = url.port, (1...65535).contains(port),
              url.path.isEmpty || url.path == "/", url.query == nil, url.fragment == nil else { return nil }
        return host + ":" + String(port)
    }

    var body: some View {
        AppleSettingsForm {
            HStack {
                Button(access.tr("返回"), action: back)
                Text(access.tr("应用设置")).font(.headline)
            }
            Section(header: Text(access.tr("隐私与安全"))) {
                Toggle(access.tr("模糊封面"), isOn: Binding(
                    get: { access.privacy?.blurCovers == true },
                    set: { value in Task { await access.updatePrivacy("blurCovers", enabled: value) } }
                ))
                Toggle(access.tr("安全屏幕"), isOn: Binding(
                    get: { access.privacy?.secureScreen == true },
                    set: { value in Task { await access.updatePrivacy("secureScreen", enabled: value) } }
                ))
                Toggle(access.tr("应用锁"), isOn: Binding(
                    get: { access.privacy?.appLock == true },
                    set: { value in Task { await access.updatePrivacy("appLock", enabled: value) } }
                ))
                Toggle(access.tr("订阅通知"), isOn: Binding(
                    get: { access.privacy?.subscriptionNotifications == true },
                    set: { value in Task { await access.updatePrivacy("subscriptionNotifications", enabled: value) } }
                ))
                Toggle(access.tr("通知显示预览"), isOn: Binding(
                    get: { access.privacy?.subscriptionNotificationPreview == true },
                    set: { value in Task { await access.updatePrivacy("subscriptionNotificationPreview", enabled: value) } }
                ))
            }
            Section(header: Text(access.tr("网络"))) {
                Button(access.tr("HTTP 代理")) {
                    proxy = access.privacy?.proxy ?? ""
                    invalidProxy = false
                    editProxy = true
                }
                let stored = access.privacy?.proxy ?? ""
                Text(stored.isEmpty ? access.tr("未设置（使用系统网络 / VPN）") :
                    proxySummary(stored) ?? access.tr("代理地址格式无效，请填写主机:端口")).font(.caption)
            }
            if let session {
                AppleStorageSettings(session: session, tr: access.tr)
            }
            Section(header: Text(access.tr("语言"))) {
                Picker(access.tr("界面语言"), selection: Binding(
                    get: { access.privacy?.language.flatMap { $0 == "system" ? nil : $0 } ?? "" },
                    set: { value in
                        guard let encoded = try? JSONEncoder().encode(value) else { return }
                        Task { await access.updatePreference("language", json: String(decoding: encoded, as: UTF8.self)) }
                    }
                )) {
                    Text(access.tr("跟随系统")).tag("")
                    Text("简体中文").tag("zh-Hans")
                    Text("繁體中文").tag("zh-Hant")
                    Text("English").tag("en")
                    Text("日本語").tag("ja")
                    Text("한국어").tag("ko")
                    Text("Español").tag("es")
                    Text("Русский").tag("ru")
                    Text("العربية").tag("ar")
                }
            }
            if session != nil, InstalledSources.library != nil {
                Section(header: Text(access.tr("官方标签翻译包"))) {
                    Button(access.tr("翻译包管理")) { dictionaries = true }
                }
            }
            Section(header: Text(access.tr("首页兴趣"))) {
                ForEach(appleCategoryOptions) { option in
                    Toggle(access.tr(option.label), isOn: Binding(
                        get: { categories.contains(option.id) },
                        set: { enabled in
                            if enabled { categories.insert(option.id) }
                            else { categories.remove(option.id) }
                        }
                    ))
                }
                Button(access.tr("保存")) {
                    let selected: Set<String> = categories.count == appleCategoryOptions.count ? [] : categories
                    let data = (try? JSONEncoder().encode(selected.sorted())) ?? Data("[]".utf8)
                    let json = String(data: data, encoding: .utf8) ?? "[]"
                    Task { await access.updatePreference("homeCategories", json: json) }
                }.disabled(access.busy || categories.isEmpty)
            }
            if access.failed { Text(access.tr("保存失败，请重试")) }
        }
        .disabled(access.busy)
        .sheet(isPresented: $dictionaries) {
            if let session, let library = InstalledSources.library {
                AppleTagPacks(session: session, library: library, initialLocale: access.locale, tr: access.tr)
            }
        }
        .sheet(isPresented: $editProxy) {
            VStack(alignment: .leading, spacing: 12) {
                Text(access.tr("HTTP 代理")).font(.headline)
                TextField(access.tr("填写代理主机:端口，留空表示不使用"), text: $proxy)
                    .textFieldStyle(.roundedBorder).onChange(of: proxy) { _ in invalidProxy = false }
                if invalidProxy { Text(access.tr("代理地址格式无效，请填写主机:端口")) }
                if access.failed { Text(access.tr("保存失败，请重试")) }
                HStack {
                    Button(access.tr("取消")) { editProxy = false }
                    Button(access.tr("保存")) {
                        let value = proxy.trimmingCharacters(in: .whitespacesAndNewlines)
                        guard value.isEmpty || proxySummary(value) != nil else { invalidProxy = true; return }
                        Task {
                            guard let encoded = try? JSONEncoder().encode(value) else { return }
                            await access.updatePreference("proxy", json: String(decoding: encoded, as: UTF8.self))
                            if !access.failed { editProxy = false }
                        }
                    }
                }
            }.padding().disabled(access.busy).interactiveDismissDisabled(access.busy)
        }
        .onAppear {
            if let saved = access.privacy?.homeCategories, !saved.isEmpty { categories = Set(saved) }
        }
    }
}

@MainActor private struct AppleOnboardingPanel: View {
    @ObservedObject var access: LibraryAccess
    @State private var blurCovers = false
    @State private var secureScreen = false
    @State private var appLock = false
    @State private var categories = Set(appleCategoryOptions.map { $0.id })

    private var finish: some View {
        Button(access.tr("完成设置")) {
            let selected: Set<String> = categories.count == appleCategoryOptions.count ? [] : categories
            Task { await access.finishOnboarding(categories: selected, secureScreen: secureScreen, appLock: appLock, blurCovers: blurCovers) }
        }.disabled(access.busy || categories.isEmpty)
    }

    var body: some View {
        AppleSettingsForm {
            Text(access.tr("首次使用设置")).font(.title2)
            Text(access.tr("选择首页内容范围，并设置隐私保护选项。之后仍可在我的页面调整。"))
                .foregroundColor(.secondary)
            Section(header: Text(access.tr("隐私"))) {
                Toggle(access.tr("模糊封面"), isOn: $blurCovers)
                Toggle(access.tr("安全屏幕"), isOn: $secureScreen)
                Toggle(access.tr("应用锁"), isOn: $appLock)
            }
            Section(header: Text(access.tr("首页兴趣"))) {
                ForEach(appleCategoryOptions) { option in
                    Toggle(access.tr(option.label), isOn: Binding(
                        get: { categories.contains(option.id) },
                        set: { enabled in
                            if enabled { categories.insert(option.id) }
                            else { categories.remove(option.id) }
                        }
                    ))
                }
            }
            if access.failed { Text(access.tr("保存失败，请重试")) }
            #if !os(macOS)
            finish
            #endif
        }
        #if os(macOS)
        .safeAreaInset(edge: .bottom) {
            VStack(spacing: 0) {
                Divider()
                HStack { Spacer(); finish.keyboardShortcut(.defaultAction) }.padding()
            }.background(.regularMaterial)
        }
        #endif
        .disabled(access.busy)
        .onAppear {
            secureScreen = access.privacy?.secureScreen == true
            blurCovers = access.privacy?.blurCovers == true
            appLock = access.privacy?.appLock == true
            if let saved = access.privacy?.homeCategories, !saved.isEmpty { categories = Set(saved) }
        }
    }
}
