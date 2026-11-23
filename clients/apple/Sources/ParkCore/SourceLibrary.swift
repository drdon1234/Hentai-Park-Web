import CoreFoundation
import Foundation

/// 同一应用的窗口共用站点服务。资源文件与脚本均来自当前安装包。
public final class SourceLibrary: BundledScripts, @unchecked Sendable {
  let resources: URL
  private let cache: URL
  private let lock = NSLock()
  private var active: (directory: URL, proxy: String, service: SourceService)?

  public init(resources: URL, cache: URL) {
    self.resources = resources
    self.cache = cache
  }

  public func asset(name: String) -> Data? {
    guard name.hasSuffix(".js"), URL(fileURLWithPath: name).lastPathComponent == name else {
      return nil
    }
    return try? Data(contentsOf: resources.appendingPathComponent(name))
  }

  func bundledSites() throws -> [(String, String)] {
    struct Entry: Decodable { let key: String; let name: String }
    struct Index: Decodable { let sources: [Entry] }
    let data = try Data(contentsOf: resources.appendingPathComponent("index.json"))
    return try JSONDecoder().decode(Index.self, from: data).sources.map { ($0.key, $0.name) }
  }

  func service(directory: URL, request: Request) throws -> SourceService {
    lock.lock()
    defer { lock.unlock() }
    struct Network: Decodable { let proxy: String? }
    let setting = directory.appendingPathComponent("park/settings.json")
    let proxy: String
    do {
      proxy = try JSONDecoder().decode(Network.self, from: Data(contentsOf: setting)).proxy ?? ""
    } catch let error as CocoaError where error.code == .fileReadNoSuchFile { proxy = "" }
    if let active, active.directory == directory, active.proxy == proxy { return active.service }
    try FileManager.default.createDirectory(at: cache, withIntermediateDirectories: true)
    let script = try String(
      contentsOf: resources.appendingPathComponent("park_polyfills.js"), encoding: .utf8)
    let index = try String(
      contentsOf: resources.appendingPathComponent("index.json"), encoding: .utf8)
    let service = try SourceService(
      support: directory.path, cacheDirectory: cache.path, polyfills: script, proxy: proxy,
      systemRoots: [])
    try service.loadBundledPlugins(index: index, assets: self, request: request)
    active = (directory, proxy, service)
    return service
  }
}

public struct InstalledSite: Identifiable, Sendable {
  public struct Feed: Sendable {
    public let key: String
    fileprivate let defaults: Data
  }
  public let key: String
  public let name: String?
  public let category: String?
  public let mediaType: String?
  public let entryFeed: Feed?
  public let search: Bool?
  public let detail: Bool
  public let comments: Bool
  public let related: Bool
  public let thumbnails: Bool
  public let random: Bool
  fileprivate let resolveLink: Bool
  public let subscriptionUpdates: Bool
  public let nativeVideo: Bool
  public let unavailable: Bool?
  let rankings: Bool
  public let metadataSearch: Bool
  public let metadataDetail: Bool
  let rankingOnly: Bool?
  let metadata: Bool?
  private let rankingCategories: [String]
  public var isBrowsableSite: Bool {
    rankingOnly != true && metadata != true && reverseOnly != true
      && (entryFeed != nil || search == true || unavailable == true)
  }
  public func offersReference(in domain: String) -> Bool {
    (metadata == true || (rankingOnly == true && metadataDetail))
      && (category == domain || rankingCategories.contains(domain))
  }
  let reverseOnly: Bool?
  public var id: String { key }

  fileprivate init(_ site: [String: Any]) throws {
    guard let id = site["key"] as? String, !id.isEmpty else { throw SiteReadError.malformed }
    key = id
    name = site["name"] as? String
    category = site["category"] as? String
    mediaType = site["type"] as? String
    search = site["search"] as? Bool
    detail = site["detail"] as? Bool == true
    comments = site["comments"] as? Bool == true
    related = site["related"] as? Bool == true
    thumbnails = site["thumbnails"] as? Bool == true
    random = site["random"] as? Bool == true
    resolveLink = site["resolveLink"] as? Bool == true
    subscriptionUpdates = site["subscriptionUpdates"] as? Bool == true
    nativeVideo = site["nativeVideo"] as? Bool == true
    unavailable = site["unavailable"] as? Bool
    rankings = site["rankings"] as? Bool == true
    metadataSearch = site["metadataSearch"] as? Bool == true
    metadataDetail = site["metadataDetail"] as? Bool == true
    rankingOnly = site["rankingOnly"] as? Bool
    metadata = site["metadata"] as? Bool
    rankingCategories = site["rankingCategories"] as? [String] ?? []
    reverseOnly = site["reverseOnly"] as? Bool
    if let feed = site["entryFeed"] as? [String: Any], let key = feed["key"] as? String {
      var defaults: [String: Any] = [:]
      for option in feed["options"] as? [[String: Any]] ?? [] {
        if let key = option["key"] as? String { defaults[key] = option["default"] }
      }
      entryFeed = Feed(key: key, defaults: try JSONSerialization.data(withJSONObject: defaults))
    } else {
      entryFeed = nil
    }
  }
}

public struct DetailSearchTag: Hashable, Sendable, Decodable {
  public let name: String
  public var namespace = ""
  public let keyword: String
  public let filters: [String: PluginOptionValue]
  private enum Keys: String, CodingKey { case name, keyword, filters }

  public init(name: String, keyword: String? = nil, filters: [String: PluginOptionValue] = [:]) {
    self.name = name
    self.keyword = keyword ?? name
    self.filters = filters
  }

  public init(from decoder: Decoder) throws {
    if let text = try? decoder.singleValueContainer().decode(String.self) {
      self.init(name: text)
    } else {
      let values = try decoder.container(keyedBy: Keys.self)
      self.init(name: try values.decode(String.self, forKey: .name),
        keyword: try values.decodeIfPresent(String.self, forKey: .keyword),
        filters: try values.decodeIfPresent([String: PluginOptionValue].self, forKey: .filters) ?? [:])
    }
  }
}

extension ProgressSession {
  public func tagDictionary(source: String, locale: String) async throws -> SearchDictionary? {
    return try await run { store, _ in
      guard let bytes = try store.installedDictionary(source: source, locale: locale) else { return nil }
      return try SearchDictionary(bytes: bytes, source: source, locale: locale)
    }
  }

  public func translateTag(_ name: String, locale: String, library: SourceLibrary) async throws -> String {
    let root = directory
    let target = ["zh-Hans":"zh-CN","zh-Hant":"zh-TW"][locale] ?? locale
    return try await run { _, request in
      try library.service(directory: root, request: request).translateText(text: name, target: target, request: request)
    }
  }

  public func blockTag(_ value: String) async throws {
    try await run { store, _ in _ = try store.addBlockedTag(tag: value) }
  }

  public func subscribeToTag(_ tag: DetailSearchTag, source: String, library: SourceLibrary) async throws {
    struct Filter: Encodable { let keyword: String; let filters: [String: PluginOptionValue] }
    let encoder = JSONEncoder()
    encoder.outputFormatting = .sortedKeys
    let input = try encoder.encode(Filter(keyword: tag.keyword, filters: tag.filters))
    _ = try await saveSubscription(library, original: nil, source: source, dimension: "filter",
      input: String(decoding: input, as: UTF8.self), label: tag.name)
  }
}

private struct SiteWorkTag: Decodable, Sendable {
  let namespace: String
  let entries: [DetailSearchTag]
  private enum Keys: String, CodingKey { case namespace, tags }
  init(from decoder: Decoder) throws {
    if let flat = try? decoder.singleValueContainer().decode(String.self) {
      namespace = ""
      entries = [DetailSearchTag(name: flat)]
    } else {
      let group = try decoder.container(keyedBy: Keys.self)
      namespace = try group.decodeIfPresent(String.self, forKey: .namespace) ?? ""
      entries = try group.decode([DetailSearchTag].self, forKey: .tags)
    }
  }
}

public enum SiteArticleBlock: Decodable, Sendable {
  case text(String)
  case image(String)
  case video(id: String, preview: String?)

  private enum Keys: String, CodingKey { case type, text, url, id, preview }

  public init(from decoder: Decoder) throws {
    let block = try decoder.container(keyedBy: Keys.self)
    switch try block.decode(String.self, forKey: .type) {
    case "text": self = .text(try block.decode(String.self, forKey: .text))
    case "image": self = .image(try block.decode(String.self, forKey: .url))
    case "video":
      let id = try block.decode(String.self, forKey: .id)
      guard !id.isEmpty else { throw SiteReadError.malformed }
      self = .video(id: id, preview: try block.decodeIfPresent(String.self, forKey: .preview))
    default: throw SiteReadError.malformed
    }
  }

  public var isMedia: Bool {
    if case .text = self { return false }
    return true
  }
}

public struct AuthorSearch: Decodable, Sendable {
  public let keyword: String?
  public let filters: [String: PluginOptionValue]?
  public let url: String?
}

public enum SiteLinkTarget: Sendable {
  case work(InstalledSite, String)
  case search(InstalledSite, URL)
  case external(URL)
}

public struct SiteThumbnail: Decodable, Hashable, Sendable {
  public let url: String
  public let index: Int?
  public let offsetX, offsetY, width, height: Double?

  private enum Keys: String, CodingKey { case url, index, offsetX, offsetY, width, height }
  public init(from decoder: Decoder) throws {
    if let address = try? decoder.singleValueContainer().decode(String.self) {
      url = address
      index = nil
      offsetX = nil; offsetY = nil; width = nil; height = nil
    } else {
      let value = try decoder.container(keyedBy: Keys.self)
      url = try value.decode(String.self, forKey: .url)
      index = try value.decodeIfPresent(Int.self, forKey: .index)
      offsetX = try value.decodeIfPresent(Double.self, forKey: .offsetX)
      offsetY = try value.decodeIfPresent(Double.self, forKey: .offsetY)
      width = try value.decodeIfPresent(Double.self, forKey: .width)
      height = try value.decodeIfPresent(Double.self, forKey: .height)
    }
  }
}

public struct SiteThumbnailPage: Decodable, Sendable {
  public let thumbnails: [SiteThumbnail]
  public let hasNext: Bool?
}

public struct SiteWork: Decodable, Identifiable, Sendable {
  public struct Chapter: Decodable, Identifiable, Sendable {
    public let id: String
    public let title: String?
  }
  public let id: String
  public let title: String
  public let subtitle: String?
  public let description: String?
  public fileprivate(set) var allowsComments = true
  public fileprivate(set) var recommendations: [SiteWork] = []
  public fileprivate(set) var commentURL: String?
  public let cover: String?
  private let tagGroups: [SiteWorkTag]?
  public var tags: [String]? {
    tagGroups?.flatMap { group in group.entries.map { group.namespace.isEmpty ? $0.name : group.namespace + ":" + $0.name } }
  }
  public var searchTags: [DetailSearchTag] {
    tagGroups?.flatMap { group in group.entries.map { entry in
      var tag = entry
      tag.namespace = group.namespace
      return tag
    } } ?? []
  }
  public let authors: [String]?
  public let authorSearch: AuthorSearch?
  public let workCode: String?
  public let editions: [String]?
  public fileprivate(set) var variants: [SiteWork] = []
  public let chapters: [Chapter]?
  public let chaptersComplete: Bool?
  public let thumbnails: [SiteThumbnail]?
  public let thumbnailsTotal: Int?
  public let singleChapterId: String?
  public let voiceRoot: VoiceNode?
  let voiceMediaForm: String?
  let voiceTrackKinds: [String: String]?
  let voiceSubtitles: [String: [String]]?
  fileprivate var listingSource: String?
  fileprivate var listingData: Data?
  fileprivate var summary: Data?
  fileprivate var bodyData: Data?
  fileprivate var subscriptionPost = false

  func listingField(_ name: String) -> Any? {
    guard let listingData, let row = try? JSONSerialization.jsonObject(with: listingData) as? [String: Any] else { return nil }
    return row[name]
  }

  public func articleBlocks() throws -> [SiteArticleBlock] {
    guard let bodyData else { throw SiteReadError.malformed }
    let blocks = try JSONDecoder().decode([SiteArticleBlock].self, from: bodyData)
    guard !blocks.isEmpty else { throw SiteReadError.malformed }
    return blocks
  }
  private enum CodingKeys: String, CodingKey {
    case id, title, subtitle, description, cover, chapters, chaptersComplete, singleChapterId
    case tagGroups = "tags"
    case workCode, editions, authors, authorSearch
    case thumbnails, thumbnailsTotal
    case voiceRoot, voiceMediaForm, voiceTrackKinds, voiceSubtitles
  }
}

public struct SiteWorkPage: Sendable {
  public let items: [SiteWork]
  public let hasNext: Bool
  public let cursor: String?
  public let receivedCount: Int
}

public enum SiteReadError: Error {
  case confirmation(ManualRead)
  case malformed
}

extension OnlineGame {
  /// v0.15.4 的 WebKit Profile 使用 SHA-256 的前 128 位，不设置 UUID 版本位。
  public var websiteDataIdentifier: UUID? {
    let name = profileName()
    let prefix = "park_game_"
    guard name.hasPrefix(prefix) else { return nil }
    let digest = name.dropFirst(prefix.count)
    guard digest.count == 64, digest.allSatisfy({ "0123456789abcdef".contains($0) }) else {
      return nil
    }
    let chars = Array(digest.prefix(32))
    let groups = [0..<8, 8..<12, 12..<16, 16..<20, 20..<32]
    return UUID(uuidString: groups.map { String(chars[$0]) }.joined(separator: "-"))
  }
}

extension ProgressSession {
  public func htmlGame(_ library: SourceLibrary, source: String, work: String) async throws
    -> OnlineGame
  {
    let root = directory
    return try await run { _, request in
      let game = try library.service(directory: root, request: request).onlineGame(
        source: source, work: work, request: request)
      guard game.player() == nil else { throw SourceFailure.Unsupported }
      return game
    }
  }

  public func videoChoices(_ library: SourceLibrary, key: ChapterKey) async throws -> VideoLines {
    let root = directory
    return try await run { _, request in
      try library.service(directory: root, request: request).videoLines(
        source: key.source, work: key.workId, chapter: key.chapterId, request: request)
    }
  }

  public func stream(_ choices: VideoLines, index: Int32) async throws -> PlaybackSession {
    try await run { _, request in try choices.play(index: index, request: request) }
  }

  public func comicPages(_ library: SourceLibrary, key: ChapterKey) async throws -> ChapterImages {
    let root = directory
    return try await run { store, request in
      let cache = try FileManager.default.url(for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
      do { return try store.chapterImages(key: key, cacheDirectory: cache.path) }
      catch StorageError.UnavailableDownload {
        return try library.service(directory: root, request: request).comicChapter(
          store: store, key: key, request: request)
      }
    }
  }

  public func siteThumbnails(_ library: SourceLibrary, site: String, work: String, page: Int,
    confirmation: ManualRead? = nil) async throws -> SiteThumbnailPage {
    let arguments = try JSONSerialization.data(withJSONObject: [work, page])
    let raw = try await siteRead(library, site: site, method: .thumbnails,
      arguments: String(decoding: arguments, as: UTF8.self), confirmation: confirmation)
    return try JSONDecoder().decode(SiteThumbnailPage.self, from: Data(raw.utf8))
  }

  public func sourceNovel(_ library: SourceLibrary, key: ChapterKey, locale: String, engine: String)
    async throws -> NovelDocument
  {
    let root = directory
    return try await run { store, request in
      try library.service(directory: root, request: request).novelChapter(
        store: store, key: key, locale: locale, engine: engine, request: request)
    }
  }

  public func openSiteWork(
    _ library: SourceLibrary, site: InstalledSite, item: SiteWork, incognito: Bool,
    favorite: Bool? = nil, categories: [Int64]? = nil
  ) async throws -> LibraryWork {
    let root = directory
    let work = try await run { store, request in
      let service = try library.service(directory: root, request: request)
      let raw = try service.sourceManifest(source: site.key, request: request)
      guard let manifest = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any],
        let siteMedia = manifest["type"] as? String, let domain = manifest["category"] as? String,
        let capabilities = manifest["capabilities"] as? [String: Any]
      else { throw SiteReadError.malformed }
      let media = item.subscriptionPost ? "article" : siteMedia
      let readable =
        (media == "novel" && capabilities["textChapter"] as? Bool == true)
        || (media == "comic" && capabilities["chapter"] as? Bool == true)
        || (media == "video" && domain != "live"
          && (capabilities["videos"] as? Bool == true || capabilities["webVideo"] as? Bool == true))
      var chapters: [OfflineChapter] = []
      if readable {
        chapters = (item.chapters ?? []).filter { !$0.id.isEmpty }.map {
          OfflineChapter(id: $0.id, title: $0.title ?? item.title)
        }
        if chapters.isEmpty && (item.chapters == nil || media == "video") {
          chapters = [
            OfflineChapter(
              id: item.singleChapterId.flatMap { $0.isEmpty ? nil : $0 } ?? "_", title: item.title)
          ]
        }
      }
      let starred: Bool
      if incognito && favorite == nil {
        starred = try store.isFavorite(source: site.key, work: item.id)
      } else {
        var summary =
          try item.summary.flatMap { try JSONSerialization.jsonObject(with: $0) as? [String: Any] }
          ?? [:]
        summary["id"] = item.id
        summary["title"] = item.title
        if summary["mediaKind"] == nil { summary["mediaKind"] = media }
        if summary["contentDomain"] == nil { summary["contentDomain"] = domain }
        let json = String(
          decoding: try JSONSerialization.data(withJSONObject: summary), as: UTF8.self)
        if let categories, favorite == true {
          starred = try store.favoriteWithCategories(source: site.key, summary: json, categories: categories)
        } else {
          starred = try store.saveWorkSummary(source: site.key, summary: json, favorite: favorite)
        }
      }
      return LibraryWork(
        downloaded: false, invalidDownload: false, pendingDeletion: nil, downloadStatus: nil,
        readerMode: nil,
        media: media, source: site.key, workId: item.id, title: item.title, categoryNames: [],
        favorite: starred,
        historyMs: nil, historyChapter: nil, historyPage: 0, chapters: chapters,
        contentDomain: domain)
    }
    if favorite != nil { await Self.favoritesChanged([work.libraryID]) }
    return work
  }
  public func lastReadChapter(source: String, work: String) async throws -> String? {
    try await run { store, _ in try store.lastReadChapter(source: source, work: work) }
  }

  public func migrateFavorite(from work: LibraryWork, to site: InstalledSite,
                             item: SiteWork, categories: Bool, progress: Bool, remove: Bool) async throws -> Bool {
    guard site.key != work.source, site.mediaType == work.media, let domain = site.category,
          let data = item.summary, !item.title.isEmpty else { throw SiteReadError.malformed }
    var summary = try JSONSerialization.jsonObject(with: data) as? [String: Any] ?? [:]
    summary["id"] = item.id
    summary["title"] = item.title
    summary["mediaKind"] = work.media
    summary["contentDomain"] = domain
    summary["chapters"] = item.chapters?.map { ["id": $0.id, "title": $0.title ?? $0.id] }
    summary["chaptersComplete"] = item.chaptersComplete
    let json = String(decoding: try JSONSerialization.data(withJSONObject: summary), as: UTF8.self)
    let removed = try await run { store, _ in
      try store.migrateFavorite(fromSource: work.source, fromId: work.workId, toSource: site.key,
        detailJson: json, categories: categories, progress: progress, remove: remove)
    }
    await Self.favoritesChanged([work.libraryID, LibraryWorkID(source: site.key, work: item.id)])
    return removed
  }
  public func sites(_ library: SourceLibrary) async throws -> [InstalledSite] {
    try await installedSites(library).filter(\.isBrowsableSite)
  }

  public struct ManagedSite: Identifiable, Sendable {
    public let id: String
    public let name: String
    public let enabled: Bool
  }

  public func managedSites(_ library: SourceLibrary) async throws -> [ManagedSite] {
    let root = directory
    return try await run { _, request in
      struct Row: Decodable { let key: String; let order: Int; let enabled: Bool }
      struct Registry: Decodable { let sources: [Row] }
      let service = try library.service(directory: root, request: request)
      let json = try service.pluginRegistry(request: request)
      let rows = try JSONDecoder().decode(Registry.self, from: Data(json.utf8)).sources
      var names = Dictionary(uniqueKeysWithValues: try library.bundledSites())
      let available = try service.discoverySources(request: request)
      if let items = try JSONSerialization.jsonObject(with: Data(available.utf8)) as? [[String: Any]] {
        for item in items {
          if let key = item["key"] as? String, let name = item["name"] as? String { names[key] = name }
        }
      }
      return rows.sorted { $0.order < $1.order }.map {
        ManagedSite(id: $0.key, name: names[$0.key] ?? $0.key, enabled: $0.enabled)
      }
    }
  }

  public func moveSite(_ library: SourceLibrary, key: String, anchor: String, after: Bool) async throws {
    let root = directory
    try await run { _, request in
      try library.service(directory: root, request: request).movePlugin(key: key, anchor: anchor, after: after, request: request)
    }
  }

  #if os(macOS) || os(iOS)
  public func clearCookies(_ library: SourceLibrary, source: String) async throws {
    let root = directory
    try await run { _, request in
      let service = try library.service(directory: root, request: request)
      try service.clearSiteCookies(key: source, browser: SiteCookieRecovery(), request: request)
    }
    try await SiteBrowserCookies.clear(source)
  }
  #endif

  public func restoreSiteOrder(_ library: SourceLibrary) async throws {
    let root = directory
    try await run { _, request in
      let keys = try library.bundledSites().map { $0.0 }
      try library.service(directory: root, request: request).resetPluginOrder(bundled: keys, request: request)
    }
  }

  public func installedSites(_ library: SourceLibrary) async throws -> [InstalledSite] {
    let root = directory
    return try await run { _, request in
      let service = try library.service(directory: root, request: request)
      let value = try service.discoverySources(request: request)
      guard let rows = try JSONSerialization.jsonObject(with: Data(value.utf8)) as? [[String: Any]]
      else { throw SiteReadError.malformed }
      return try rows.map(InstalledSite.init)
    }
  }

  func siteRead(
    _ library: SourceLibrary, site: String, method: BrowseMethod, arguments: String,
    confirmation: ManualRead?
  ) async throws -> String {
    let root = directory
    return try await run { _, request in
      if let confirmation { return try confirmation.readAgain(request: request) }
      let service = try library.service(directory: root, request: request)
      let response = try service.browse(
        source: site, method: method, arguments: arguments, request: request)
      if let content = response.content { return content }
      if let page = response.confirmation { throw SiteReadError.confirmation(page) }
      throw SiteReadError.malformed
    }
  }

  public func contentLink(_ library: SourceLibrary, source: InstalledSite, address: String,
    confirmation: ManualRead? = nil) async throws -> SiteLinkTarget {
    func webURL(_ value: String) -> URL? {
      guard let url = URL(string: value), ["http", "https"].contains(url.scheme?.lowercased() ?? ""),
            url.host != nil, url.user == nil, url.password == nil else { return nil }
      return url
    }
    guard let url = webURL(address) else { throw SiteReadError.malformed }
    let sites = try await installedSites(library)
    let root = directory
    for site in [source] + sites.filter({ $0.key != source.key }) {
      let data = try await run { _, request in
        try library.service(directory: root, request: request).sourceManifest(source: site.key, request: request)
      }
      guard let manifest = try JSONSerialization.jsonObject(with: Data(data.utf8)) as? [String: Any],
            let domains = manifest["domains"] as? [String] else { continue }
      func owns(_ link: URL) -> Bool {
        guard let host = link.host?.lowercased(), link.scheme?.lowercased() == "https", (link.port ?? 443) == 443 else { return false }
        return domains.contains { domain in
          let name = domain.lowercased()
          return !name.isEmpty && (host == name || host.hasSuffix("." + name))
        }
      }
      guard owns(url) else { continue }
      let capabilities = manifest["capabilities"] as? [String: Any]
      if capabilities?["resolveContentLink"] as? Bool == true {
        let args = String(decoding: try JSONEncoder().encode([address]), as: UTF8.self)
        let data = try await siteRead(library, site: site.key, method: .resolveContentLink, arguments: args, confirmation: confirmation)
        guard let row = try JSONSerialization.jsonObject(with: Data(data.utf8), options: .fragmentsAllowed) as? [String: Any] else { continue }
        switch row["type"] as? String {
        case "work":
          if let id = row["id"] as? String, !id.isEmpty, id.utf8.count <= 8192 { return .work(site, id) }
        case "search", "external":
          if let value = row["url"] as? String, let result = webURL(value), owns(result) {
            if row["type"] as? String == "external" { return .external(result) }
            if capabilities?["searchLink"] as? Bool == true { return .search(site, result) }
          }
        default: break
        }
      } else if site.detail {
        for pattern in manifest["linkPatterns"] as? [String] ?? [] {
          guard let regex = try? NSRegularExpression(pattern: pattern),
                let hit = regex.firstMatch(in: address, range: NSRange(address.startIndex..., in: address)),
                let range = Range(hit.range(at: hit.numberOfRanges > 1 ? 1 : 0), in: address), !range.isEmpty else { continue }
          return .work(site, String(address[range]))
        }
      }
    }
    return .external(url)
  }

  private func checkedReference(_ value: String) throws -> String {
    guard !value.isEmpty, value.count <= 4096, !value.unicodeScalars.contains(where: { $0.value < 32 })
    else { throw SiteReadError.malformed }
    return value
  }

  public func randomReference(_ library: SourceLibrary, site: InstalledSite,
    confirmation: ManualRead? = nil) async throws -> String {
    guard site.random else { throw SiteReadError.malformed }
    let raw = try await siteRead(library, site: site.key, method: .random, arguments: "[{}]", confirmation: confirmation)
    let data = Data(raw.utf8)
    let id: String
    if let text = try? JSONDecoder().decode(String.self, from: data) { id = text }
    else { id = String(try JSONDecoder().decode(Int64.self, from: data)) }
    return try checkedReference(id)
  }

  public func resolveWorkReference(_ library: SourceLibrary, site: InstalledSite, reference: String,
    confirmation: ManualRead? = nil) async throws -> String {
    guard site.resolveLink else { return reference }
    let args = String(decoding: try JSONEncoder().encode([reference]), as: UTF8.self)
    let target = try await siteRead(library, site: site.key, method: .resolveLink, arguments: args, confirmation: confirmation)
    return try checkedReference(JSONDecoder().decode(String.self, from: Data(target.utf8)))
  }

  public func sitePage(
    _ library: SourceLibrary, site: InstalledSite, keyword: String, page: Int, cursor: String?,
    feedKey: String? = nil, values: [String: PluginOptionValue]? = nil, filterSearch: Bool = false, link: URL? = nil, referenceDomain: String? = nil,
    common: [String: [String: PluginOptionValue]] = [:], confirmation: ManualRead? = nil
  ) async throws -> SiteWorkPage {
    var request: [String: Any] = ["page": page]
    if let cursor { request["cursor"] = cursor }
    request["common"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(common))
    let options = try values.map { try JSONSerialization.jsonObject(with: JSONEncoder().encode($0)) }
    let args: [Any]
    let method: BrowseMethod
    if let referenceDomain, !keyword.isEmpty, site.search != true, site.metadataSearch {
      let arguments = try JSONSerialization.data(withJSONObject: [keyword, ["category": referenceDomain]])
      let raw = try await siteRead(library, site: site.key, method: .metadataSearch,
                                   arguments: String(decoding: arguments, as: UTF8.self), confirmation: confirmation)
      guard let items = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [[String: Any]] else {
        throw SiteReadError.malformed
      }
      let result = try JSONSerialization.data(withJSONObject: ["items": items, "hasNext": false] as [String: Any])
      return try await decodeSitePage(String(decoding: result, as: UTF8.self), source: site.key, cursor: nil)
    } else if let link {
      args = [link.absoluteString, request]
      method = .searchLink
    } else if !keyword.isEmpty || filterSearch, site.search == true {
      request["keyword"] = keyword
      request["filters"] = options
      args = [request]
      method = .search
    } else if let key = feedKey ?? site.entryFeed?.key {
      if let options { request["options"] = options }
      else if let feed = site.entryFeed, feed.key == key {
        request["options"] = try JSONSerialization.jsonObject(with: feed.defaults)
      }
      args = [key, request]
      method = .feed
    } else {
      throw SiteReadError.malformed
    }
    let arguments = String(
      decoding: try JSONSerialization.data(withJSONObject: args), as: UTF8.self)
    let raw = try await siteRead(
      library, site: site.key, method: method, arguments: arguments, confirmation: confirmation)
    return try await decodeSitePage(raw, source: site.key, cursor: cursor)
  }

  func decodeSitePage(_ raw: String, source sourceKey: String, cursor: String?) async throws -> SiteWorkPage {
    struct Cursor: Decodable {
      let value: String
      init(from decoder: Decoder) throws {
        let field = try decoder.singleValueContainer()
        if let text = try? field.decode(String.self) {
          value = text
        } else if let integer = try? field.decode(Int64.self) {
          value = String(integer)
        } else if let unsigned = try? field.decode(UInt64.self) {
          value = String(unsigned)
        } else {
          value = String(try field.decode(Double.self))
        }
      }
    }
    struct Response: Decodable {
      let hasNext: Bool?
      let cursor: Cursor?
    }
    let bytes = Data(raw.utf8)
    let result = try JSONDecoder().decode(Response.self, from: bytes)
    guard let listing = try JSONSerialization.jsonObject(with: bytes) as? [String: Any] else {
      throw SiteReadError.malformed
    }
    let rows = listing["items"] as? [[String: Any]]
    let sections = listing["sections"] as? [[String: Any]]
    guard rows != nil || sections != nil else { throw SiteReadError.malformed }
    let source = try ((rows ?? []) + (sections ?? []).flatMap { section -> [[String: Any]] in
      guard let items = section["items"] as? [[String: Any]] else { throw SiteReadError.malformed }
      return items
    }).map { fields -> SiteWork in
      let bytes = try JSONSerialization.data(withJSONObject: fields)
      var work = try JSONDecoder().decode(SiteWork.self, from: bytes)
      work.listingSource = sourceKey
      work.listingData = bytes
      return work
    }
    let file = directory.appendingPathComponent("park/settings.json")
    let items = try await run { _, _ in
      let settings: String
      do { settings = try String(contentsOf: file, encoding: .utf8) } catch let error as CocoaError
        where error.code == .fileReadNoSuchFile
      { settings = "{}" }
      let filter = try ContentFilter(settingsJson: settings)
      return source.filter { filter.firstMatch(title: $0.title, tags: $0.tags ?? []) == nil }
    }
    return SiteWorkPage(
      items: items,
      hasNext: result.hasNext == true && (result.cursor == nil || result.cursor?.value != cursor),
      cursor: result.cursor?.value, receivedCount: source.count)
  }

  public func siteDetail(
    _ library: SourceLibrary, site: String, id: String, preview: SiteWork? = nil,
    confirmation: ManualRead? = nil
  ) async throws -> SiteWork {
    let seed: Data?
    if let preview {
      guard preview.id == id, preview.listingSource == site else { throw SiteReadError.malformed }
      seed = preview.listingData
    } else {
      seed = try await run { store, _ in try store.storedWork(source: site, work: id).map { Data($0.utf8) } }
    }
    var arguments: [Any] = [id]
    var prior: [String: Any] = [:]
    if let seed, let object = try JSONSerialization.jsonObject(with: seed) as? [String: Any],
      object["id"] as? String == id {
      prior = object
      arguments.append(object)
    }
    let args = String(decoding: try JSONSerialization.data(withJSONObject: arguments), as: UTF8.self)
    let raw = try await siteRead(
      library, site: site, method: .detail, arguments: args, confirmation: confirmation)
    let bytes = Data(raw.utf8)
    guard var fields = try JSONSerialization.jsonObject(with: bytes) as? [String: Any],
      fields["id"] as? String == id else { throw SiteReadError.malformed }
    let keys: Set<String> = [
      "cover", "coverImages", "coverAspect", "coverBlurred", "uploaded", "tags", "authors",
      "subtitle", "mediaKind", "contentDomain", "singleChapterId", "voiceNodeType", "workCode",
      "editions", "tavernKind",
    ]
    for key in keys where fields[key] == nil || fields[key] is NSNull || fields[key] as? String == "" {
      fields[key] = prior[key]
    }
    return try await decodeSiteDetail(fields, site: site, summaryKeys: keys)
  }

  private func decodeSiteDetail(_ fields: [String: Any], site: String, summaryKeys: Set<String>) async throws -> SiteWork {
    var work = try JSONDecoder().decode(SiteWork.self, from: JSONSerialization.data(withJSONObject: fields))
    guard !work.title.isEmpty else { throw SiteReadError.malformed }
    if let flag = fields["commentsAvailable"] as? NSNumber, CFGetTypeID(flag) == CFBooleanGetTypeID() {
      work.allowsComments = flag.boolValue
    }
    work.commentURL = fields["url"] as? String
    if let variants = fields["variants"] as? [[String: Any]], !variants.isEmpty {
      let page = try JSONSerialization.data(withJSONObject: ["items": variants])
      work.variants = try await decodeSitePage(String(decoding: page, as: UTF8.self), source: site, cursor: nil).items
    }
    if let related = fields["related"] as? [[String: Any]], !related.isEmpty {
      let page = try JSONSerialization.data(withJSONObject: ["items": related])
      work.recommendations = try await decodeSitePage(String(decoding: page, as: UTF8.self), source: site, cursor: nil).items
    }
    if let blocks = fields["blocks"] as? [Any] {
      work.bodyData = try JSONSerialization.data(withJSONObject: blocks)
    }
    work.summary = try JSONSerialization.data(
      withJSONObject: fields.filter { summaryKeys.contains($0.key) && !($0.value is NSNull) })
    return work
  }

  public func subscriptionPostPage(_ library: SourceLibrary, source: String, target: String, id: String,
    cursor: String?, confirmation: ManualRead?) async throws -> (work: SiteWork?, next: String?) {
    let page: [String: Any] = ["cursor": cursor.map { $0 as Any } ?? NSNull()]
    let arguments = try JSONSerialization.data(withJSONObject: [target, page] as [Any])
    let response = try await siteRead(library, site: source, method: .subscriptionPosts,
      arguments: String(decoding: arguments, as: UTF8.self), confirmation: confirmation)
    guard let batch = try JSONSerialization.jsonObject(with: Data(response.utf8)) as? [String: Any],
      let items = batch["items"] as? [[String: Any]], let hasNext = batch["hasNext"] as? Bool
    else { throw SiteReadError.malformed }
    if var post = items.first(where: { $0["id"] as? String == id }) {
      let text = post["text"] as? String ?? ""
      if (post["title"] as? String)?.isEmpty != false {
        post["title"] = text.split(separator: "\n").first.map { String($0.prefix(120)) } ?? id
      }
      var blocks: [[String: Any]] = text.isEmpty ? [] : [["type": "text", "text": text]]
      blocks.append(contentsOf: post["media"] as? [[String: Any]] ?? [])
      post["blocks"] = blocks
      post["mediaKind"] = "article"
      var work = try await decodeSiteDetail(post, site: source, summaryKeys: ["mediaKind", "cover"])
      work.subscriptionPost = true
      return (work, nil)
    }
    guard hasNext else { return (nil, nil) }
    guard let next = batch["cursor"] as? String, !next.isEmpty else { throw SiteReadError.malformed }
    return (nil, next)
  }

  public func siteTavernResource(_ library: SourceLibrary, site: String, item: SiteWork)
    async throws -> SourceTavernResource?
  {
    let root = directory
    return try await run { store, request in
      let service = try library.service(directory: root, request: request)
      let manifest = try service.sourceManifest(source: site, request: request)
      guard let fields = try JSONSerialization.jsonObject(with: Data(manifest.utf8)) as? [String: Any],
        let capabilities = fields["capabilities"] as? [String: Any]
      else { throw SiteReadError.malformed }
      guard fields["type"] as? String == "tavern", capabilities["tavernAsset"] as? Bool == true
      else { return nil }
      var summary = try item.summary.flatMap { try JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
      summary["title"] = item.title
      let bytes = try JSONSerialization.data(withJSONObject: summary)
      return try service.tavernResource(store: store, source: site, work: item.id,
        summary: String(decoding: bytes, as: UTF8.self), request: request)
    }
  }

  public func saveTavernResource(_ resource: SourceTavernResource) async throws {
    _ = try await run { store, _ in try resource.save(store: store) }
  }

  public func originalTavernBytes(_ resource: SourceTavernResource) async throws -> Data {
    try await run { _, _ in
      let file = try resource.originalFile(directory: FileManager.default.temporaryDirectory.path)
      return try withExtendedLifetime(file) { try Data(contentsOf: URL(fileURLWithPath: file.path())) }
    }
  }

  public func siteImage(_ library: SourceLibrary, site: String, address: String) async throws
    -> PageImage
  {
    let root = directory
    return try await run { store, request in
      let service = try library.service(directory: root, request: request)
      let images = try service.sourceImages(source: site, store: store, request: request)
      return try images.load(address: address, request: request)
    }
  }
}
