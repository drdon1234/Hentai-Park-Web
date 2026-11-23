import Foundation

public struct SiteComment: Decodable, Identifiable, Sendable {
  public struct Author: Decodable, Sendable { public let name: String? }
  public struct Part: Decodable, Sendable {
    public let type: String
    public let text: String?
    public let url: String?
  }
  public let id: String
  public let author: Author?
  public let body: [Part]
}

public enum DetailEntry: Identifiable, Sendable {
  case comment(SiteComment), work(SiteWork)
  public var id: String {
    switch self {
    case .comment(let row): return row.id
    case .work(let row): return row.id
    }
  }
}

public enum DetailSectionKind: Equatable, Sendable { case comments, related }

public struct DetailEntryPage: Sendable {
  public let entries: [DetailEntry]
  public let cursor: PluginOptionValue?
  public let nextPage: Int?
  public let reset: Bool
}

extension ProgressSession {
  public func detailEntries(_ library: SourceLibrary, site: InstalledSite, work: SiteWork,
    section: DetailSectionKind, page: Int, cursor: PluginOptionValue?, confirmation: ManualRead?
  ) async throws -> DetailEntryPage {
    var options: [String: Any] = ["page": page]
    if let cursor {
      options["cursor"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(cursor), options: .fragmentsAllowed)
    }
    let method: BrowseMethod
    switch section {
    case .comments:
      guard site.comments && work.allowsComments else { throw SiteReadError.malformed }
      method = .comments
      if let address = work.commentURL { options["context"] = ["url": address] }
    case .related:
      guard site.related else { throw SiteReadError.malformed }
      method = .related
    }
    let arguments = try JSONSerialization.data(withJSONObject: [work.id, options] as [Any])
    let raw = try await siteRead(library, site: site.key, method: method,
      arguments: String(decoding: arguments, as: UTF8.self), confirmation: confirmation)
    struct Page: Decodable {
      let hasNext: Bool?
      let page: Int?
      let cursor: PluginOptionValue?
      let reset: Bool?
    }
    let data = Data(raw.utf8)
    let info = try JSONDecoder().decode(Page.self, from: data)
    let entries: [DetailEntry]
    switch section {
    case .comments:
      struct Comments: Decodable { let items: [SiteComment] }
      let rows = try JSONDecoder().decode(Comments.self, from: data).items
      guard rows.allSatisfy({ !$0.id.isEmpty }) else { throw SiteReadError.malformed }
      entries = rows.map(DetailEntry.comment)
    case .related:
      let list = try await decodeSitePage(raw, source: site.key, cursor: nil)
      entries = list.items.map(DetailEntry.work)
    }
    let current = info.page ?? page
    guard current > 0 && current < Int.max else { throw SiteReadError.malformed }
    let more = info.hasNext == true && (info.cursor == nil || info.cursor != cursor)
    return DetailEntryPage(entries: entries, cursor: info.cursor, nextPage: more ? current + 1 : nil, reset: info.reset == true)
  }
}
