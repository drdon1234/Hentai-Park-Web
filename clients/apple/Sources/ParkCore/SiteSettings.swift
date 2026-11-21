import Foundation

/// 保留插件选项的 JSON 类型。数字、开关及多选值按原类型传回站点。
public enum PluginOptionValue: Hashable, Sendable, Codable {
  case text(String), number(Decimal), flag(Bool), list([PluginOptionValue]), none

  public init(from decoder: Decoder) throws {
    let value = try decoder.singleValueContainer()
    if value.decodeNil() { self = .none }
    else if let flag = try? value.decode(Bool.self) { self = .flag(flag) }
    else if let text = try? value.decode(String.self) { self = .text(text) }
    else if let number = try? value.decode(Decimal.self) { self = .number(number) }
    else { self = .list(try value.decode([PluginOptionValue].self)) }
  }

  public func encode(to encoder: Encoder) throws {
    var box = encoder.singleValueContainer()
    switch self {
    case .text(let x): try box.encode(x)
    case .number(let x): try box.encode(x)
    case .flag(let x): try box.encode(x)
    case .list(let x): try box.encode(x)
    case .none: try box.encodeNil()
    }
  }
}

public struct PluginOption: Decodable, Sendable, Identifiable {
  public struct Choice: Decodable, Sendable {
    public let label: String
    public let value: PluginOptionValue
  }
  public let key: String
  public let label: String?
  public let type: String?
  public let dependsOn: [String]?
  public let keywordDependent: Bool?
  public let min, max, step: Decimal?
  public let dynamic: Bool?
  public let secret: Bool?
  public let values: [Choice]?
  public let defaultValue: PluginOptionValue?
  public var id: String { key }
  private enum CodingKeys: String, CodingKey { case key, label, type, dynamic, dependsOn, keywordDependent, min, max, step, secret, values, defaultValue = "default" }

  func selection(_ previous: PluginOptionValue?) -> PluginOptionValue? {
    guard let previous, previous != .none else { return defaultValue }
    let allowed = Set((values ?? []).map(\.value))
    if type == "multi" {
      if case .list(let chosen) = previous, allowed.isEmpty || chosen.allSatisfy(allowed.contains) { return previous }
    } else if allowed.isEmpty || allowed.contains(previous) { return previous }
    return defaultValue
  }
}


public struct SiteLoginForm: Decodable, Sendable {
  public struct Field: Decodable, Identifiable, Sendable {
    public let key: String
    public let label: String?
    public let optional, secret: Bool?
    public var id: String { key }
  }
  public let title, hint, type: String?
  public let fields: [Field]?
  public let managedAccount: Bool?

  public func accepts(_ values: [String: String]) -> Bool {
    guard let fields, !fields.isEmpty else { return false }
    let used = Set(values.filter { !$0.value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }.keys)
    return !used.isEmpty && Set(values.keys).isSubset(of: Set(fields.map(\.key)))
      && fields.allSatisfy { $0.optional == true || used.contains($0.key) }
  }
}

public struct SiteOptionState: Decodable, Sendable {
  public let values: [String: PluginOptionValue]
  public let configured: [String]
}

public struct SiteSettings: Sendable {
  public let name: String
  public let login: SiteLoginForm?
  public let options: [PluginOption]
  public let account: SiteAccount?
  public let state: SiteOptionState
  private let copy: CopyCatalog
  public func localized(_ key: String) -> String { copy.text(key: key, params: [:]) }

  fileprivate init(manifest raw: String, locale: String, state: SiteOptionState, account: SiteAccount?) throws {
    struct Manifest: Decodable {
      let name: String
      let account: SiteLoginForm?
      let settings: [PluginOption]?
    }
    let data = Data(raw.utf8)
    let manifest = try JSONDecoder().decode(Manifest.self, from: data)
    guard let fields = try JSONSerialization.jsonObject(with: data) as? [String: Any] else { throw SiteReadError.malformed }
    let words = try fields["i18n"].map { String(decoding: try JSONSerialization.data(withJSONObject: $0), as: UTF8.self) }
    copy = try CopyCatalog(locale: locale, pluginJson: words)
    name = manifest.name; login = manifest.account; options = manifest.settings ?? []
    self.state = state; self.account = account
  }
}

extension ProgressSession {
  public func hasSiteSettings(_ library: SourceLibrary, source: String) async throws -> Bool {
    let root = directory
    return try await run { _, request in
      let service = try library.service(directory: root, request: request)
      let raw = try service.sourceManifest(source: source, request: request)
      guard let spec = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any] else { throw SiteReadError.malformed }
      return spec["account"] is [String: Any] || !(spec["settings"] as? [Any] ?? []).isEmpty
    }
  }

  public func siteSettings(_ library: SourceLibrary, source: String, locale: String) async throws -> SiteSettings {
    let root = directory
    return try await run { _, request in
      let service = try library.service(directory: root, request: request)
      let manifest = try service.sourceManifest(source: source, request: request)
      let options = try service.sourceOptions(source: source, request: request)
      let state = try JSONDecoder().decode(SiteOptionState.self, from: Data(options.utf8))
      struct Status: Decodable { struct Features: Decodable { let status: Bool? }; let capabilities: Features? }
      let capabilities = try JSONDecoder().decode(Status.self, from: Data(manifest.utf8))
      let account = capabilities.capabilities?.status == true ? try service.siteAccount(source: source, request: request) : nil
      return try SiteSettings(manifest: manifest, locale: locale, state: state, account: account)
    }
  }

  public func changeSiteOption(_ library: SourceLibrary, source: String, key: String, value: PluginOptionValue) async throws -> SiteOptionState {
    let root = directory
    let encoded = String(decoding: try JSONEncoder().encode(value), as: UTF8.self)
    return try await run { _, request in
      let service = try library.service(directory: root, request: request)
      let response = try service.setSourceOption(source: source, name: key, jsonValue: encoded, request: request)
      return try JSONDecoder().decode(SiteOptionState.self, from: Data(response.utf8))
    }
  }

  public func signIn(_ library: SourceLibrary, source: String, values: [String: String]) async throws -> SiteAccount {
    let root = directory
    return try await run { _, request in
      try library.service(directory: root, request: request).signIn(source: source, values: values, request: request)
    }
  }

  public func signOut(_ library: SourceLibrary, source: String) async throws {
    let root = directory
    try await run { _, request in
      try library.service(directory: root, request: request).signOut(source: source, request: request)
    }
  }
}
