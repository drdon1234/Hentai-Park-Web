import Foundation

public struct SiteGameResource: Identifiable, Decodable, Sendable {
    public struct Channel: Identifiable, Decodable, Sendable {
        public let id, label, kind, replyText: String
        public let requiresReply: Bool
    }
    public let id, title, version, size, notes, testStatus: String
    public let platforms, languages: [String]
    public let expired: Bool
    public let channels: [Channel]
}

public struct GameResourceList: Sendable {
    public let entries: [SiteGameResource]
    fileprivate let offers: GameOffers
}

public struct ResolvedGameResource: Sendable {
    public struct Fields: Decodable, Sendable {
        public let urls, extractionCodes: [String]?
        public let extractionCode, archivePassword, original: String?
        public let direct, confirmRequired: Bool?
    }
    public let fields: Fields
    public let links: GameLinks
}

public struct GameDownloadRecord: Identifiable, Decodable, Sendable {
    public struct Item: Decodable, Sendable {
        public let source, id: String
        public let title: String?
    }
    public let key: [String]
    public var id: [String] { key }
    public let item: Item
    public let filename, status: String
    public let bytes, total: UInt64?
    public let extractionCode, archivePassword, file: String?
    public let missing: Bool
    public var active: Bool { status == "queued" || status == "running" }
    public var statusCode: Int32? {
        ["queued":0, "running":1, "paused":2, "done":3, "failed":4][status]
    }
}

extension ProgressSession {
    public func remembersGameReply(source: String) async throws -> Bool {
        struct Preferences: Decodable, Sendable { let replyWithoutAsking: [String]? }
        return try await preferences(Preferences.self).replyWithoutAsking?.contains(source) == true
    }

    public func rememberGameReply(source: String) async throws {
        try await run { store, _ in try store.rememberGameReply(source: source) }
    }

    public func gameResources(_ library: SourceLibrary, source: String, work: String) async throws -> GameResourceList {
        let root=directory
        return try await run { _, request in
            let offers=try library.service(directory: root, request: request).gameOffers(source: source, work: work, request: request)
            let entries=try JSONDecoder().decode([SiteGameResource].self, from: Data(offers.entries().utf8))
            return GameResourceList(entries: entries, offers: offers)
        }
    }

    public func resolveGameChannel(_ list: GameResourceList, resource: String, channel: SiteGameResource.Channel,
                                   confirmedReply: String?) async throws -> ResolvedGameResource {
        try await run { _, request in
            let links=try list.offers.resolve(resource: resource, channel: channel.id, confirmedReply: confirmedReply, request: request)
            let fields=try JSONDecoder().decode(ResolvedGameResource.Fields.self, from: Data(links.details().utf8))
            return ResolvedGameResource(fields: fields, links: links)
        }
    }

    public func gameDownloads() async throws -> [GameDownloadRecord] {
        try await run { store, _ in
            let data=try store.gameDownloads()
            return try JSONDecoder().decode([GameDownloadRecord].self, from: Data(data.utf8))
        }
    }

    public func changeGameDownload(_ key: [String], action: String) async throws {
        try await run { store, _ in try store.changeGameDownload(identity:key, action:action) }
        if action == "pause" { gameRequests[key]?.cancel() }
    }

    func transferGame(_ library: SourceLibrary, key: [String], grant: GameLinks?) async throws {
        let request=Request(), root=directory
        gameRequests[key]=request
        defer { gameRequests.removeValue(forKey:key) }
        try await run(request: request) { store, request in
            try library.service(directory:root,request:request).runGameDownload(store:store,identity:key,resolved:grant,request:request)
        }
    }
}
