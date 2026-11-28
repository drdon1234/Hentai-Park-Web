import Foundation

public struct SearchScope: Sendable {
  public let sites: [InstalledSite]
  public let excluded: Set<String>
  fileprivate let referenceDomains: [String: String]
}

extension ProgressSession {
  public func searchScope(_ library: SourceLibrary, category: String? = nil, preferred: String? = nil, metadataOnly: Bool = false) async throws -> SearchScope {
    struct Scope: Decodable, Sendable {
      let aggregateExcluded: [String]?
      let domainOrder: [String]?
      let homeDimension: String?
      let homeMedia: String?
    }
    let settings = try await preferences(Scope.self)
    let domains = discoveryDomains(order: settings.domainOrder ?? [],
                                   dimension: settings.homeDimension ?? "all", media: settings.homeMedia ?? "all")
    let referenceCategories = category.map { [$0] } ?? domains
    var referenceDomains = [String: String]()
    let available = try await installedSites(library).filter { site in
      if metadataOnly {
        guard site.search == true || site.metadataSearch,
              let domain = referenceCategories.first(where: { site.offersReference(in: $0) }) else { return false }
        referenceDomains[site.key] = domain
        return true
      }
      return site.search == true && site.isBrowsableSite
        && (category.map { allowed in site.category == allowed } ?? domains.contains(site.category ?? ""))
    }
    if let preferred {
      let selected = preferred == "preferred" ? available.first : available.first { $0.key == preferred }
      return SearchScope(sites: selected.map { [$0] } ?? [], excluded: [], referenceDomains: referenceDomains)
    }
    return SearchScope(sites: available, excluded: Set(settings.aggregateExcluded ?? []), referenceDomains: referenceDomains)
  }

  public func setSearchExcluded(_ keys: Set<String>) async throws {
    let json = String(decoding: try JSONEncoder().encode(keys.sorted()), as: UTF8.self)
    try await run { store, _ in try store.savePreference(name: "aggregateExcluded", json: json) }
  }
}

public struct SiteSearchSection: Identifiable, Sendable {
  public let site: InstalledSite
  public var id: String { site.key }
  public fileprivate(set) var items: [SiteWork] = []
  public fileprivate(set) var page = 0
  fileprivate var cursor: String?
  public fileprivate(set) var hasNext = true
  public fileprivate(set) var busy = true
  public fileprivate(set) var error: Error?
}

/// 每次提交的检索词拥有独立的分页状态与请求。离开或提交新词时取消整个检索。
public actor AggregateSearch {
  public nonisolated let updates: AsyncStream<[SiteSearchSection]>
  private let publish: AsyncStream<[SiteSearchSection]>.Continuation
  private let session: ProgressSession
  private let library: SourceLibrary
  private let keyword: String
  private let referenceDomains: [String: String]
  private let order: [String]
  private var rows: [String: SiteSearchSection]
  private var pending: [String]
  private var running: [String: Task<Void, Never>] = [:]
  private var closed = false

  public init(session: ProgressSession, library: SourceLibrary, keyword: String, scope: SearchScope) {
    self.session = session
    self.library = library
    self.keyword = keyword.trimmingCharacters(in: .whitespacesAndNewlines)
    referenceDomains = scope.referenceDomains
    let included = self.keyword.isEmpty ? [] : scope.sites.filter { !scope.excluded.contains($0.key) }
    order = included.map(\.key)
    pending = order
    rows = Dictionary(uniqueKeysWithValues: included.map { ($0.key, SiteSearchSection(site: $0)) })
    let channel = AsyncStream<[SiteSearchSection]>.makeStream(bufferingPolicy: .bufferingNewest(1))
    updates = channel.stream
    publish = channel.continuation
  }

  public func start() {
    send()
    schedule()
  }

  public func more(site: String) {
    guard !closed, var row = rows[site], !row.busy, row.hasNext else { return }
    row.busy = true
    row.error = nil
    rows[site] = row
    pending.append(site)
    send()
    schedule()
  }

  public func close() {
    guard !closed else { return }
    closed = true
    pending.removeAll()
    running.values.forEach { $0.cancel() }
    running.removeAll()
    publish.finish()
  }

  private func schedule() {
    while !closed && running.count < 6 && !pending.isEmpty {
      let id = pending.removeFirst()
      guard let row = rows[id] else { continue }
      running[id] = Task { await fetch(row) }
    }
  }

  private func fetch(_ previous: SiteSearchSection) async {
    var row = previous
    do {
      let next = try await session.sitePage(library, site: row.site, keyword: keyword, page: row.page + 1, cursor: row.cursor,
                                            referenceDomain: referenceDomains[row.id])
      var ids = Set(row.items.map(\.id))
      row.items.append(contentsOf: next.items.filter { ids.insert($0.id).inserted })
      row.page += 1
      row.hasNext = next.hasNext
      row.cursor = next.cursor
    } catch { row.error = error }
    guard !closed, !Task.isCancelled else { return }
    row.busy = false
    rows[row.id] = row
    running.removeValue(forKey: row.id)
    send()
    schedule()
  }

  private func send() {
    publish.yield(order.compactMap { rows[$0] })
  }
}
