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
  public let spoiler: Bool?
  public let repliesAvailable: Bool?
  public let replyCount: Int?
  public let replies: [SiteComment]?
}

public struct SiteCommentOrder: Decodable, Identifiable, Sendable {
  public let value: String
  public let label: String
  public var id: String { value }
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
  public let orders: [SiteCommentOrder]
}

extension ProgressSession {
  public func detailEntries(_ library: SourceLibrary, site: InstalledSite, work: SiteWork,
    section: DetailSectionKind, page: Int, cursor: PluginOptionValue?, confirmation: ManualRead?,
    sort: String = "default", parent: String? = nil
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
      options["sort"] = sort
      if let parent { options["parentId"] = parent }
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
      let sorts: [SiteCommentOrder]?
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
    return DetailEntryPage(entries: entries, cursor: info.cursor, nextPage: more ? current + 1 : nil,
      reset: info.reset == true, orders: info.sorts ?? [])
  }
}

/// 同一作品的初始预览、远端页号与自动加载开关。重复页保留手动继续入口。
public struct PreviewPager: Sendable {
  private enum Identity: Hashable {
    case index(Int)
    case sprite(String, Double?, Double?)
  }

  public private(set) var items: [SiteThumbnail]
  public private(set) var hasNext: Bool
  public private(set) var automatic = false
  public private(set) var nextRemotePage = 1
  private let local: [SiteThumbnail]
  private let remote: Bool
  private let total: Int?

  public init(local: [SiteThumbnail] = [], remote: Bool = false, total: Int? = nil) {
    self.local = local
    self.remote = remote
    self.total = total
    let initial = Array(local.prefix(20))
    items = initial
    hasNext = local.count > initial.count || (remote && (total.map { $0 > initial.count } ?? true))
  }

  public mutating func startManually() { automatic = true }
  public mutating func stopAutomatic() { automatic = false }

  public mutating func appendLocal() -> Bool {
    guard items.count < local.count else { return false }
    items = Array(local.prefix(items.count + 40))
    hasNext = items.count < local.count || (remote && (total.map { $0 > items.count } ?? true))
    return true
  }

  public mutating func append(_ page: SiteThumbnailPage) {
    func identity(_ item: SiteThumbnail) -> Identity {
      if let index = item.index { return .index(index) }
      return .sprite(item.url, item.offsetX, item.offsetY)
    }
    var seen = Set(items.map(identity))
    let additions = page.thumbnails.filter { seen.insert(identity($0)).inserted }
    if additions.isEmpty { automatic = false }
    items.append(contentsOf: additions)
    nextRemotePage += 1
    hasNext = page.hasNext == true
  }
}
