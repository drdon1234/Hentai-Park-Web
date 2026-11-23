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
            do { try await SubscriptionScheduler.shared.schedule(hours: settings.subscriptionHours) }
            catch { failed = true }
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

    func finishOnboarding(categories: Set<String>, secureScreen: Bool, appLock: Bool) async {
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
                try await session.completeOnboarding(categories: categories.sorted(), secureScreen: secureScreen, appLock: appLock)
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
    @State private var group = 0
    @State private var retry = 0
    private enum Destination {
        case sites, search, subscriptions, updates, rankings, recordings, settings
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
        .task(id: retry) { await model.refresh() }
        .onChange(of: downloads.revision) { _ in retry += 1 }
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
        if index != group { destination = nil; group = index }
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
                    detail(showBack: false)
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
                Toggle(access.tr("无痕模式"), isOn: Binding(get: { access.privacy?.incognito == true }, set: { enabled in
                    Task { await access.setIncognito(enabled) }
                })).disabled(access.busy)
                if access.failed { Text(access.tr("保存失败，请重试")) }
            }
        } else if model.failed {
            Text(access.tr("加载失败"))
            Button(access.tr("重试")) { retry += 1 }
        } else if let rows = model.works {
            let filtered = rows.filter { index == 0 ? $0.historyMs != nil : index == 1 ? $0.favorite : $0.downloaded }
            List {
                if index == 2 {
                    Button(access.tr("录制分卷")) { destination = .recordings }
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
                }
                Section(header: Text(access.tr(index == 0 ? "继续看" : labels[index]))) {
                    if filtered.isEmpty { Text(access.tr(["没有继续阅读的作品","还没有收藏","没有下载任务"][index])) }
                    ForEach(filtered, id: \.libraryID) { work in
                        VStack(alignment: .leading, spacing: 8) {
                            if index == 0, let chapter = work.historyChapter,
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
                                }
                            } else if index != 2 {
                                Button(work.title) { destination = .work(work) }
                            } else { Text(work.title) }

                            if index == 2, let state = work.downloadStatus, state != 3,
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
            AppleSettingsPanel(access: access, back: leaveChapter)
        } else if case .sites? = destination, let session = model.session {
            SourceDirectory(session:session, locale:access.locale, incognito:access.privacy?.incognito==true, tr:access.tr, close:leaveChapter)
        } else if case .work(let onlineWork)? = destination, let session = model.session {
            StoredSiteDetails(session:session,work:onlineWork,locale:access.locale,incognito:access.privacy?.incognito==true,tr:access.tr,back:leaveChapter).id(onlineWork)
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
            else {
                let previous = try await session.read(key)
                let history = work.historyChapter == key.chapterId ? work.historyPage : 0
                index = Int32(max(0, min(Int64(total - 1), previous?.pageIndex ?? history)))
            }
            let asset = try await session.pageImage(chapter, index: index)
            let pixels = try await decodedPage(asset,maxSize:4096)
            try Task.checkCancellation()
            if requested != nil && !incognito {
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

private struct AppleCategoryOption: Identifiable {
    let id: String
    let label: String
}

private let appleCategoryOptions: [AppleCategoryOption] = [
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

@MainActor private struct AppleSettingsPanel: View {
    @ObservedObject var access: LibraryAccess
    let back: () -> Void
    @State private var categories = Set(appleCategoryOptions.map { $0.id })

    var body: some View {
        Form {
            HStack {
                Button(access.tr("返回"), action: back)
                Text(access.tr("应用设置")).font(.headline)
            }
            Section(header: Text(access.tr("隐私与安全"))) {
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
        .onAppear {
            if let saved = access.privacy?.homeCategories, !saved.isEmpty { categories = Set(saved) }
        }
    }
}

@MainActor private struct AppleOnboardingPanel: View {
    @ObservedObject var access: LibraryAccess
    @State private var secureScreen = false
    @State private var appLock = false
    @State private var categories = Set(appleCategoryOptions.map { $0.id })

    var body: some View {
        Form {
            Text(access.tr("首次使用设置")).font(.title2)
            Text(access.tr("选择首页内容范围，并设置隐私保护选项。之后仍可在我的页面调整。"))
                .foregroundColor(.secondary)
            Section(header: Text(access.tr("隐私"))) {
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
            Button(access.tr("完成设置")) {
                let selected: Set<String> = categories.count == appleCategoryOptions.count ? [] : categories
                Task { await access.finishOnboarding(categories: selected, secureScreen: secureScreen, appLock: appLock) }
            }.disabled(access.busy || categories.isEmpty)
        }
        .disabled(access.busy)
        .onAppear {
            secureScreen = access.privacy?.secureScreen == true
            appLock = access.privacy?.appLock == true
            if let saved = access.privacy?.homeCategories, !saved.isEmpty { categories = Set(saved) }
        }
    }
}
