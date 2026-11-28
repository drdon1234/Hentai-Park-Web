import Foundation

extension ProgressSession {
    public func editStoredFavoriteCategories(_ work: LibraryWork, added: [Int64], removed: [Int64]) async throws {
        try await run { store, _ in try store.editFavoriteCategories(source: work.source, work: work.workId, added: added, removed: removed) }
        await Self.favoritesChanged([work.libraryID])
    }
    public func setStoredFavorite(_ work: LibraryWork, enabled: Bool) async throws {
        try await run { store, _ in try store.setStoredFavorite(source: work.source, work: work.workId, enabled: enabled) }
        await Self.favoritesChanged([work.libraryID])
    }

    public func storedAuthors(_ work: LibraryWork) async throws -> [String] {
        try await run { store, _ in
            guard let raw = try store.storedWork(source: work.source, work: work.workId) else { return [] }
            let values = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String:Any]
            return values?["authors"] as? [String] ?? []
        }
    }

    public func blockHistoryWork(_ work: LibraryWork, author: String? = nil) async throws {
        try await run { store, _ in try store.blockLibraryItem(source: work.source, work: work.workId, title: work.title, author: author) }
        await MainActor.run { NotificationCenter.default.post(name: .parkHistoryChanged, object: nil) }
    }

    public func hiddenHistoryIDs(_ rows: [LibraryWork]) async throws -> Set<LibraryWorkID> {
        let file = directory.appendingPathComponent("park/settings.json")
        return try await run { store, _ in
            let data: Data
            do { data = try Data(contentsOf: file) } catch let error as CocoaError where error.code == .fileReadNoSuchFile { return [] }
            struct AuthorRule: Decodable { let works: [String] }
            struct Rules: Decodable { let blockedWorks: [String:String]?; let blockedAuthors: [String:AuthorRule]? }
            let rules = try JSONDecoder().decode(Rules.self, from: data)
            guard rules.blockedWorks?.isEmpty == false || rules.blockedAuthors?.isEmpty == false else { return [] }
            var hidden = Set<LibraryWorkID>()
            let authorWorks = Set((rules.blockedAuthors ?? [:]).values.flatMap(\.works))
            for row in rows where row.historyMs != nil {
                let uid = row.source + ":" + row.workId
                if rules.blockedWorks?[uid] != nil || authorWorks.contains(uid) {
                    hidden.insert(row.libraryID)
                } else if let authors = rules.blockedAuthors, !authors.isEmpty,
                          let raw = try store.storedWork(source: row.source, work: row.workId) {
                    let meta = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String:Any]
                    let names = meta?["authors"] as? [String] ?? []
                    if names.contains(where: { authors[row.source + ":" + $0.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()] != nil }) {
                        hidden.insert(row.libraryID)
                    }
                }
            }
            return hidden
        }
    }

    public struct Interaction: Identifiable, Decodable, Sendable {
        public let type: String
        public let value: PluginOptionValue?
        public let canUnset: Bool?
        public var id: String { type }
    }

    public func siteInteractions(_ library: SourceLibrary, work: LibraryWork, change: SitePreference? = nil) async throws -> [Interaction] {
        let root = directory
        return try await run { _, request in
            let response = try library.service(directory: root, request: request).siteInteractions(source: work.source,
                work: work.workId, change: change, request: request)
            return try JSONDecoder().decode([Interaction].self, from: Data(response.utf8))
        }
    }
}
