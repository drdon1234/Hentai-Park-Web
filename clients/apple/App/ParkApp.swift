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
                .preferredColorScheme(access.colorScheme)
                .accentColor(access.privacy?.seedColor.map { Color(parkRGB: $0) })
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
    @Published private var protection = LibraryLockState()
    var locked: Bool { protection.locked }
    @Published private(set) var failed = false
    @Published private(set) var busy = false
    private var authentication: LAContext?
    private var copy = try? CopyCatalog(locale: Locale.preferredLanguages.first ?? "en", pluginJson: nil)
    func tr(_ key: String) -> String { copy?.text(key: key, params: [:]) ?? key }
    var locale: String { copy?.locale() ?? "en" }
    var colorScheme: ColorScheme? {
        switch privacy?.themeMode { case 1: return .light; case 2: return .dark; default: return nil }
    }

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
            protection.load(enabled: settings.appLock == true)
            privacy = settings
            await SubscriptionScheduler.shared.schedule(hours: settings.subscriptionHours)
        } catch { if !Task.isCancelled { failed = true } }
    }

    func transition(_ phase: ScenePhase) {
        if phase == .active {
            guard privacy?.appLock == true else { return }
            protection.resume(at: ProcessInfo.processInfo.systemUptime, grace: privacy?.appLockAfter ?? 0)
        } else {
            #if os(macOS)
            if phase == .inactive && authentication != nil { return }
            #else
            if phase != .background { return }
            #endif
            if privacy?.appLock == true { protection.leave(at: ProcessInfo.processInfo.systemUptime) }
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
                guard try await authenticateOwner(reason: "验证身份以开启应用锁") else { failed = true; return }
            }
            let session = try await ProgressSession.openApplicationLibrary()
            do {
                try await session.setPrivacyPreference(name, enabled: enabled)
                privacy = try await session.privacySettings()
            } catch { await session.close(); throw error }
            await session.close()
        } catch let error as LAError where [.userCancel, .systemCancel, .appCancel].contains(error.code) {
            return
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

    func finishOnboarding(categories: Set<String>, secureScreen: Bool, blurCovers: Bool) async {
        guard !busy else { return }
        busy = true
        failed = false
        defer { busy = false }
        do {
            let session = try await ProgressSession.openApplicationLibrary()
            do {
                try await session.completeOnboarding(categories: categories.sorted(), secureScreen: secureScreen, blurCovers: blurCovers)
                privacy = try await session.privacySettings()
            } catch { await session.close(); throw error }
            await session.close()
        } catch { failed = true }
    }
    private func authenticateOwner(reason: String) async throws -> Bool {
        let context = LAContext()
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: nil) else { return false }
        authentication = context
        defer { if authentication === context { authentication = nil } }
        let accepted = try await context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: tr(reason))
        guard authentication === context, !Task.isCancelled else { throw LAError(.appCancel) }
        return accepted
    }

    func unlockWhenActive() async {
        guard !busy, locked, protection.takeAutomaticPrompt() else { return }
        await unlock()
    }

    func unlock() async {
        guard locked, !busy else { return }
        failed = false
        busy = true
        defer { busy = false }
        do {
            if try await authenticateOwner(reason: "验证身份以解锁 HentaiPark") { protection.authenticated() }
            else { failed = true }
        } catch let error as LAError where error.code == .userCancel || error.code == .systemCancel || error.code == .appCancel {
            return
        } catch { failed = true }
    }
}

@MainActor private struct ProtectedWindow: View {
    @ObservedObject var access: LibraryAccess
    @EnvironmentObject private var downloads: DownloadQueue
    @State private var windowID = UUID()
    @State private var group = 0
    @State private var destination: LibraryDestination?
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
    private func tryAutomaticUnlock() {
        guard phase == .active else { return }
        Task { await access.unlockWhenActive() }
    }

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
                    .onAppear { tryAutomaticUnlock() }
                    .onChange(of: phase) { _ in tryAutomaticUnlock() }
                    .onChange(of: access.busy) { _ in tryAutomaticUnlock() }
                } else if settings.onboarded != true {
                    let obscured = (inactive || captured) && (settings.secureScreen == true || settings.appLock == true)
                    AppleOnboardingPanel(access: access)
                        .opacity(obscured ? 0 : 1).accessibilityHidden(obscured).allowsHitTesting(!obscured)
                } else {
                    let hide = (inactive && (settings.secureScreen == true || settings.appLock == true))
                        || (captured && (settings.secureScreen == true || settings.appLock == true))
                    LibraryWindow(access: access, group: $group, destination: $destination)
                        .environment(\.blurCovers, settings.blurCovers == true)
                        .environment(\.autoTranslateContent, settings.autoTranslateContent == true)
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
    @Published private(set) var hiddenHistory = Set<LibraryWorkID>()
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
            let hidden = try await opened.hiddenHistoryIDs(rows)
            if generation == current && !Task.isCancelled { hiddenHistory = hidden; works = rows }
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

private enum LibraryDestination {
    case sites, manageSites, search, subscriptions, updates(Int64?), rankings, recordings, settings, categories
    case site(InstalledSite)
    case domainSearch(String)
    case references(String, [InstalledSite])
    case work(LibraryWork), chapter(ChapterKey)
}

@MainActor private struct LibraryWindow: View {
    @ObservedObject var access: LibraryAccess
    @EnvironmentObject private var downloads: DownloadQueue
    @StateObject private var model = LibraryModel()
    @StateObject private var shelf = ShelfOptions()
    @StateObject private var downloadSelection = DownloadSelection()
    @State private var homeRefresh = 0
    @State private var interactionSources = Set<String>()
    @StateObject private var siteDisplayOrder = SiteDisplayOrder()
    @Binding var group: Int
    @State private var retry = 0
    @Binding var destination: LibraryDestination?
    @State private var expandedDownloads = Set<LibraryWorkID>()
    @State private var historyScope: HistoryShelf.Scope = .all
    @State private var showingHistory = false
    @State private var showingComicExports = false
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
        .sheet(isPresented: $showingHistory) {
            if let session = model.session {
                HistoryShelf(session: session, locale: access.locale, incognito: access.privacy?.incognito == true, tr: access.tr, scope: historyScope)
            }
        }
        .sheet(isPresented: $showingComicExports) {
            if let session = model.session {
                ComicExportShelf(jobs: downloads.exports, session: session, tr: access.tr)
            }
        }
        .task(id: retry) {
            await model.refresh()
            if let session = model.session { await shelf.refresh(session) }
        }
        .onChange(of: downloads.revision) { _ in retry += 1 }
        .onReceive(NotificationCenter.default.publisher(for: .parkFavoritesChanged)) { _ in retry += 1 }
        .onReceive(NotificationCenter.default.publisher(for: .parkHistoryChanged)) { _ in retry += 1 }
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

    init(access: LibraryAccess, group: Binding<Int>, destination: Binding<LibraryDestination?>) {
        self.access = access
        self._group = group
        self._destination = destination
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
            let filtered = index == 1 ? shelf.rows(rows) : index == 2 ? downloadSelection.rows(rows) : rows.filter { $0.historyMs != nil && !model.hiddenHistory.contains($0.libraryID) }
            let damaged = index == 2 && (downloadSelection.status == nil || downloadSelection.status == 4)
                ? rows.filter(\.invalidDownload) : []
            List {
                if index == 2 {
                    Button(access.tr("录制分卷")) { destination = .recordings }
                    if let session = model.session {
                        DownloadControls(selection: downloadSelection, works: filtered, allWorks: rows,
                                         untypedSources: Set(rows.filter { $0.downloaded && !$0.invalidDownload && $0.itemKind == nil }.map(\.source)),
                                         session: session, tr: access.tr, showExports: { showingComicExports = true },
                                         manageCategories: { destination = .categories })
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
                    Button(access.tr("浏览历史")) { historyScope = .all; showingHistory = true }
                    Button(access.tr("搜索")) { destination = .search }
                    Button(access.tr("排行榜")) { destination = .rankings }
                    Button(access.tr("全部站点")) { destination = .sites }
                    Button(access.tr("订阅")) { destination = .subscriptions }
                }
                if index == 1 {
                    Button(access.tr("书库更新")) { destination = .updates(shelf.category) }
                    Button(access.tr("订阅更新")) { destination = .subscriptions }
                    if let session = model.session {
                        ShelfControls(options: shelf, session: session, visible: filtered,
                            sources: Array(Set(rows.filter(\.favorite).map(\.source))).sorted(),
                            incognito: access.privacy?.incognito == true, tr: access.tr, manageCategories: { destination = .categories })
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

                                if index == 2, !downloadSelection.selecting, work.downloaded, !work.invalidDownload,
                                   let session = model.session {
                                    DownloadRemovalMenu(work: work, session: session, tr: access.tr)
                                }
                                if index == 2, !downloadSelection.selecting, work.source != "local", work.media != "bundle" {
                                    Button(access.tr("作品详情")) { destination = .work(work) }
                                        .buttonStyle(.borderless)
                                }
                                if index == 2, !downloadSelection.selecting, work.downloaded, !work.invalidDownload,
                                   work.pendingDeletion == nil, let session = model.session {
                                    DownloadCategoryButton(work: work, session: session, tr: access.tr)
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
                                         .accessibilityHint(Text(work.title))
                                         .accessibilityIdentifier("download-toggle:" + work.source + ":" + work.workId)
                                    }
                                }
                            }.accessibilityElement(children: .contain)
                             .modifier(DownloadRowSelectionMenu(enabled: index == 2 && work.downloaded && !work.invalidDownload,
                                                                work: work, selection: downloadSelection, tr: access.tr))
                             .modifier(HistoryRowMenu(enabled: index == 0, work: work, session: model.session,
                                                      offersInteractions: interactionSources.contains(work.source),
                                                      tr: access.tr, detail: { destination = .work(work) }))
                        }
                    }
                }
                if index == 0, let session = model.session, let library = InstalledSources.library {
                    HomeSiteGroups(session: session, library: library, interests: access.privacy?.homeCategories ?? [],
                        locale: access.locale, incognito: access.privacy?.incognito == true, revision: homeRefresh, tr: access.tr,
                        history: { historyScope = .domain($0); showingHistory = true }, search: { destination = .domainSearch($0) },
                        references: { destination = .references($0, $1) },
                        capabilities: { interactionSources = Set($0.filter(\.interaction).map(\.key)) }) { destination = .site($0) }
                }
            }.modifier(HomeRefresh(enabled: index == 0, refresh: { homeRefresh += 1 }))
        } else { ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity) }
    }

    @ViewBuilder private func detail(showBack: Bool) -> some View {
        if case .categories? = destination, let session = model.session {
            ShelfCategoryManager(session: session, tr: access.tr, back: leaveChapter)
        } else if case .updates(let scope)? = destination, let session = model.session {
            FavoriteUpdatesPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true,
                                tr: access.tr, back: leaveChapter, category: scope).id(scope)
        } else if case .subscriptions? = destination, let session = model.session {
            SubscriptionPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true, tr: access.tr, back: leaveChapter)
        } else if case .rankings? = destination, let session = model.session {
            ChartPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true, tr: access.tr, back: leaveChapter)
        } else if case .domainSearch(let domain)? = destination, let session = model.session {
            AggregateSearchPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true,
                                tr: access.tr, back: leaveChapter, category: domain)
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
                PageReader(incognito:access.privacy?.incognito==true,tr:access.tr,session:session,key:key,work:work,showBack:showBack,back:leaveChapter,library:online,changeChapter: { destination = .chapter($0) }).id(key)
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
    var changeChapter: ((ChapterKey) -> Void)?
    @State private var images: ChapterImages?
    @State private var image: CGImage?
    @State private var current: Int32 = 0
    @State private var count: Int32 = 0
    @State private var sliderPage: Double?
    @State private var requested: Int32?
    @State private var retry = 0
    @State private var busy = true
    @State private var failed = false

    @State private var appearance: ComicAppearance?
    @State private var showAppearance = false
    @State private var controlsVisible = true
    @StateObject private var display = ReaderDisplayActivity()
    #if os(iOS)
    @State private var orientationDenied = false
    #endif
    @Environment(\.scenePhase) private var phase
    @Environment(\.privacyObscured) private var obscured
    @State private var readingSettings: ComicReadingSettings?
    @State private var savingMode = false
    @State private var modeError = false
    @State private var settingsRevision = 0
    @State private var scrollTarget: Int32?
    @State private var pageAspects = [Double]()
    @State private var landscape = false
    @State private var trailingHalf = false
    @State private var zoomAnchor: UnitPoint?
    private struct OriginalSelection: Identifiable { let id: Int32 }
    @State private var originalSelection: OriginalSelection?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var spreads: [ComicSpread] {
        let dual = appearance?.dual == "always" || (appearance?.dual == "landscape" && landscape)
        return ComicSpread.pages(aspects: pageAspects, dual: dual, coverSingle: appearance?.coverSingle != false,
                                 splitWide: appearance?.splitWide == true, rtl: readingSettings?.effective == .rightToLeft)
    }

    private func ordinal(in pages: [ComicSpread]) -> Int? {
        guard let first = pages.firstIndex(where: { $0.first == current || $0.second == current }) else { return nil }
        if trailingHalf, pages[first].half != nil, first + 1 < pages.count, pages[first + 1].first == current { return first + 1 }
        return first
    }

    private func changeMode(_ operation: @escaping () async throws -> Void) {
        guard !savingMode else { return }
        savingMode = true
        Task {
            defer { savingMode = false }
            do { try await operation(); modeError = false }
            catch { modeError = true }
        }
    }

    private var modeMenu: some View {
        Menu(tr("阅读设置")) {
            if appearance != nil { Button(tr("外观")) { showAppearance = true } }
            if let settings = readingSettings {
                Picker(tr("阅读模式（仅本作）"), selection: Binding(
                    get: { settings.effective },
                    set: { value in changeMode { try await session.setComicMode(key, mode: value) } }
                )) {
                    ForEach(ComicReadingMode.allCases, id: \.self) { mode in
                        Text(tr(mode.label)).tag(mode)
                    }
                }
                if settings.workMode != nil {
                    if settings.workMode != settings.defaultMode {
                        Button(tr("设为默认")) { changeMode { try await session.promoteComicMode(key) } }
                    }
                    Button(tr("跟随默认")) { changeMode { try await session.setComicMode(key, mode: nil) } }
                }
            } else {
                Button(tr("重试")) { settingsRevision += 1 }
            }
        }.disabled(savingMode)
    }

    var body: some View {
        VStack {
            if controlsVisible {
                HStack {
                    if showBack { Button(tr("返回"), action: back) }
                    Text(work.title).lineLimit(2)
                    Spacer()
                    modeMenu
                    if count > 0 { Text("\(current + 1) / \(count)").monospacedDigit() }
                }.padding()
            }
            if modeError {
                Button(tr("保存失败，请重试")) { modeError = false; settingsRevision += 1 }
            }
            if failed {
                Text(tr("加载失败"))
                Button(tr("重试")) { retry += 1 }
                Spacer()
            } else if readingSettings?.effective == .vertical, let images, count > 0 {
                ComicPageStrip(session: session, images: images, appearance: appearance, count: count, initial: current, jump: scrollTarget, tr: tr,
                               original: { originalSelection = OriginalSelection(id: $0) }) { index in
                    if current != index && requested != index { turn(index, scroll: false) }
                }.background(readerBackground)
                .modifier(ReaderTaps(tap: tapped, doubleTap: magnify))
                .scaleEffect(zoomAnchor == nil ? 1 : 2, anchor: zoomAnchor ?? .center).clipped()
            } else if let image {
                GeometryReader { area in
                    let pages = spreads
                    let spread = ordinal(in: pages).map { pages[$0] }
                    HStack(spacing: 0) {
                        if let spread, let other = spread.second, let images {
                            let pair = readingSettings?.effective == .rightToLeft ? [other, spread.first] : [spread.first, other]
                            ForEach(pair, id: \.self) { page in
                                Group {
                                    if page == current { ReaderArtwork(image: image, appearance: appearance) }
                                    else {
                                        ComicStripImage(session: session, chapter: images, index: page, tr: tr, appearance: appearance) { aspect in
                                            if pageAspects.indices.contains(Int(page)), pageAspects[Int(page)] == 0 {
                                                pageAspects[Int(page)] = aspect
                                            }
                                        }
                                    }
                                }.frame(width: area.size.width / 2, height: area.size.height)
                                .accessibilityIdentifier("reader-page-" + String(page + 1))
                                .contentShape(Rectangle()).contextMenu { originalButton(page) }
                            }
                        } else {
                            ReaderArtwork(image: halfImage(image, spread?.half), appearance: appearance)
                                .id(current)
                                .frame(width: area.size.width, height: area.size.height)
                                .contentShape(Rectangle()).contextMenu { originalButton(current) }
                        }
                    }.environment(\.layoutDirection, .leftToRight)
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier("reading-canvas")
                    .scaleEffect(zoomAnchor == nil ? 1 : 2, anchor: zoomAnchor ?? .center).clipped()
                    .background(readerBackground).contentShape(Rectangle())
                    .modifier(ReaderTaps(tap: tapped, doubleTap: magnify))
                    .simultaneousGesture(DragGesture(minimumDistance: 35).onEnded { drag in
                        guard !busy, zoomAnchor == nil, abs(drag.translation.width) > abs(drag.translation.height) else { return }
                        let next = (drag.translation.width < 0) != (readingSettings?.effective == .rightToLeft)
                        advance(next ? 1 : -1)
                    })
                }
                .accessibilityElement(children: .contain)
                .accessibilityIdentifier("reader-viewport")
            } else { ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity) }
            if controlsVisible {
                if count > 1 {
                    Slider(value: Binding(get: { sliderPage ?? Double(current) }, set: { sliderPage = $0 }),
                        in: 0...Double(count - 1), step: 1, onEditingChanged: { editing in
                            if !editing, let sliderPage {
                                self.sliderPage = nil
                                turn(Int32(sliderPage.rounded()))
                            }
                        }, label: { Text(tr("页码")) }).accessibilityIdentifier("reader-progress").padding(.horizontal)
                }
                HStack {
                    Button(tr("上一页")) { advance(-1) }.disabled(busy || !canAdvance(-1))
                        #if os(macOS)
                        .keyboardShortcut(readingSettings?.effective == .rightToLeft ? .rightArrow : .leftArrow, modifiers: [])
                        #endif
                    Spacer()
                    if changeChapter != nil && work.chapters.count > 1 {
                        Menu(tr("章节")) {
                            Button(tr("上一章")) { openAdjacent(-1) }.disabled(adjacent(-1) == nil)
                            Button(tr("下一章")) { openAdjacent(1) }.disabled(adjacent(1) == nil)
                        }.disabled(busy)
                        Spacer()
                    }
                    Button(tr("下一页")) { advance(1) }.disabled(busy || !canAdvance(1))
                        #if os(macOS)
                        .keyboardShortcut(readingSettings?.effective == .rightToLeft ? .leftArrow : .rightArrow, modifiers: [])
                        #endif
                }.environment(\.layoutDirection, readingSettings?.effective == .rightToLeft ? .rightToLeft : .leftToRight)
                 .padding()
            } else if appearance?.showPageNumber != false && count > 0 {
                Text("\(current + 1) / \(count)").font(.caption).monospacedDigit()
            }
        }
        #if os(iOS)
        .statusBar(hidden: !controlsVisible)
        .background {
            if !busy, !obscured, originalSelection == nil, !showAppearance, zoomAnchor == nil {
                let mode = readingSettings?.effective
                readerShortcut(.leftArrow, mode == .rightToLeft ? 1 : -1)
                readerShortcut(.rightArrow, mode == .rightToLeft ? -1 : 1)
                readerShortcut(.pageUp, -1)
                readerShortcut(.pageDown, 1)
                if mode == .vertical {
                    readerShortcut(.upArrow, -1)
                    readerShortcut(.downArrow, 1)
                }
            }
        }
        .background(ReaderSceneOrientation(preference: obscured ? "auto" : appearance?.orientation ?? "auto") { orientationDenied = $0 })
        .overlay(alignment: .top) {
            if controlsVisible && orientationDenied {
                Text(tr("当前窗口的方向由系统管理")).font(.caption).padding(6).background(.regularMaterial)
            }
        }
        #else
        .background(ReaderKeyboard { keyCode in
            guard !busy, !obscured, originalSelection == nil, !showAppearance, zoomAnchor == nil else { return false }
            let delta: Int32
            switch keyCode {
            case 123: delta = readingSettings?.effective == .rightToLeft ? 1 : -1
            case 124: delta = readingSettings?.effective == .rightToLeft ? -1 : 1
            case 116: delta = -1
            case 121: delta = 1
            case 125 where readingSettings?.effective == .vertical: delta = 1
            case 126 where readingSettings?.effective == .vertical: delta = -1
            default: return false
            }
            advance(delta)
            return true
        })
        #endif
        .background(GeometryReader { area in
            Color.clear.onAppear { landscape = area.size.width > area.size.height }
                .onChange(of: area.size) { landscape = $0.width > $0.height }
        })
        .task(id: retry) { await load() }
         .task(id: settingsRevision) {
            do {
                let settings = try await session.comicReadingSettings(key)
                let view = try await session.comicAppearance()
                try Task.checkCancellation()
                let cropChanged = (appearance?.crop ?? false) != view.crop
                appearance = view
                readingSettings = settings
                if cropChanged { retry += 1 }
                modeError = false
            } catch { if !Task.isCancelled { modeError = true } }
         }
         .onReceive(NotificationCenter.default.publisher(for: .parkReaderModeChanged)) { _ in settingsRevision += 1 }
         .sheet(isPresented: $showAppearance) {
            if let appearance { ReaderAppearanceForm(original: appearance, vertical: readingSettings?.effective == .vertical, session: session, tr: tr) }
         }
         .sheet(item: $originalSelection) { selection in
             if let images { OriginalPageActions(session: session, images: images, index: selection.id, tr: tr) }
         }
         .onAppear { display.set(holdingDisplay) }
         .onChange(of: holdingDisplay) { display.set($0) }
         .onDisappear { display.set(false) }
    }

    private var holdingDisplay: Bool { phase == .active && !obscured && appearance?.keepAwake == true }
    #if os(iOS)
    private func readerShortcut(_ key: KeyEquivalent, _ delta: Int32) -> some View {
        Button(tr(delta > 0 ? "下一页" : "上一页")) { advance(delta) }
            .keyboardShortcut(key, modifiers: [], localization: .fixed)
            .disabled(!canAdvance(delta))
            .frame(width: 0, height: 0).clipped().accessibilityHidden(true).allowsHitTesting(false)
    }
    #endif
    private func originalButton(_ index: Int32) -> some View {
        Button(tr("第 {value0} 页").replacingOccurrences(of: "{value0}", with: String(index + 1))) {
            originalSelection = OriginalSelection(id: index)
        }
    }
    private var readerBackground: Color {
        switch appearance?.background { case "white": return .white; case "gray": return .gray; default: return .black }
    }

    private func adjacent(_ offset: Int) -> OfflineChapter? {
        guard changeChapter != nil, let index = work.chapters.firstIndex(where: { $0.id == key.chapterId }),
              work.chapters.indices.contains(index + offset) else { return nil }
        return work.chapters[index + offset]
    }

    private func openAdjacent(_ offset: Int) {
        guard !busy, let next = adjacent(offset) else { return }
        changeChapter?(ChapterKey(source: key.source, workId: key.workId, chapterId: next.id))
    }

    private func canAdvance(_ delta: Int32) -> Bool {
        if readingSettings?.effective != .vertical {
            let pages = spreads
            if let i = ordinal(in: pages) { return pages.indices.contains(i + Int(delta)) || adjacent(Int(delta)) != nil }
        }
        return (0..<count).contains(current + delta) || adjacent(Int(delta)) != nil
    }

    private func tapped(_ point: UnitPoint) {
        guard zoomAnchor == nil else { return }
        switch appearance?.tap(x: Double(point.x), y: Double(point.y), mode: readingSettings?.effective ?? .leftToRight) {
        case .next: if !busy { advance(1) }
        case .previous: if !busy { advance(-1) }
        default: controlsVisible.toggle()
        }
    }

    private func magnify(_ point: UnitPoint) { zoomAnchor = zoomAnchor == nil ? point : nil }

    private func halfImage(_ image: CGImage, _ half: ComicSpread.Half?) -> CGImage {
        guard let half, image.width >= 2 else { return image }
        let mid = image.width / 2
        let x = half == .left ? 0 : mid
        return image.cropping(to: CGRect(x: x, y: 0, width: half == .left ? mid : image.width - mid, height: image.height)) ?? image
    }

    private func advance(_ delta: Int32) {
        if readingSettings?.effective != .vertical {
            let pages = spreads
            if let i = ordinal(in: pages) {
                let target = i + Int(delta)
                guard pages.indices.contains(target) else { openAdjacent(Int(delta)); return }
                let next = pages[target]
                let lastHalf = next.half != nil && target > 0 && pages[target - 1].first == next.first
                if next.first == current { trailingHalf = lastHalf }
                else { turn(next.first, trailing: lastHalf) }
                return
            }
        }
        let next = current + delta
        if (0..<count).contains(next) { turn(next) }
        else { openAdjacent(Int(delta)) }
    }

    private func turn(_ index: Int32, scroll: Bool = true, trailing: Bool = false) {
        guard (0..<count).contains(index) else { return }
        trailingHalf = trailing
        zoomAnchor = nil
        if scroll { scrollTarget = index } else { scrollTarget = nil }
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
            let asset = try await session.pageImage(chapter, index: index, crop: appearance?.crop == true)
            let pixels = try await decodedPage(asset,maxSize:4096)
            try Task.checkCancellation()
            if (requested != nil || count == 0 && (initialPage != nil || work.historyChapter != key.chapterId)) && !incognito {
                let turn = PageTurn(key: key, historyChapterId: key.chapterId,
                    chapterTitle: work.chapters.first(where: { $0.id == key.chapterId })?.title,
                    pageIndex: Int64(index), pageCount: Int64(total), updatedMs: Int64(Date().timeIntervalSince1970 * 1000))
                _ = try await session.turnPage(turn)
            }
            try Task.checkCancellation()
            if pageAspects.count != total { pageAspects = chapter.aspects() }
            if pageAspects.indices.contains(Int(index)), pageAspects[Int(index)] == 0 {
                pageAspects[Int(index)] = Double(pixels.width) / Double(pixels.height)
            }
            withAnimation(!reduceMotion && appearance?.animated != false && requested != nil && readingSettings?.effective != .vertical ? .easeOut(duration: 0.18) : nil) {
                image = pixels
                current = index
            }
            count = total
            requested = nil
        } catch { if !Task.isCancelled { failed = true } }
    }
}

private struct VisibleComicRows: PreferenceKey {
    static var defaultValue: [Int32: CGFloat] { [:] }
    static func reduce(value: inout [Int32: CGFloat], nextValue: () -> [Int32: CGFloat]) {
        value.merge(nextValue(), uniquingKeysWith: { _, new in new })
    }
}

@MainActor private struct ComicPageStrip: View {
    let session: ProgressSession
    let images: ChapterImages
    let appearance: ComicAppearance?
    let count: Int32
    let initial: Int32
    let jump: Int32?
    let tr: (String) -> String
    let original: (Int32) -> Void
    let visible: (Int32) -> Void
    @Namespace private var viewport
    @State private var aligned = false

    var body: some View {
        GeometryReader { area in
            ScrollViewReader { reader in
                ScrollView {
                    LazyVStack(spacing: CGFloat(appearance?.gap ?? 0)) {
                        ForEach(0..<Int(count), id: \.self) { n in
                            ComicStripImage(session: session, chapter: images, index: Int32(n), tr: tr, appearance: appearance)
                                .frame(width: area.size.width, height: area.size.height)
                                .accessibilityIdentifier("reader-page-" + String(n + 1))
                                .contentShape(Rectangle()).contextMenu {
                                    Button(tr("第 {value0} 页").replacingOccurrences(of: "{value0}", with: String(n + 1))) { original(Int32(n)) }
                                }
                                .background(GeometryReader { row in
                                    Color.clear.preference(key: VisibleComicRows.self,
                                        value: [Int32(n): row.frame(in: .named(viewport)).midY])
                                })
                        }
                    }
                }.coordinateSpace(name: viewport)
                 .accessibilityIdentifier("reader-scroll-viewport")
                 .onAppear { reader.scrollTo(Int(initial), anchor: .center) }
                 .onChange(of: jump) { value in
                     if let value { reader.scrollTo(Int(value), anchor: .center) }
                 }
                 .onPreferenceChange(VisibleComicRows.self) { rows in
                     guard let nearest = rows.min(by: {
                         abs($0.value - area.size.height / 2) < abs($1.value - area.size.height / 2)
                     }) else { return }
                     if !aligned {
                         guard nearest.key == initial else { return }
                         aligned = true
                     }
                     visible(nearest.key)
                 }
            }
        }
    }
}

@MainActor private struct ComicStripImage: View {
    let session: ProgressSession
    let chapter: ChapterImages
    let index: Int32
    let tr: (String) -> String
    let appearance: ComicAppearance?
    var measured: ((Double) -> Void)? = nil
    @State private var pixels: CGImage?
    @State private var failed = false
    @State private var attempt = 0
    var body: some View {
        Group {
            if let pixels { ReaderArtwork(image: pixels, appearance: appearance) }
            else if failed { Button(tr("重试")) { attempt += 1 } }
            else { ProgressView() }
        }.task(id: "\(attempt)-\(appearance?.crop == true)") {
            failed = false
            do {
                let asset = try await session.pageImage(chapter, index: index, crop: appearance?.crop == true)
                let image = try await decodedPage(asset, maxSize: 4096)
                try Task.checkCancellation()
                measured?(Double(image.width) / Double(image.height))
                pixels = image
            } catch { if !Task.isCancelled { failed = true } }
        }.onDisappear { pixels = nil }
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

private struct HomeRefresh: ViewModifier {
    let enabled: Bool
    let refresh: @MainActor () -> Void
    @ViewBuilder func body(content: Content) -> some View {
        if enabled { content.refreshable { await refresh() } }
        else { content }
    }
}

@MainActor private struct HomeSiteGroups: View {
    @EnvironmentObject private var displayOrder: SiteDisplayOrder
    let session: ProgressSession
    let library: SourceLibrary
    let interests: [String]
    let locale: String
    let incognito: Bool
    let revision: Int
    let tr: (String) -> String
    let history: (String) -> Void
    let search: (String) -> Void
    let references: (String, [InstalledSite]) -> Void
    let capabilities: ([InstalledSite]) -> Void
    let open: (InstalledSite) -> Void
    @State private var sites: [InstalledSite] = []
    @State private var failed = false
    @State private var attempt = 0
    @State private var dimension = "all"
    @State private var media = "all"
    @State private var order: [String] = []
    @State private var saving = false
    @State private var expanded = Set<String>()
    @State private var hidden = Set<String>()
    @State private var dataSaver = false
    @State private var feedRevision = 0
    @State private var optionsSite: InstalledSite?
    @StateObject private var translation = TranslationPage()

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
            HStack {
                TranslationControls(page: translation, tr: tr)
                Spacer()
                Button(tr("刷新")) { if !dataSaver { feedRevision += 1 }; attempt += 1 }
            }.buttonStyle(.borderless)
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
                    DisclosureGroup(isExpanded: Binding(get: { expanded.contains(option.id) }, set: { yes in
                        update {
                            expanded = Set(try await session.setHomeDomain(option.id, expanded: yes))
                        }
                    })) {
                        Button(tr("搜索")) { search(option.id) }
                            .accessibilityIdentifier("domain-search:" + option.id)
                        Button(tr("浏览历史")) { history(option.id) }
                            .accessibilityIdentifier("domain-history:" + option.id)
                        if !catalogs.isEmpty {
                            Button(tr("搜索作品资料")) { references(option.id, catalogs) }
                                .accessibilityIdentifier("domain-references:" + option.id)
                        }
                        if let first = rows.first, rows.count > 1 {
                            Menu(tr("选择展示站点")) {
                                ForEach(rows) { site in
                                    Button(site.name ?? site.key) {
                                        guard site.key != first.key else { return }
                                        update {
                                            try await session.moveSite(library, key: site.key, anchor: first.key, after: false)
                                            displayOrder.clear()
                                            NotificationCenter.default.post(name: .parkSiteOrderChanged, object: nil)
                                        }
                                    }
                                }
                            }
                        }
                        ForEach(rows) { site in
                            VStack(alignment: .leading) {
                                Button(site.name ?? site.key) { open(site) }.contextMenu {
                                    Button(tr("站点设置")) { optionsSite = site }
                                    visibilityButton(site)
                                }
                                if !dataSaver && !hidden.contains(site.key) && site.entryFeed != nil {
                                    HomeFeedPreview(session: session, library: library, site: site, locale: locale,
                                                    incognito: incognito, tr: tr, translation: translation, revision: feedRevision)
                                }
                            }
                        }
                    } label: { Text(tr(option.label)) }
                }
            }
        }.onChange(of: revision) { _ in
            if !dataSaver { feedRevision += 1 }
            attempt += 1
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
                expanded = Set(settings.homeExpandedDomains ?? [])
                hidden = Set(settings.homeHiddenSources ?? [])
                dataSaver = (settings.dataSavingMode ?? "off") != "off"
                sites = values
                capabilities(values)
                failed = false
            } catch { if !Task.isCancelled { failed = true } }
        }.sheet(item: $optionsSite) { site in
            SiteSettingsPage(session: session, library: library, site: site, locale: locale, tr: tr,
                             changed: { attempt += 1 }, signedIn: { optionsSite = nil })
        }
    }

    private func visibilityButton(_ site: InstalledSite) -> some View {
        Toggle(tr("显示该站点的作品并自动加载"), isOn: Binding(get: { !hidden.contains(site.key) }, set: { visible in
            update { hidden = Set(try await session.setHomeSite(site.key, visible: visible)) }
        }))
    }

    private func update(_ change: @escaping () async throws -> Void) {
        guard !saving else { return }
        saving = true
        Task {
            defer { saving = false }
            do { try await change(); failed = false } catch { failed = true }
        }
    }
}

@MainActor private struct HomeFeedPreview: View {
    let session: ProgressSession
    let library: SourceLibrary
    let site: InstalledSite
    let locale: String
    let incognito: Bool
    let tr: (String) -> String
    @ObservedObject var translation: TranslationPage
    let revision: Int
    @StateObject private var prompt = SitePrompt()
    @State private var works: [SiteWork] = []
    @State private var selected: SiteWork?
    @State private var failed = false
    @State private var retry = 0
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(works) { work in
                Button { selected = work } label: {
                    RemoteText(original: work.title, dictionary: nil, session: session, library: library, locale: locale, page: translation)
                }.buttonStyle(.borderless).disabled(!site.detail)
            }
            if failed { Text(tr("加载失败")); Button(tr("重试")) { retry += 1 } }
        }.task(id: "\(revision)/\(retry)") {
            failed = false
            do {
                let page = try await prompt.perform { ticket in
                    try await session.sitePage(library, site: site, keyword: "", page: 1, cursor: nil, confirmation: ticket)
                }
                try Task.checkCancellation()
                works = Array(page.items.prefix(6))
            } catch { if !Task.isCancelled { failed = true } }
        }.siteConfirmation(prompt, tr: tr)
        .sheet(item: $selected) { work in
            SiteDetails(session: session, library: library, site: site, workId: work.id, preview: work, locale: locale,
                        incognito: incognito, tr: tr, close: { selected = nil })
            #if os(macOS)
            .frame(minWidth: 440, minHeight: 360)
            #endif
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

private extension Color {
    init(parkRGB value: UInt32) {
        self.init(.sRGB, red: Double((value >> 16) & 255) / 255,
                  green: Double((value >> 8) & 255) / 255, blue: Double(value & 255) / 255)
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
    private let allCategories = Set(appleCategoryOptions.map { $0.id })
    private var categories: Set<String> {
        guard let saved = access.privacy?.homeCategories, !saved.isEmpty else { return allCategories }
        return Set(saved)
    }
    @State private var editProxy = false
    @State private var dictionaries = false
    @State private var subscriptionSettings = false
    @State private var proxy = ""
    @State private var invalidProxy = false
    #if os(macOS)
    @State private var displaySettingsFailed = false
    #endif

    private let accents: [(UInt32,String)] = [
        (0xFFC96442,"陶土"),(0xFFB8862F,"琥珀"),(0xFF5F8A62,"青苔"),(0xFF4F6FB8,"靛蓝"),(0xFF8A5C9E,"梅紫"),(0xFFC25878,"樱粉")
    ]


    var body: some View {
        AppleSettingsForm {
            HStack {
                Button(access.tr("返回"), action: back)
                Text(access.tr("应用设置")).font(.headline)
            }
            Section(header: Text(access.tr("外观"))) {
                Picker(access.tr("主题"), selection: Binding(get: { access.privacy?.themeMode ?? 0 }, set: { mode in
                    Task { await access.updatePreference("themeMode", json: String(mode)) }
                })) {
                    Text(access.tr("跟随系统")).tag(0); Text(access.tr("浅色")).tag(1); Text(access.tr("深色")).tag(2)
                }.pickerStyle(.menu).accessibilityIdentifier("settings-theme")
                Picker(access.tr("强调色"), selection: Binding(get: { access.privacy?.seedColor }, set: { selected in
                    if let selected { Task { await access.updatePreference("seedColor", json: String(selected)) } }
                })) {
                    if access.privacy?.seedColor == nil { Text(access.tr("跟随系统")).tag(UInt32?.none) }
                    if let saved = access.privacy?.seedColor, !accents.contains(where: { $0.0 == saved }) {
                        Text(access.tr("当前")).tag(Optional(saved))
                    }
                    ForEach(accents, id: \.0) { color, name in
                        Label { Text(access.tr(name)) } icon: { Circle().fill(Color(parkRGB: color)).frame(width: 12,height: 12) }
                            .tag(Optional(color))
                    }
                }.pickerStyle(.menu).accessibilityIdentifier("settings-accent")
            }
            #if os(macOS)
            Section {
                Button(access.tr("屏幕刷新率")) {
                    guard let panel = FileManager.default.urls(for: .preferencePanesDirectory, in: .systemDomainMask).first else {
                        displaySettingsFailed = true
                        return
                    }
                    displaySettingsFailed = !NSWorkspace.shared.open(panel.appendingPathComponent("Displays.prefPane"))
                }
                if displaySettingsFailed { Text(access.tr("无法打开链接")) }
            }
            #endif
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
                if access.privacy?.appLock == true {
                    Picker(access.tr("离开多久后重新锁定"), selection: Binding(
                        get: { access.privacy?.appLockAfter ?? 0 },
                        set: { seconds in Task { await access.updatePreference("appLockAfter", json: String(seconds)) } }
                    )) {
                        Text(access.tr("立即")).tag(0)
                        Text(access.tr("1 分钟")).tag(60)
                        Text(access.tr("5 分钟")).tag(300)
                        Text(access.tr("15 分钟")).tag(900)
                    }.pickerStyle(.menu)
                }
                Toggle(access.tr("订阅通知"), isOn: Binding(
                    get: { access.privacy?.subscriptionNotifications == true },
                    set: { value in Task { await access.updatePrivacy("subscriptionNotifications", enabled: value) } }
                ))
                Toggle(access.tr("通知显示预览"), isOn: Binding(
                    get: { access.privacy?.subscriptionNotificationPreview == true },
                    set: { value in Task { await access.updatePrivacy("subscriptionNotificationPreview", enabled: value) } }
                ))
            }
            if session != nil {
                Section(header: Text(access.tr("订阅与通知"))) {
                    Button(access.tr("订阅与通知设置")) { subscriptionSettings = true }
                }
            }
            Section(header: Text(access.tr("网络"))) {
                Button(access.tr("HTTP 代理")) {
                    proxy = access.privacy?.proxy ?? ""
                    invalidProxy = false
                    editProxy = true
                }
                let stored = access.privacy?.proxy ?? ""
                Text(stored.isEmpty ? access.tr("未设置（使用系统网络 / VPN）") :
                    ProgressSession.proxySummary(stored) ?? access.tr("代理地址格式无效，请填写主机:端口")).font(.caption)
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
                }.pickerStyle(.menu).accessibilityIdentifier("settings-language")
            }
            if session != nil, InstalledSources.library != nil {
                Section(header: Text(access.tr("内容翻译"))) {
                    Toggle(access.tr("自动翻译可见内容"), isOn: Binding(get: { access.privacy?.autoTranslateContent == true }, set: { active in
                        Task { await access.updatePreference("autoTranslateContent", json: String(active)) }
                    }))
                    Text(access.tr("翻译时，所选标题、简介、标签、评论、正文、字幕、弹幕及搜索词会发送给 Google 翻译。已命中当前语言词包的内容不发送；账号凭据、Cookie 和阅读记录不作为翻译数据发送。")).font(.caption)
                }
                Section(header: Text(access.tr("官方标签翻译包"))) {
                    Button(access.tr("翻译包管理")) { dictionaries = true }
                }
            }
            Section(header: Text(access.tr("首页兴趣"))) {
                ForEach(appleCategoryOptions) { option in
                    Toggle(access.tr(option.label), isOn: Binding(
                        get: { categories.contains(option.id) },
                        set: { enabled in
                            var selected = categories
                            if enabled { selected.insert(option.id) }
                            else { selected.remove(option.id) }
                            guard !selected.isEmpty,
                                  let data = try? JSONEncoder().encode(selected == allCategories ? [] : selected.sorted()) else { return }
                            Task { await access.updatePreference("homeCategories", json: String(decoding: data, as: UTF8.self)) }
                        }
                    )).disabled(categories.count == 1 && categories.contains(option.id))
                }
            }
            if access.failed { Text(access.tr("保存失败，请重试")) }
        }
        .disabled(access.busy)
        .sheet(isPresented: $subscriptionSettings) {
            if let session {
                SubscriptionPage(session: session, locale: access.locale, incognito: access.privacy?.incognito == true,
                    tr: access.tr, back: { subscriptionSettings = false }, fromSettings: true)
                #if os(macOS)
                .frame(minWidth: 460, minHeight: 390)
                #endif
            }
        }
        .sheet(isPresented: $dictionaries) {
            if let session, let library = InstalledSources.library {
                AppleTagPacks(session: session, library: library, initialLocale: access.locale, tr: access.tr)
            }
        }
        .sheet(isPresented: $editProxy) {
            VStack(alignment: .leading, spacing: 12) {
                Text(access.tr("HTTP 代理")).font(.headline)
                TextField(access.tr("填写代理主机:端口，留空表示不使用"), text: $proxy)
                    .accessibilityIdentifier("proxy-address")
                    .textFieldStyle(.roundedBorder).onChange(of: proxy) { _ in invalidProxy = false }
                if invalidProxy { Text(access.tr("代理地址格式无效，请填写主机:端口")) }
                if access.failed { Text(access.tr("保存失败，请重试")) }
                HStack {
                    Button(access.tr("取消")) { editProxy = false }
                    Button(access.tr("保存")) {
                        let value = proxy.trimmingCharacters(in: .whitespacesAndNewlines)
                        guard value.isEmpty || ProgressSession.proxySummary(value) != nil else { invalidProxy = true; return }
                        Task {
                            guard let encoded = try? JSONEncoder().encode(value) else { return }
                            await access.updatePreference("proxy", json: String(decoding: encoded, as: UTF8.self))
                            if !access.failed { editProxy = false }
                        }
                    }
                }
            }.padding().disabled(access.busy).interactiveDismissDisabled(access.busy)
        }
    }
}

@MainActor private struct AppleOnboardingPanel: View {
    @ObservedObject var access: LibraryAccess
    @State private var categories = Set(appleCategoryOptions.map { $0.id })

    private func complete(_ selected: Set<String>) {
        Task { await access.finishOnboarding(categories: selected, secureScreen: access.privacy?.secureScreen == true,
                                             blurCovers: access.privacy?.blurCovers == true) }
    }

    private var finish: some View {
        Button(access.tr("完成设置")) {
            complete(categories.count == appleCategoryOptions.count ? [] : categories)
        }.disabled(access.busy || categories.isEmpty)
    }

    private var skip: some View {
        Button(access.tr("跳过")) { complete([]) }.disabled(access.busy)
    }

    var body: some View {
        AppleSettingsForm {
            Text(access.tr("首次使用设置")).font(.title2)
            Text(access.tr("选择首页内容范围，并设置隐私保护选项。之后仍可在我的页面调整。"))
                .foregroundColor(.secondary)
            Section(header: Text(access.tr("隐私"))) {
                Toggle(access.tr("模糊封面"), isOn: Binding(get: { access.privacy?.blurCovers == true },
                    set: { value in Task { await access.updatePrivacy("blurCovers", enabled: value) } }))
                Toggle(access.tr("安全屏幕"), isOn: Binding(get: { access.privacy?.secureScreen == true },
                    set: { value in Task { await access.updatePrivacy("secureScreen", enabled: value) } }))
                Toggle(access.tr("应用锁"), isOn: Binding(
                    get: { access.privacy?.appLock == true },
                    set: { value in Task { await access.updatePrivacy("appLock", enabled: value) } }
                ))
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
            skip
            finish
            #endif
        }
        #if os(macOS)
        .safeAreaInset(edge: .bottom) {
            VStack(spacing: 0) {
                Divider()
                HStack { Spacer(); skip; finish.keyboardShortcut(.defaultAction) }.padding()
            }.background(.regularMaterial)
        }
        #endif
        .disabled(access.busy)
        .onAppear {
            if let saved = access.privacy?.homeCategories, !saved.isEmpty { categories = Set(saved) }
        }
    }
}
