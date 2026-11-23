import Foundation

public struct LibraryPrivacy: Decodable, Sendable {
    public let adultConfirmed: Bool?
    public let onboarded: Bool?
    public let incognito: Bool?
    public let appLock: Bool?
    public let appLockAfter: Int?
    public let secureScreen: Bool?
    public let blurCovers: Bool?
    public let language: String?
    public let subscriptionUpdateHours: Int?
    public let subscriptionNotifications: Bool?
    public let subscriptionNotificationPreview: Bool?
    public let homeCategories: [String]?
    public let homeDimension: String?
    public let homeMedia: String?
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

    public func clearCache(home: Bool) async throws {
        let cache = try FileManager.default.url(for: .cachesDirectory, in: .userDomainMask,
                                               appropriateFor: nil, create: true)
        try await run { store, _ in try store.clearStorageCache(cacheDirectory: cache.path, home: home) }
    }

    public func deleteReadDownloads() async throws -> UInt64 {
        try await run { store, _ in try store.deleteReadDownloads() }
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

    public func setPreference(_ name: String, json: String) async throws {
        guard ["language", "homeCategories", "domainOrder", "proxy", "appLockAfter", "subscriptionUpdateHours", "onboarded"].contains(name) else {
            throw CocoaError(.coderInvalidValue)
        }
        try await run { store, _ in try store.savePreference(name: name, json: json) }
    }

    public func setDiscoveryScope(dimension: String, media: String) async throws {
        try await run { store, _ in try store.saveDiscoveryScope(dimension: dimension, media: media) }
    }

    public func completeOnboarding(categories: [String], secureScreen: Bool, appLock: Bool, blurCovers: Bool = false) async throws {
        let json = String(decoding: try JSONEncoder().encode(categories), as: UTF8.self)
        try await run { store, _ in
            try store.savePreference(name: "homeCategories", json: json)
            try store.savePreference(name: "blurCovers", json: blurCovers ? "true" : "false")
            try store.savePreference(name: "secureScreen", json: secureScreen ? "true" : "false")
            try store.savePreference(name: "appLock", json: appLock ? "true" : "false")
            try store.savePreference(name: "onboarded", json: "true")
        }
    }

    public func libraryWorks() async throws -> [LibraryWork] {
        try await run { store, _ in try store.libraryWorks() }
    }

    public func shelfCategories() async throws -> [LibraryCategory] {
        try await run { store, _ in try store.libraryCategories(source: nil, work: nil) }
    }

    public func shelfPreferences() async throws -> ShelfPreferences {
        try await preferences(ShelfPreferences.self)
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
    }

    public func createShelfCategory(_ name: String) async throws {
        try await run { store, _ in try store.nameCategory(id: nil, name: name) }
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

    public func pageImage(_ images: ChapterImages, index: Int32) async throws -> PageImage {
        try await run { _, request in try images.image(index: index, crop: false, preload: false, request: request) }
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
