import Foundation

public struct SiteBrowseOptions: Sendable {
  public struct Feed: Decodable, Sendable, Identifiable {
    public let key: String
    public let label: String?
    public let options: [PluginOption]?
    public let commonFilters: [String: SiteCommonRule]?
    public var id: String { key }
  }

  public let feeds: [Feed]
  public let searchFilters: [PluginOption]
  public let searchCommonFilters: [String: SiteCommonRule]
  public let hasSettings: Bool
  private let copy: CopyCatalog

  init(manifest: String, locale: String) throws {
    struct Fields: Decodable {
      let feeds: [Feed]?
      let searchFilters: [PluginOption]?
      let searchCommonFilters: [String: SiteCommonRule]?
    }
    let data = Data(manifest.utf8)
    let fields = try JSONDecoder().decode(Fields.self, from: data)
    feeds = fields.feeds ?? []
    searchFilters = fields.searchFilters ?? []
    searchCommonFilters = fields.searchCommonFilters ?? [:]
    let object = try JSONSerialization.jsonObject(with: data) as? [String: Any]
    hasSettings = object?["account"] is [String: Any] || !(object?["settings"] as? [Any] ?? []).isEmpty
    let words = try object?["i18n"].map { String(decoding: try JSONSerialization.data(withJSONObject: $0), as: UTF8.self) }
    copy = try CopyCatalog(locale: locale, pluginJson: words)
  }

  public func localized(_ key: String) -> String { copy.text(key: key, params: [:]) }

  public static func defaults(_ options: [PluginOption]) -> [String: PluginOptionValue] {
    options.reduce(into: [:]) { values, option in values[option.key] = option.defaultValue }
  }

  public static func reset(_ keys: Set<String>, in options: [PluginOption], values: inout [String: PluginOptionValue]) {
    var affected = keys
    for _ in options {
      let children = options.filter { !affected.isDisjoint(with: $0.dependsOn ?? []) }.map(\.key)
      let prior = affected.count
      affected.formUnion(children)
      if prior == affected.count { break }
    }
    for option in options where affected.contains(option.key) { values[option.key] = option.defaultValue }
  }
}

public struct SiteCommonRule: Decodable, Sendable, Equatable {
  public let range, sort: Set<String>
  private enum Fields: String, CodingKey { case range, sort }

  public init(from decoder: Decoder) throws {
    let fields = try decoder.container(keyedBy: Fields.self)
    func supported(_ key: Fields, _ allowed: Set<String>) throws -> Set<String> {
      if let flag = try? fields.decode(Bool.self, forKey: key) { return flag ? allowed : [] }
      return allowed.intersection(try fields.decodeIfPresent([String].self, forKey: key) ?? [])
    }
    range = try supported(.range, ["min", "max"])
    sort = try supported(.sort, ["asc", "desc"])
  }

  public static let dimensions = ["pages", "likes", "duration", "score", "favorites"]

  public static func number(_ text: String) throws -> Decimal {
    let scanner = Scanner(string: text.trimmingCharacters(in: .whitespacesAndNewlines))
    scanner.locale = Locale(identifier: "en_US_POSIX")
    guard let n = scanner.scanDecimal(), scanner.isAtEnd, !n.isNaN else { throw SiteReadError.malformed }
    return n
  }

  public static func validated(_ draft: [String: [String: String]], rules: [String: SiteCommonRule]) throws -> [String: [String: PluginOptionValue]] {
    var result: [String: [String: PluginOptionValue]] = [:]
    var ordered = false
    for dimension in dimensions {
      guard let rule = rules[dimension], let fields = draft[dimension] else { continue }
      var record: [String: PluginOptionValue] = [:]
      for bound in rule.range {
        guard let input = fields[bound], !input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { continue }
        let n = try number(input)
        guard dimension == "score" || n >= 0 else { throw SiteReadError.malformed }
        if dimension != "duration" {
          var original = n, integer = Decimal()
          NSDecimalRound(&integer, &original, 0, .down)
          guard integer == n, n >= -9_007_199_254_740_991, n <= 9_007_199_254_740_991 else { throw SiteReadError.malformed }
        }
        record[bound] = .number(n)
      }
      if case .number(let low) = record["min"], case .number(let high) = record["max"], low > high { throw SiteReadError.malformed }
      if let order = fields["order"], !order.isEmpty {
        guard !ordered, rule.sort.contains(order) else { throw SiteReadError.malformed }
        ordered = true
        record["order"] = .text(order)
      }
      if !record.isEmpty { result[dimension] = record }
    }
    return result
  }
}

public struct SiteFilterQuery: Hashable, Sendable {
  let source, scope, filter: String
  let feed, keyword: String?
  let parents: [String: PluginOptionValue]

  public init?(source: String, feed: String?, keyword: String, option: PluginOption, values: [String: PluginOptionValue], definitions: [PluginOption]) {
    guard option.dynamic == true else { return nil }
    var parents: [String: PluginOptionValue] = [:]
    for key in option.dependsOn ?? [] {
      guard let parent = definitions.first(where: { $0.key == key }), let value = values[key] else { return nil }
      let normalized: PluginOptionValue
      switch value {
      case .text(let text) where !text.isEmpty && parent.type == "number":
        guard let number = try? SiteCommonRule.number(text),
              parent.min.map({ number >= $0 }) ?? true, parent.max.map({ number <= $0 }) ?? true else { return nil }
        normalized = .number(number)
      case .text(let text) where !text.isEmpty: normalized = value
      case .number, .flag: normalized = value
      default: return nil
      }
      if parent.dynamic != true, let choices = parent.values, !choices.isEmpty,
         !choices.contains(where: { $0.value == normalized }) { return nil }
      parents[key] = normalized
    }
    self.source = source
    scope = feed == nil ? "search" : "feed"
    self.feed = feed
    self.keyword = feed == nil ? keyword : nil
    filter = option.key
    self.parents = parents
  }
}

extension ProgressSession {
  public func browseOptions(_ library: SourceLibrary, site: String, locale: String) async throws -> SiteBrowseOptions {
    let root = directory
    return try await run { _, request in
      let service = try library.service(directory: root, request: request)
      return try SiteBrowseOptions(manifest: service.sourceManifest(source: site, request: request), locale: locale)
    }
  }

  public func filterChoices(_ library: SourceLibrary, query: SiteFilterQuery, confirmation: ManualRead? = nil) async throws -> [PluginOption.Choice] {
    var request: [String: Any] = ["scope": query.scope, "filter": query.filter,
      "values": try JSONSerialization.jsonObject(with: JSONEncoder().encode(query.parents))]
    request["feed"] = query.feed
    request["keyword"] = query.keyword
    let data = try JSONSerialization.data(withJSONObject: [request])
    let raw = try await siteRead(library, site: query.source, method: .filterOptions,
                                 arguments: String(decoding: data, as: UTF8.self), confirmation: confirmation)
    return try JSONDecoder().decode([PluginOption.Choice].self, from: Data(raw.utf8))
  }
}
