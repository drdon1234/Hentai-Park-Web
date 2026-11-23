import Foundation

/// 同一应用的窗口共用站点服务。资源文件与脚本均来自当前安装包。
public final class SourceLibrary: BundledScripts, @unchecked Sendable {
  private let resources: URL
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
  public let nativeVideo: Bool
  public let unavailable: Bool?
  let rankings: Bool
  public let metadataSearch: Bool
  public let metadataDetail: Bool
  let rankingOnly: Bool?
  let metadata: Bool?
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
    nativeVideo = site["nativeVideo"] as? Bool == true
    unavailable = site["unavailable"] as? Bool
    rankings = site["rankings"] as? Bool == true
    metadataSearch = site["metadataSearch"] as? Bool == true
    metadataDetail = site["metadataDetail"] as? Bool == true
    rankingOnly = site["rankingOnly"] as? Bool
    metadata = site["metadata"] as? Bool
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

private struct SiteWorkTag: Decodable, Sendable {
  let values: [String]
  private enum Keys: String, CodingKey { case namespace, tags, name }

  private struct Name: Decodable {
    let text: String
    init(from decoder: Decoder) throws {
      if let value = try? decoder.singleValueContainer().decode(String.self) {
        text = value
      } else {
        text = try decoder.container(keyedBy: Keys.self).decode(String.self, forKey: .name)
      }
    }
  }

  init(from decoder: Decoder) throws {
    if let flat = try? decoder.singleValueContainer().decode(String.self) {
      values = [flat]
    } else {
      let group = try decoder.container(keyedBy: Keys.self)
      let namespace = try group.decodeIfPresent(String.self, forKey: .namespace) ?? ""
      values = try group.decode([Name].self, forKey: .tags).map { tag in
        namespace.isEmpty ? tag.text : namespace + ":" + tag.text
      }
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

public struct SiteWork: Decodable, Identifiable, Sendable {
  public struct Chapter: Decodable, Identifiable, Sendable {
    public let id: String
    public let title: String?
  }
  public let id: String
  public let title: String
  public let subtitle: String?
  public let description: String?
  public let cover: String?
  private let tagGroups: [SiteWorkTag]?
  public var tags: [String]? { tagGroups?.flatMap(\.values) }
  public let chapters: [Chapter]?
  public let chaptersComplete: Bool?
  public let singleChapterId: String?
  public let voiceRoot: VoiceNode?
  let voiceMediaForm: String?
  let voiceTrackKinds: [String: String]?
  let voiceSubtitles: [String: [String]]?
  fileprivate var listingSource: String?
  fileprivate var listingData: Data?
  fileprivate var summary: Data?
  fileprivate var bodyData: Data?

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
      try library.service(directory: root, request: request).comicChapter(
        store: store, key: key, request: request)
    }
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
    favorite: Bool? = nil
  ) async throws -> LibraryWork {
    let root = directory
    return try await run { store, request in
      let service = try library.service(directory: root, request: request)
      let raw = try service.sourceManifest(source: site.key, request: request)
      guard let manifest = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any],
        let media = manifest["type"] as? String, let domain = manifest["category"] as? String,
        let capabilities = manifest["capabilities"] as? [String: Any]
      else { throw SiteReadError.malformed }
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
        starred = try store.saveWorkSummary(source: site.key, summary: json, favorite: favorite)
      }
      return LibraryWork(
        downloaded: false, invalidDownload: false, pendingDeletion: nil, downloadStatus: nil,
        readerMode: nil,
        media: media, source: site.key, workId: item.id, title: item.title, categoryNames: [],
        favorite: starred,
        historyMs: nil, historyChapter: nil, historyPage: 0, chapters: chapters,
        contentDomain: domain)
    }
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
    return try await run { store, _ in
      try store.migrateFavorite(fromSource: work.source, fromId: work.workId, toSource: site.key,
        detailJson: json, categories: categories, progress: progress, remove: remove)
    }
  }
  public func sites(_ library: SourceLibrary) async throws -> [InstalledSite] {
    try await installedSites(library).filter {
      $0.rankingOnly != true && $0.metadata != true && $0.reverseOnly != true
        && ($0.entryFeed != nil || $0.search == true || $0.unavailable == true)
    }
  }

  func installedSites(_ library: SourceLibrary) async throws -> [InstalledSite] {
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

  public func sitePage(
    _ library: SourceLibrary, site: InstalledSite, keyword: String, page: Int, cursor: String?,
    feedKey: String? = nil, values: [String: PluginOptionValue]? = nil,
    common: [String: [String: PluginOptionValue]] = [:], confirmation: ManualRead? = nil
  ) async throws -> SiteWorkPage {
    var request: [String: Any] = ["page": page]
    if let cursor { request["cursor"] = cursor }
    request["common"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(common))
    let options = try values.map { try JSONSerialization.jsonObject(with: JSONEncoder().encode($0)) }
    let args: [Any]
    let method: BrowseMethod
    if !keyword.isEmpty, site.search == true {
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
    return try await decodeSitePage(raw, site: site, cursor: cursor)
  }

  func decodeSitePage(_ raw: String, site: InstalledSite, cursor: String?) async throws -> SiteWorkPage {
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
      work.listingSource = site.key
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
    var work = try JSONDecoder().decode(SiteWork.self, from: JSONSerialization.data(withJSONObject: fields))
    guard !work.title.isEmpty else { throw SiteReadError.malformed }
    if let blocks = fields["blocks"] as? [Any] {
      work.bodyData = try JSONSerialization.data(withJSONObject: blocks)
    }
    work.summary = try JSONSerialization.data(
      withJSONObject: fields.filter { keys.contains($0.key) && !($0.value is NSNull) })
    return work
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
