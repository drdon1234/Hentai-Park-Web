import Foundation

public struct AlbumAttachment: Identifiable, Sendable {
    public let id, kind: String
    public let preview: String?
    public let previewResource: Data?
    let order: Double?
    let raw: Data

    init(_ fields: [String: Any], locked: Bool) throws {
        guard let id = fields["id"] as? String, !id.isEmpty,
              let kind = fields["type"] as? String, ["image","video"].contains(kind) else { throw SiteReadError.malformed }
        self.id = id
        self.kind = kind
        self.order = fields["order"] as? Double
        raw = try JSONSerialization.data(withJSONObject: fields)
        previewResource = try (fields["previewResource"] as? [String:Any]).map { try JSONSerialization.data(withJSONObject: $0) }
        let address = (fields["preview"] as? String).flatMap { $0.isEmpty ? nil : $0 }
        preview = address ?? (previewResource == nil && kind == "image" && !locked && fields["previewOnly"] as? Bool != true ? fields["url"] as? String : nil)
    }
}

public struct AlbumEntry: Identifiable, Sendable {
    public let id, title, presentation: String
    public let date: Date?
    public let locked: Bool
    public fileprivate(set) var media: [AlbumAttachment]
    let raw: Data

    init(_ row: [String:Any]) throws {
        guard let id = row["id"] as? String, !id.isEmpty, let presentation = row["presentation"] as? String,
              ["work","post"].contains(presentation), let media = row["media"] as? [[String:Any]] else { throw SiteReadError.malformed }
        self.id = id
        self.presentation = presentation
        self.title = (row["title"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? id
        let locked = row["locked"] as? Bool == true
        self.locked = locked
        raw = try JSONSerialization.data(withJSONObject: row)
        self.media = try media.map { try AlbumAttachment($0, locked: locked) }
        guard Set(self.media.map(\.id)).count == self.media.count else { throw SiteReadError.malformed }
        let iso = ISO8601DateFormatter()
        let stamp = row["date"] as? String ?? ""
        iso.formatOptions = [.withInternetDateTime,.withFractionalSeconds]
        if let value = iso.date(from: stamp) { date = value }
        else { iso.formatOptions = [.withInternetDateTime]; date = iso.date(from: stamp) }
    }

    public func matches(from: Date?, through: Date?, calendar: Calendar = .current) -> Bool {
        guard from != nil || through != nil else { return true }
        guard let date else { return false }
        let day = calendar.startOfDay(for: date)
        return from.map { day >= calendar.startOfDay(for: $0) } != false &&
            through.map { day <= calendar.startOfDay(for: $0) } != false
    }
}

public struct SiteAlbum: Sendable {
    public private(set) var entries = [AlbumEntry]()
    public private(set) var page = 0
    public private(set) var cursor: String?
    private var cursors = Set<String>()
    public init() {}

    public mutating func append(response: Data) throws {
        guard let raw = try JSONSerialization.jsonObject(with: response) as? [String:Any],
              let rows = raw["items"] as? [[String:Any]], let more = raw["hasNext"] as? Bool else { throw SiteReadError.malformed }
        let next = raw["cursor"] as? String
        if more && (next?.isEmpty != false || cursors.contains(next ?? "")) { throw SiteReadError.malformed }
        var merged = entries
        var offsets = Dictionary(uniqueKeysWithValues: merged.enumerated().map { ($0.element.id,$0.offset) })
        for row in try rows.map(AlbumEntry.init) {
            if let index = offsets[row.id] {
                guard merged[index].presentation == row.presentation else { throw SiteReadError.malformed }
                // 新页若改变可见范围，应以新的整条记录更新所有预览权限。
                if merged[index].locked != row.locked { merged[index] = row; continue }
                var ids = Set(merged[index].media.map(\.id))
                merged[index].media.append(contentsOf: row.media.filter { ids.insert($0.id).inserted })
            } else { offsets[row.id] = merged.count; merged.append(row) }
        }
        for i in merged.indices where merged[i].media.allSatisfy({ $0.order?.isFinite == true }) {
            merged[i].media = merged[i].media.sorted { $0.order! < $1.order! }
        }
        entries = merged
        page += 1
        cursor = more ? next : nil
        if let cursor { cursors.insert(cursor) }
    }
}

public struct AlbumLayout: Decodable, Sendable {
    public let albumColumns: Int?
    public let albumCollapsed: Bool?
}

extension ProgressSession {
    public func albumLayout() async throws -> AlbumLayout { try await preferences(AlbumLayout.self) }
    public func saveAlbumColumns(_ count: Int) async throws {
        try await run { store, _ in try store.savePreference(name: "albumColumns", json: String(count)) }
    }
    public func saveAlbumCollapsed(_ collapsed: Bool) async throws {
        try await run { store, _ in try store.savePreference(name: "albumCollapsed", json: String(collapsed)) }
    }

    public func albumPage(_ library: SourceLibrary, site: InstalledSite, scope: Data, prior: SiteAlbum,
                          confirmation: ManualRead?) async throws -> SiteAlbum {
        guard site.album else { throw SiteReadError.malformed }
        let input = try JSONSerialization.jsonObject(with: scope)
        let request: [String:Any] = ["page":prior.page + 1, "cursor":prior.cursor as Any? ?? NSNull()]
        let args = try JSONSerialization.data(withJSONObject: [input, request])
        let response = try await siteRead(library, site: site.key, method: .subscriptionMedia,
                                          arguments: String(decoding: args, as: UTF8.self), confirmation: confirmation)
        var result = prior
        try result.append(response: Data(response.utf8))
        return result
    }

    public func albumThumbnail(_ library: SourceLibrary, site: String, media: AlbumAttachment) async throws -> PageImage? {
        if let url = media.preview { return try await siteImage(library, site: site, address: url) }
        guard let resource = media.previewResource else { return nil }
        let root = directory
        return try await run { store, request in
            try library.service(directory: root, request: request).albumPreview(source: site,
                resource: String(decoding: resource, as: UTF8.self), store: store, request: request)
        }
    }

    public func albumPost(_ entry: AlbumEntry, source: String) async throws -> SiteWork {
        guard entry.presentation == "post", var raw = try JSONSerialization.jsonObject(with: entry.raw) as? [String:Any] else { throw SiteReadError.malformed }
        raw["title"] = entry.title
        var blocks = [[String:Any]]()
        if let text = raw["text"] as? String, !text.isEmpty { blocks.append(["type":"text","text":text]) }
        for attachment in entry.media {
            let fields = try JSONSerialization.jsonObject(with: attachment.raw) as? [String:Any] ?? [:]
            if entry.locked || fields["previewOnly"] as? Bool == true {
                if let url = attachment.preview { blocks.append(["type":"image","url":url]) }
            } else if attachment.kind == "video" || fields["url"] is String { blocks.append(fields) }
        }
        raw["blocks"] = blocks
        raw["mediaKind"] = "article"
        var work = try await decodeSiteDetail(raw, site: source, summaryKeys: ["mediaKind","cover"])
        work.subscriptionPost = true
        return work
    }
}
