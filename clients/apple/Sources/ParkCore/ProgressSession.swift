import Foundation

public struct LibraryPrivacy: Decodable, Sendable {
    public let adultConfirmed: Bool?
    public let incognito: Bool?
    public let appLock: Bool?
    public let appLockAfter: Int?
    public let secureScreen: Bool?
    public let language: String?
    public let subscriptionUpdateHours: Int?
    public let subscriptionNotifications: Bool?
    public let subscriptionNotificationPreview: Bool?

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
    var downloadRequests: [DownloadID: [UUID: Request]] = [:]

    private init(store: ProgressStore, directory: URL) {
        self.store = store
        self.directory = directory
    }

    public static func open(directory: URL) async throws -> ProgressSession {
        let store = try await Task.detached {
            try ProgressStore.openLibrary(supportDirectory: directory.path)
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

    public func libraryWorks() async throws -> [LibraryWork] {
        try await run { store, _ in try store.libraryWorks() }
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
