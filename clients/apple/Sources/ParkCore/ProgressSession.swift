import Foundation

public extension Notification.Name {
    static let parkReaderModeChanged = Notification.Name("ParkReaderModeChanged")
    static let parkHistoryChanged = Notification.Name("ParkHistoryChanged")
    static let parkFavoritesChanged = Notification.Name("ParkFavoritesChanged")
}

/// 一个进程的认证状态。窗口返回时按单调时钟计算，取消后只接受显式重试。
public struct LibraryLockState: Sendable {
    public private(set) var locked = true
    private var automatic = false
    private var away: TimeInterval?
    public init() {}

    public mutating func load(enabled: Bool) {
        locked = enabled
        automatic = enabled
        away = nil
    }

    public mutating func leave(at time: TimeInterval) {
        if away == nil { away = time }
    }

    public mutating func resume(at time: TimeInterval, grace: Int) {
        defer { away = nil }
        if !locked, let away, time - away >= Double(max(0, grace)) {
            locked = true
            automatic = true
        }
    }

    public mutating func takeAutomaticPrompt() -> Bool {
        guard locked && automatic else { return false }
        automatic = false
        return true
    }

    public mutating func authenticated() { load(enabled: false) }
}

public struct LibraryPrivacy: Decodable, Sendable {
    public let adultConfirmed: Bool?
    public let onboarded: Bool?
    public let incognito: Bool?
    public let appLock: Bool?
    public let appLockAfter: Int?
    public let secureScreen: Bool?
    public let blurCovers: Bool?
    public let language: String?
    public let themeMode: Int?
    public let seedColor: UInt32?
    public let autoTranslateContent: Bool?
    public let subscriptionUpdateHours: Int?
    public let subscriptionNotifications: Bool?
    public let subscriptionNotificationPreview: Bool?
    public let homeCategories: [String]?
    public let homeDimension: String?
    public let homeMedia: String?
    public let homeExpandedDomains, homeHiddenSources: [String]?
    public let dataSavingMode: String?
    public let domainOrder: [String]?
    public let proxy: String?

    public var subscriptionHours: Int {
        adultConfirmed == true ? max(0, subscriptionUpdateHours ?? 6) : 0
    }

    public func permitsNotificationPresentation(includesPreview: Bool) -> Bool {
        permitsSubscriptionNotification && (!includesPreview ||
            (subscriptionNotificationPreview == true && appLock != true && secureScreen != true))
    }

    public var permitsSubscriptionNotification: Bool {
        adultConfirmed == true && subscriptionNotifications == true && incognito != true
    }
}

public enum ComicReadingMode: Int32, CaseIterable, Decodable, Sendable {
    case vertical, leftToRight, rightToLeft

    public var label: String {
        switch self {
        case .vertical: return "上下滚动（条漫）"
        case .leftToRight: return "左右翻页（从左到右）"
        case .rightToLeft: return "左右翻页（日漫，从右到左）"
        }
    }
}

public struct ComicReadingSettings: Sendable {
    public let workMode: ComicReadingMode?
    public let defaultMode: ComicReadingMode
    public var effective: ComicReadingMode { workMode ?? defaultMode }
}

private struct ComicDefaults: Decodable, Sendable {
    var readerMode: Int32?
}

public struct ComicAppearance: Decodable, Equatable, Sendable {
    public var background: String
    public var showPageNumber: Bool
    public var keepAwake: Bool
    public var orientation: String
    public var dim: Double
    public var warmth: Double
    public var gray: Bool
    public var inverted: Bool
    public var gap: Double
    public var animated: Bool
    public var dual: String
    public var coverSingle: Bool
    public var splitWide: Bool
    public var crop: Bool
    public var tapEnabled: Bool
    public var tapLayout: Int
    public var invertH: Bool
    public var invertV: Bool

    private enum Keys: String, CodingKey {
        case readerBackground, readerShowPageNumber, keepScreenOn, readerOrientation, readerDim, readerWarm, readerGrayscale, readerInvert
        case readerGap, readerAnimate, readerDual, readerCoverSingle, readerSplitWide, readerCrop
        case tapToPage, readerTapLayout, readerTapInvertH, readerTapInvertV
    }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: Keys.self)
        background = try values.decodeIfPresent(String.self, forKey: .readerBackground) ?? "black"
        showPageNumber = try values.decodeIfPresent(Bool.self, forKey: .readerShowPageNumber) ?? true
        keepAwake = try values.decodeIfPresent(Bool.self, forKey: .keepScreenOn) ?? true
        orientation = try values.decodeIfPresent(String.self, forKey: .readerOrientation) ?? "auto"
        dim = try values.decodeIfPresent(Double.self, forKey: .readerDim) ?? 0
        warmth = try values.decodeIfPresent(Double.self, forKey: .readerWarm) ?? 0
        gray = try values.decodeIfPresent(Bool.self, forKey: .readerGrayscale) ?? false
        inverted = try values.decodeIfPresent(Bool.self, forKey: .readerInvert) ?? false
        gap = try values.decodeIfPresent(Double.self, forKey: .readerGap) ?? 0
        animated = try values.decodeIfPresent(Bool.self, forKey: .readerAnimate) ?? true
        dual = try values.decodeIfPresent(String.self, forKey: .readerDual) ?? "off"
        coverSingle = try values.decodeIfPresent(Bool.self, forKey: .readerCoverSingle) ?? true
        splitWide = try values.decodeIfPresent(Bool.self, forKey: .readerSplitWide) ?? false
        crop = try values.decodeIfPresent(Bool.self, forKey: .readerCrop) ?? false
        tapEnabled = try values.decodeIfPresent(Bool.self, forKey: .tapToPage) ?? true
        tapLayout = try values.decodeIfPresent(Int.self, forKey: .readerTapLayout) ?? 0
        invertH = try values.decodeIfPresent(Bool.self, forKey: .readerTapInvertH) ?? false
        invertV = try values.decodeIfPresent(Bool.self, forKey: .readerTapInvertV) ?? false
    }

    public mutating func resetFilters() {
        dim = 0; warmth = 0; gray = false; inverted = false
    }

    fileprivate func patch(from original: Self) throws -> String {
        var changed: [String: Any] = [:]
        if background != original.background { changed["readerBackground"] = background }
        if showPageNumber != original.showPageNumber { changed["readerShowPageNumber"] = showPageNumber }
        if keepAwake != original.keepAwake { changed["keepScreenOn"] = keepAwake }
        if orientation != original.orientation { changed["readerOrientation"] = orientation }
        if dim != original.dim { changed["readerDim"] = dim }
        if warmth != original.warmth { changed["readerWarm"] = warmth }
        if gray != original.gray { changed["readerGrayscale"] = gray }
        if inverted != original.inverted { changed["readerInvert"] = inverted }
        if gap != original.gap { changed["readerGap"] = gap }
        if animated != original.animated { changed["readerAnimate"] = animated }
        if dual != original.dual { changed["readerDual"] = dual }
        if coverSingle != original.coverSingle { changed["readerCoverSingle"] = coverSingle }
        if splitWide != original.splitWide { changed["readerSplitWide"] = splitWide }
        if crop != original.crop { changed["readerCrop"] = crop }
        if tapEnabled != original.tapEnabled { changed["tapToPage"] = tapEnabled }
        if tapLayout != original.tapLayout { changed["readerTapLayout"] = tapLayout }
        if invertH != original.invertH { changed["readerTapInvertH"] = invertH }
        if invertV != original.invertV { changed["readerTapInvertV"] = invertV }
        return String(decoding: try JSONSerialization.data(withJSONObject: changed), as: UTF8.self)
    }
}

public struct ComicSpread: Equatable, Sendable {
    public enum Half: Sendable { case left, right }
    public let first: Int32
    public let second: Int32?
    public let half: Half?

    public static func pages(aspects: [Double], dual: Bool, coverSingle: Bool, splitWide: Bool, rtl: Bool) -> [Self] {
        var spreads = [Self]()
        var index = 0
        while index < aspects.count {
            let page = Int32(index)
            if dual && !(index == 0 && coverSingle) && aspects[index] <= 1 &&
                index + 1 < aspects.count && aspects[index + 1] <= 1 {
                spreads.append(Self(first: page, second: page + 1, half: nil))
                index += 2
            } else {
                if !dual && splitWide && aspects[index] > 1 {
                    spreads.append(Self(first: page, second: nil, half: rtl ? .right : .left))
                    spreads.append(Self(first: page, second: nil, half: rtl ? .left : .right))
                } else {
                    spreads.append(Self(first: page, second: nil, half: nil))
                }
                index += 1
            }
        }
        return spreads
    }
}

extension ComicAppearance {
    public enum TapResult { case previous, next, controls }
    public func tap(x: Double, y: Double, mode: ComicReadingMode) -> TapResult {
        guard tapEnabled else { return .controls }
        let horizontal = (mode == .rightToLeft) != invertH ? 1 - x : x
        let vertical = invertV ? 1 - y : y
        let middle = (1.0 / 3)...(2.0 / 3)
        let left = horizontal < middle.lowerBound, top = vertical < middle.lowerBound
        switch tapLayout == 0 ? (mode == .vertical ? 2 : 1) : tapLayout {
        case 1: return left ? .previous : horizontal > middle.upperBound ? .next : .controls
        case 2:
            if middle.contains(horizontal) && middle.contains(vertical) { return .controls }
            return top || left && vertical <= middle.upperBound ? .previous : .next
        case 3: return top ? .controls : left ? .previous : .next
        case 4: return !middle.contains(horizontal) ? .next : vertical > middle.upperBound ? .previous : .controls
        default: return .controls
        }
    }
}

public struct NovelPreferences: Decodable, Sendable {
    public let novelFontSize: Double?
    public let translateEngine: String?
}

/// 页面或窗口拥有一个会话。核心调用不占主线程，关闭会话会等待已接受的调用退出。
public actor ProgressSession {
    private struct Pending: Sendable {
        let request: Request
        let join: @Sendable () async -> Void
    }

    private var store: ProgressStore?
    let directory: URL
    private var pending: [UUID: Pending] = [:]
    var downloadRequests: [LibraryWorkID: [UUID: Request]] = [:]
    var gameRequests: [[String]: Request] = [:]

    private init(store: ProgressStore, directory: URL) {
        self.store = store
        self.directory = directory
    }

    public static func open(directory: URL) async throws -> ProgressSession {
        let store = try await Task.detached {
            #if os(macOS) || os(iOS)
            return try ProgressStore.openLibraryWithBrowser(supportDirectory: directory.path, browser: SiteCookieRecovery())
            #else
            return try ProgressStore.openLibrary(supportDirectory: directory.path)
            #endif
        }.value
        try Task.checkCancellation()
        return ProgressSession(store: store, directory: directory)
    }

    #if os(macOS) || os(iOS)
    public static func openApplicationLibrary() async throws -> ProgressSession {
        var directory = try FileManager.default.url(for: .applicationSupportDirectory,
                                                    in: .userDomainMask, appropriateFor: nil, create: true)
        #if os(macOS)
        // 与原宿主的 path_provider_foundation 一致，macOS 的支持目录包含 Bundle ID。
        guard let id = Bundle.main.bundleIdentifier, !id.isEmpty else {
            throw CocoaError(.fileNoSuchFile)
        }
        directory.appendPathComponent(id, isDirectory: true)
        #endif
        return try await open(directory: directory)
    }
    #endif

    public func read(_ key: ChapterKey) async throws -> ChapterProgress? {
        try await run { store, _ in try store.read(key: key) }
    }

    public func privacySettings() async throws -> LibraryPrivacy {
        try await preferences(LibraryPrivacy.self)
    }

    public func readChapterIDs(source: String, work: String) async throws -> Set<String> {
        try await run { store, _ in Set(try store.readChapterIds(source: source, work: work)) }
    }

    public func markChapters(_ chapters: [OfflineChapter], source: String, work: String, read: Bool) async throws {
        try await run { store, _ in try store.markChaptersRead(source: source, work: work, chapters: chapters, read: read) }
    }

    public func clearCache(home: Bool) async throws {
        let cache = try FileManager.default.url(for: .cachesDirectory, in: .userDomainMask,
                                               appropriateFor: nil, create: true)
        try await run { store, _ in try store.clearStorageCache(cacheDirectory: cache.path, home: home) }
    }

    public func deleteReadDownloads() async throws -> UInt64 {
        try await run { store, _ in try store.deleteReadDownloads() }
    }

    public func comicReadingSettings(_ key: ChapterKey) async throws -> ComicReadingSettings {
        let defaults = try await preferences(ComicDefaults.self)
        let stored = try await run { store, _ in try store.readerMode(key: key) }
        return ComicReadingSettings(workMode: stored.flatMap(ComicReadingMode.init(rawValue:)),
            defaultMode: defaults.readerMode.flatMap(ComicReadingMode.init(rawValue:)) ?? .vertical)
    }

    public func setComicMode(_ key: ChapterKey, mode: ComicReadingMode?) async throws {
        try await run { store, _ in try store.setReaderMode(key: key, mode: mode?.rawValue) }
        await MainActor.run { NotificationCenter.default.post(name: .parkReaderModeChanged, object: nil) }
    }

    public func promoteComicMode(_ key: ChapterKey) async throws {
        try await run { store, _ in
            guard let raw = try store.readerMode(key: key), ComicReadingMode(rawValue: raw) != nil else {
                throw CocoaError(.coderInvalidValue)
            }
            try store.savePreference(name: "readerMode", json: String(raw))
        }
        await MainActor.run { NotificationCenter.default.post(name: .parkReaderModeChanged, object: nil) }
    }

    public func comicAppearance() async throws -> ComicAppearance { try await preferences(ComicAppearance.self) }

    public func saveComicAppearance(_ values: ComicAppearance, from original: ComicAppearance) async throws {
        guard original != values else { return }
        let json = try values.patch(from: original)
        try await run { store, _ in try store.saveReaderPreferences(json: json) }
        await MainActor.run { NotificationCenter.default.post(name: .parkReaderModeChanged, object: nil) }
    }

    public func novelSettings() async throws -> NovelPreferences {
        try await preferences(NovelPreferences.self)
    }

    func preferences<T: Decodable & Sendable>(_ type: T.Type) async throws -> T {
        let file = directory.appendingPathComponent("park/settings.json")
        return try await run { _, _ in
            let data: Data
            do { data = try Data(contentsOf: file) }
            catch let error as CocoaError where error.code == .fileReadNoSuchFile {
                data = Data("{}".utf8)
            }
            return try JSONDecoder().decode(type, from: data)
        }
    }

    public func confirmAge() async throws {
        try await run { store, _ in try store.savePreference(name: "adultConfirmed", json: "true") }
    }

    public func setIncognito(_ enabled: Bool) async throws {
        try await run { store, _ in try store.savePreference(name: "incognito", json: enabled ? "true" : "false") }
    }

    public func setPrivacyPreference(_ name: String, enabled: Bool) async throws {
        guard ["blurCovers", "secureScreen", "appLock", "subscriptionNotifications", "subscriptionNotificationPreview"].contains(name) else {
            throw CocoaError(.coderInvalidValue)
        }
        try await run { store, _ in try store.savePreference(name: name, json: enabled ? "true" : "false") }
    }

    public static func proxySummary(_ value: String) -> String? {
        guard !value.contains(where: { $0.isWhitespace || $0 == "\\" }),
              let url = URLComponents(string: value.contains("://") ? value : "http://" + value),
              ["http","https"].contains(url.scheme?.lowercased() ?? ""), url.user == nil, url.password == nil,
              let host = url.host, !host.isEmpty, url.url != nil,
              let port = url.port, (1...65535).contains(port),
              url.path.isEmpty || url.path == "/", url.query == nil, url.fragment == nil else { return nil }
        return host + ":" + String(port)
    }

    public func setPreference(_ name: String, json: String) async throws {
        guard ["language", "themeMode", "seedColor", "autoTranslateContent", "homeCategories", "domainOrder", "proxy", "appLockAfter", "subscriptionUpdateHours", "onboarded"].contains(name) else {
            throw CocoaError(.coderInvalidValue)
        }
        if name == "proxy" {
            let value = try JSONDecoder().decode(String.self, from: Data(json.utf8))
            guard value.isEmpty || Self.proxySummary(value) != nil else { throw CocoaError(.coderInvalidValue) }
        }
        try await run { store, _ in try store.savePreference(name: name, json: json) }
    }

    public func setDiscoveryScope(dimension: String, media: String) async throws {
        try await run { store, _ in try store.saveDiscoveryScope(dimension: dimension, media: media) }
    }

    public func setHomeDomain(_ domain: String, expanded: Bool) async throws -> [String] {
        try await run { store, _ in try store.setHomeMembership(name: "homeExpandedDomains", entry: domain, enabled: expanded) }
    }

    public func setHomeSite(_ source: String, visible: Bool) async throws -> [String] {
        try await run { store, _ in try store.setHomeMembership(name: "homeHiddenSources", entry: source, enabled: !visible) }
    }

    public func completeOnboarding(categories: [String], secureScreen: Bool, blurCovers: Bool = false) async throws {
        try await run { store, _ in
            try store.completeOnboarding(categories: categories, secureScreen: secureScreen, blurCovers: blurCovers)
        }
    }

    public func libraryWorks() async throws -> [LibraryWork] {
        try await run { store, _ in try store.libraryWorks() }
    }

    public func removeHistory(_ works: [LibraryWork]) async throws {
        let entries = works.compactMap { work -> HistoryPosition? in
            guard let time = work.historyMs else { return nil }
            return HistoryPosition(source: work.source, work: work.workId, chapterTitle: nil, pageCount: 0,
                                   position: nil, timeMs: time)
        }
        try await run { store, _ in try store.removeHistoryPositions(entries: entries) }
        await MainActor.run { NotificationCenter.default.post(name: .parkHistoryChanged, object: nil) }
    }

    public func historyGrid() async throws -> Bool {
        struct Layout: Decodable, Sendable { let historyDisplay: String? }
        return try await preferences(Layout.self).historyDisplay == "grid"
    }

    public func setHistoryGrid(_ enabled: Bool) async throws {
        try await run { store, _ in try store.savePreference(name: "historyDisplay", json: enabled ? "\"grid\"" : "\"list\"") }
    }

    public func shelfCategories() async throws -> [LibraryCategory] {
        try await run { store, _ in try store.libraryCategories(source: nil, work: nil) }
    }

    public func downloadCategories(_ work: LibraryWork) async throws -> [LibraryCategory] {
        try await run { store, _ in try store.libraryCategories(source: work.source, work: work.workId) }
    }

    public func editDownloadCategories(_ work: LibraryWork, added: [Int64], removed: [Int64]) async throws {
        try await run { store, _ in
            try store.editDownloadCategories(source: work.source, work: work.workId, chapter: nil, added: added, removed: removed)
        }
        await Self.favoritesChanged([])
    }

    public func isFavorite(source: String, work: String) async throws -> Bool {
        try await run { store, _ in try store.isFavorite(source: source, work: work) }
    }

    @MainActor static func favoritesChanged(_ affected: Set<LibraryWorkID>) {
        NotificationCenter.default.post(name: .parkFavoritesChanged, object: affected)
    }

    public func similarFavorite(source: String, id: String, title: String) async throws -> SimilarFavorite? {
        try await run { store, _ in try store.similarFavorite(source: source, id: id, title: title) }
    }

    public func askForFavoriteCategory(_ enabled: Bool) async throws {
        try await run { store, _ in try store.savePreference(name: "askCategoryOnFavorite", json: String(enabled)) }
    }

    public func shelfPreferences() async throws -> ShelfPreferences {
        try await preferences(ShelfPreferences.self)
    }

    public func shelfGrid() async throws -> Bool {
        struct Layout: Decodable, Sendable { let libraryDisplay: String? }
        return try await preferences(Layout.self).libraryDisplay != "list"
    }

    public func setShelfGrid(_ enabled: Bool) async throws {
        try await run { store, _ in
            try store.savePreference(name: "libraryDisplay", json: enabled ? "\"grid\"" : "\"list\"")
        }
        await Self.favoritesChanged([])
    }

    public func saveShelfOptions(_ options: ShelfPreferences) async throws {
        let encoded = String(decoding: try JSONEncoder().encode(options), as: UTF8.self)
        try await run { store, _ in try store.saveShelfPreferences(json: encoded) }
    }

    public func favoriteStats() async throws -> [String: FavoriteCardInfo] {
        try await run { store, _ in
            try JSONDecoder().decode([String: FavoriteCardInfo].self, from: Data(try store.favoriteCardStats().utf8))
        }
    }

    public func editFavorites(_ works: [LibraryWork], action: FavoriteEdit) async throws {
        let keys = works.map { FavoriteIdentity(source: $0.source, work: $0.workId) }
        try await run { store, _ in try store.editFavorites(works: keys, action: action) }
        if case .remove = action {
            await Self.favoritesChanged(Set(works.map(\.libraryID)))
        } else {
            await Self.favoritesChanged([])
        }
    }

    public func nameShelfCategory(_ name: String, id: Int64? = nil) async throws {
        try await run { store, _ in try store.nameCategory(id: id, name: name) }
        await Self.favoritesChanged([])
    }

    public func removeShelfCategory(_ id: Int64) async throws {
        try await run { store, _ in try store.removeCategory(id: id) }
        await Self.favoritesChanged([])
    }

    public func reorderShelfCategories(_ ids: [Int64]) async throws {
        try await run { store, _ in try store.reorderCategories(ordered: ids) }
        await Self.favoritesChanged([])
    }

    public func chapterImages(_ key: ChapterKey, cache: URL) async throws -> ChapterImages {
        try await run { store, _ in try store.chapterImages(key: key, cacheDirectory: cache.path) }
    }

    public func offlineVideo(_ key: ChapterKey) async throws -> PlaybackSession {
        try await run { store, request in try store.offlineVideo(key: key, request: request) }
    }

    public func offlineNovel(_ key: ChapterKey, locale: String, engine: String) async throws -> NovelDocument {
        try await run { store, _ in try store.offlineNovel(key: key, locale: locale, engine: engine) }
    }

    public func setNovelFontSize(_ size: Double) async throws {
        guard size.isFinite, (12...42).contains(size) else { throw CocoaError(.fileWriteUnknown) }
        try await run { store, _ in try store.savePreference(name: "novelFontSize", json: String(size)) }
    }

    public func pageImage(_ images: ChapterImages, index: Int32, crop: Bool = false) async throws -> PageImage {
        try await run { _, request in try images.image(index: index, crop: crop, preload: false, request: request) }
    }

    public func turnPage(_ turn: PageTurn) async throws -> ChapterProgress {
        try await run { try $0.turnPage(turn: turn, request: $1) }
    }

    public func snapshot() async throws -> ProgressSnapshot {
        try await run { try $0.snapshot(request: $1) }
    }

    public func changes(since revision: Int64) async throws -> Changes {
        try await run { store, _ in try store.changesSince(after: revision) }
    }

    public func savePosition(_ position: MediaPosition) async throws -> ChapterProgress {
        try await run { try $0.savePosition(position: position, request: $1) }
    }

    func run<T: Sendable>(request: Request = Request(), _ operation: @escaping @Sendable (ProgressStore, Request) throws -> T) async throws -> T {
        guard let store else { throw CancellationError() }
        try Task.checkCancellation()
        let id = UUID()
        let task = Task.detached { try operation(store, request) }
        pending[id] = Pending(request: request, join: { _ = await task.result })
        defer { pending.removeValue(forKey: id) }
        do {
            let result = try await withTaskCancellationHandler {
                try await task.value
            } onCancel: {
                request.cancel()
            }
            try Task.checkCancellation()
            guard self.store != nil else { throw CancellationError() }
            return result
        } catch StorageError.Cancelled {
            throw CancellationError()
        } catch SourceFailure.Cancelled {
            throw CancellationError()
        }
    }

    public func close() async {
        store = nil
        let calls = Array(pending.values)
        for call in calls { call.request.cancel() }
        for call in calls { await call.join() }
    }
}

public struct FavoriteCardInfo: Decodable, Sendable {
    public let added, updated, unread: Int64
    public let downloaded, oneShot: Bool
    public let cover: String?
}

public struct ShelfPreferences: Codable, Sendable {
    public var librarySort = "added", libraryFilterKind = "all"
    public var librarySortAsc = false, libraryFilterUnread = false, libraryFilterDownloaded = false
    public var libraryFilterSources: [String] = []
    public var gridColumns = 0
    public var badgeSource = true, badgeUnread = true, badgeDownloaded = true

    public init() {}
    public init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        librarySort = try fields.decodeIfPresent(String.self, forKey: .librarySort) ?? librarySort
        libraryFilterKind = try fields.decodeIfPresent(String.self, forKey: .libraryFilterKind) ?? libraryFilterKind
        librarySortAsc = try fields.decodeIfPresent(Bool.self, forKey: .librarySortAsc) ?? librarySortAsc
        libraryFilterUnread = try fields.decodeIfPresent(Bool.self, forKey: .libraryFilterUnread) ?? libraryFilterUnread
        libraryFilterDownloaded = try fields.decodeIfPresent(Bool.self, forKey: .libraryFilterDownloaded) ?? libraryFilterDownloaded
        libraryFilterSources = try fields.decodeIfPresent([String].self, forKey: .libraryFilterSources) ?? []
        let columns = try fields.decodeIfPresent(Int.self, forKey: .gridColumns) ?? 0
        gridColumns = [2, 3, 4].contains(columns) ? columns : 0
        badgeSource = try fields.decodeIfPresent(Bool.self, forKey: .badgeSource) ?? badgeSource
        badgeUnread = try fields.decodeIfPresent(Bool.self, forKey: .badgeUnread) ?? badgeUnread
        badgeDownloaded = try fields.decodeIfPresent(Bool.self, forKey: .badgeDownloaded) ?? badgeDownloaded
    }
}
