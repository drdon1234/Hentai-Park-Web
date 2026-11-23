import Foundation

public struct SiteChart: Sendable, Identifiable {
  public let site: InstalledSite
  public let key, label, category, section, kind, metric: String
  public let feed: String?
  let domains: [String]
  let declared: [PluginOption]
  let copy: CopyCatalog
  public var id: String { site.key + ":" + key }
  public func localized(_ label: String) -> String { copy.text(key: label, params: [:]) }

  public func owns(_ link: URL) -> Bool {
    guard let host = link.host?.lowercased(), link.scheme?.lowercased() == "https",
          link.user == nil, link.password == nil, (link.port ?? 443) == 443 else { return false }
    return domains.contains { value in
      let suffix = value.lowercased()
      return !suffix.isEmpty && (host == suffix || host.hasSuffix("." + suffix))
    }
  }

  public func workID(for row: SiteWork) -> String? {
    guard kind == "work", let feed, !feed.isEmpty, site.detail else { return nil }
    return (row.listingField("workId") as? String).flatMap { $0.isEmpty ? nil : $0 } ?? row.id
  }
}

extension SiteWork {
  public var rankingKeyword: String {
    let keyword = (listingField("keyword") as? String)?.trimmingCharacters(in: .whitespacesAndNewlines)
    return keyword.flatMap { $0.isEmpty ? nil : $0 } ?? title
  }
  public var rankingPosition: Int? { listingField("rank") as? Int }
  public var rankingLink: URL? {
    guard let link = listingField("url") as? String, let url = URL(string: link),
          url.scheme?.lowercased() == "https", url.host != nil, url.user == nil, url.password == nil else { return nil }
    return url
  }
}

public struct ChartSettings: Codable, Sendable {
  public var category, section, kind, selected: String?
  public var navigation, searchSources, detailSources: [String: String]?
  public var options: [String: [String: PluginOptionValue]]?

  public func board(in boards: [SiteChart], category newCategory: String? = nil,
                    section newSection: String? = nil, kind newKind: String? = nil) -> SiteChart? {
    guard let first = boards.first else { return nil }
    let cats = boards.filter { $0.category == (newCategory ?? category ?? "jav") }
    let rows = cats.isEmpty ? boards.filter { $0.category == first.category } : cats
    let cat = rows[0].category
    let group = [newSection, navigation?["section-" + cat], section, "market", "site"].compactMap { $0 }
      .first { value in rows.contains { $0.section == value } } ?? rows[0].section
    let within = rows.filter { $0.section == group }
    let kinds = [newKind, navigation?["kind-" + cat + "-" + group], kind, "work", "actress", "genre", "seller"]
    let type = kinds.compactMap { $0 }.first { value in within.contains { $0.kind == value } } ?? within[0].kind
    let matching = within.filter { $0.kind == type }
    let preferred = navigation?["board-" + cat + "-" + group + "-" + type] ?? selected
    return matching.first { $0.id == preferred } ?? matching.first
  }

  public func values(for board: SiteChart, definitions: [PluginOption]) -> [String: PluginOptionValue] {
    var result = [String: PluginOptionValue]()
    for option in definitions { result[option.key] = option.selection(options?[board.id]?[option.key]) }
    return result
  }

  mutating func select(_ board: SiteChart) {
    category = board.category; section = board.section; kind = board.kind; selected = board.id
    var nav = navigation ?? [:]
    nav["section-" + board.category] = board.section
    nav["kind-" + board.category + "-" + board.section] = board.kind
    nav["board-" + board.category + "-" + board.section + "-" + board.kind] = board.id
    navigation = nav
  }
}

private struct ChartPreferences: Decodable, Sendable { let rankingSelection: ChartSettings? }
private let chartWriteLock = NSLock()

extension ProgressSession {
  public func chartSettings() async throws -> ChartSettings {
    try await preferences(ChartPreferences.self).rankingSelection ?? ChartSettings()
  }

  public func rememberChart(_ board: SiteChart, values: [String: PluginOptionValue]? = nil,
                            searchSite: String? = nil, detailSite: String? = nil) async throws -> ChartSettings {
    let file = directory.appendingPathComponent("park/settings.json")
    return try await run { store, _ in
      chartWriteLock.lock()
      defer { chartWriteLock.unlock() }
      var settings: ChartSettings
      do { settings = try JSONDecoder().decode(ChartPreferences.self, from: Data(contentsOf: file)).rankingSelection ?? ChartSettings() }
      catch let error as CocoaError where error.code == .fileReadNoSuchFile { settings = ChartSettings() }
      settings.select(board)
      if let values { var map = settings.options ?? [:]; map[board.id] = values; settings.options = map }
      if let searchSite { var map = settings.searchSources ?? [:]; map[board.category] = searchSite; settings.searchSources = map }
      if let detailSite { var map = settings.detailSources ?? [:]; map[board.category] = detailSite.isEmpty ? nil : detailSite; settings.detailSources = map }
      let json = String(decoding: try JSONEncoder().encode(settings), as: UTF8.self)
      try store.savePreference(name: "rankingSelection", json: json)
      return settings
    }
  }

  public func charts(_ library: SourceLibrary, locale: String) async throws -> [SiteChart] {
    let sources = try await installedSites(library).filter(\.rankings)
    let root = directory
    return try await run { _, request in
      let service = try library.service(directory: root, request: request)
      var charts = [SiteChart]()
      for site in sources {
        let raw = try service.sourceManifest(source: site.key, request: request)
        guard let manifest = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any],
              let rows = manifest["rankings"] as? [[String: Any]] else { throw SiteReadError.malformed }
        let words = try manifest["i18n"].map { String(decoding: try JSONSerialization.data(withJSONObject: $0), as: UTF8.self) }
        let copy = try CopyCatalog(locale: locale, pluginJson: words)
        for row in rows {
          guard let key = row["key"] as? String, !key.isEmpty,
                let category = (row["category"] as? String) ?? site.category else { throw SiteReadError.malformed }
          let section = row["section"] as? String ?? (site.rankingOnly == true ? "market" : "site")
          let kind = row["kind"] as? String ?? "work"
          guard ["market","site"].contains(section), ["work","seller","actress","genre"].contains(kind) else { continue }
          let definitions = try JSONSerialization.data(withJSONObject: row["options"] ?? [])
          let options = try JSONDecoder().decode([PluginOption].self, from: definitions)
          let label = copy.text(key: row["label"] as? String ?? key, params: [:])
          let metric = copy.text(key: row["metric"] as? String ?? "", params: [:])
          charts.append(SiteChart(site: site, key: key, label: label, category: category, section: section,
                                  kind: kind, metric: metric, feed: row["feed"] as? String, domains: manifest["domains"] as? [String] ?? [], declared: options, copy: copy))
        }
      }
      return charts
    }
  }

  public func chartOptions(_ library: SourceLibrary, chart: SiteChart, confirmation: ManualRead? = nil) async throws -> [PluginOption] {
    guard chart.declared.contains(where: { $0.dynamic == true }) else { return chart.declared }
    let args = String(decoding: try JSONEncoder().encode([chart.key]), as: UTF8.self)
    let raw = try await siteRead(library, site: chart.site.key, method: .rankingOptions, arguments: args, confirmation: confirmation)
    return try JSONDecoder().decode([PluginOption].self, from: Data(raw.utf8))
  }

  public func chartPage(_ library: SourceLibrary, chart: SiteChart, page: Int, offset: Int, cursor: String?,
                        values: [String: PluginOptionValue], confirmation: ManualRead? = nil) async throws -> SiteWorkPage {
    var request: [String: Any] = ["page": page, "offset": offset,
      "options": try JSONSerialization.jsonObject(with: JSONEncoder().encode(values))]
    request["cursor"] = cursor
    let data = try JSONSerialization.data(withJSONObject: [chart.key, request])
    let raw = try await siteRead(library, site: chart.site.key, method: .ranking,
                                 arguments: String(decoding: data, as: UTF8.self), confirmation: confirmation)
    return try await decodeSitePage(raw, source: chart.site.key, cursor: cursor)
  }
}

private func normalizedChartIdentity(_ value: String) -> String {
      value.lowercased().unicodeScalars.filter {
        !CharacterSet.whitespacesAndNewlines.contains($0) && !CharacterSet.punctuationCharacters.contains($0)
      }.map(String.init).joined()
    }

extension SiteWork {
  public func matchesChartWork(_ row: SiteWork) -> Bool {
    let titleKey = normalizedChartIdentity(title), query = normalizedChartIdentity(row.rankingKeyword)
    return (!titleKey.isEmpty && titleKey == normalizedChartIdentity(row.title)) ||
      (!query.isEmpty && (normalizedChartIdentity(id) == query ||
        (listingField("code") as? String).map(normalizedChartIdentity) == query))
  }
}

public struct ChartReference: Decodable, Sendable, Identifiable {
  public let id, title: String
  public let code, description: String?
  public func matches(_ row: SiteWork) -> Bool {
    let name = normalizedChartIdentity(title), keyword = normalizedChartIdentity(row.rankingKeyword)
    return (!name.isEmpty && name == normalizedChartIdentity(row.title)) ||
      (!keyword.isEmpty && code.map(normalizedChartIdentity) == keyword)
  }
}

extension ProgressSession {
  public func chartDetailSites(_ library: SourceLibrary, chart: SiteChart, charts: [SiteChart]) async throws -> [InstalledSite] {
    try await installedSites(library).filter { site in
      (site.category == chart.category || charts.contains { $0.site.key == site.key && $0.category == chart.category }) &&
        ((site.metadataDetail && (site.key == chart.site.key || site.metadataSearch)) ||
          (site.rankingOnly != true && site.search == true && site.detail))
    }
  }

  public func chartReferences(_ library: SourceLibrary, site: InstalledSite, chart: SiteChart,
                              row: SiteWork, confirmation: ManualRead? = nil) async throws -> [ChartReference] {
    guard site.metadataSearch else { throw SiteReadError.malformed }
    let data = try JSONSerialization.data(withJSONObject: [row.rankingKeyword, ["category": chart.category]])
    let raw = try await siteRead(library, site: site.key, method: .metadataSearch,
                                 arguments: String(decoding: data, as: UTF8.self), confirmation: confirmation)
    return try JSONDecoder().decode([ChartReference].self, from: Data(raw.utf8))
  }

  public func chartReference(_ library: SourceLibrary, site: InstalledSite, chart: SiteChart,
                             row: SiteWork, referenceID: String, confirmation: ManualRead? = nil) async throws -> ChartReference {
    guard site.metadataDetail else { throw SiteReadError.malformed }
    var args: [Any] = [referenceID]
    if site.key == chart.site.key && referenceID == row.id {
      var context = ["board": chart.key]
      context["url"] = row.rankingLink?.absoluteString
      args.append(context)
    }
    let data = try JSONSerialization.data(withJSONObject: args)
    let raw = try await siteRead(library, site: site.key, method: .metadataDetail,
                                 arguments: String(decoding: data, as: UTF8.self), confirmation: confirmation)
    let item = try JSONDecoder().decode(ChartReference.self, from: Data(raw.utf8))
    guard item.id == referenceID else { throw SiteReadError.malformed }
    return item
  }
}
