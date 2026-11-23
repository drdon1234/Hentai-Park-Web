import Foundation

public struct FavoriteUpdates: Decodable, Sendable {
  public struct Category: Decodable, Identifiable, Sendable {
    public let id: Int64
    public let name: String
  }
  public struct Entry: Decodable, Hashable, Sendable {
    public let source: String
    public let id: String
    public let title: String
    public let chapter: String
    public let label: String
  }
  public let categories: [Category]
  public let items: [Entry]
}

public enum FavoriteFilter: String, CaseIterable, Sendable {
  case updateOnlyMultiChapter, updateSkipUnread, updateSkipNotStarted, autoDownloadNew
}

public struct FavoritePreferences: Decodable, Sendable {
  public let askCategoryOnFavorite: Bool?
  public let libraryUpdateHours: Int?
  let adultConfirmed: Bool?
  let downloadDirectory: String?
  private let updateOnlyMultiChapter: Bool?
  private let updateSkipUnread: Bool?
  private let updateSkipNotStarted: Bool?
  private let autoDownloadNew: Bool?

  public func enabled(_ filter: FavoriteFilter) -> Bool {
    switch filter {
    case .updateOnlyMultiChapter: return updateOnlyMultiChapter ?? true
    case .updateSkipUnread: return updateSkipUnread ?? false
    case .updateSkipNotStarted: return updateSkipNotStarted ?? false
    case .autoDownloadNew: return autoDownloadNew ?? false
    }
  }
}

public struct FavoriteCheck: Decodable, Sendable {
  public let busy: Bool?
  public let checked: Int?
  public let chapters: Int?
  public let failed: Int?
  public let updated: Int?
}

extension ProgressSession {
  public func favoritePreferences() async throws -> FavoritePreferences {
    try await preferences(FavoritePreferences.self)
  }

  public func setFavoriteFilter(_ filter: FavoriteFilter, enabled: Bool) async throws {
    try await run { store, _ in try store.savePreference(name: filter.rawValue, json: enabled ? "true" : "false") }
  }

  public func setFavoriteInterval(_ hours: Int) async throws {
    guard [0, 6, 12, 24, 48, 168].contains(hours) else { throw SiteReadError.malformed }
    try await run { store, _ in try store.savePreference(name: "libraryUpdateHours", json: String(hours)) }
  }

  public func favoriteUpdates(category: Int64?) async throws -> FavoriteUpdates {
    try await run { store, _ in
      try JSONDecoder().decode(FavoriteUpdates.self, from: Data(store.favoriteUpdates(category: category).utf8))
    }
  }

  public func checkFavorites(_ library: SourceLibrary, category: Int64?, force: Bool) async throws -> FavoriteCheck? {
    let prefs = try await favoritePreferences()
    guard prefs.adultConfirmed == true, force || (prefs.libraryUpdateHours ?? 0) > 0 else { return nil }
    let root = directory
    let volume = prefs.enabled(.autoDownloadNew) ? try downloadDirectory(configured: prefs.downloadDirectory).path : ""
    return try await run { store, request in
      let raw = try library.service(directory: root, request: request)
        .checkFavorites(store: store, category: category, downloadDirectory: volume, force: force, request: request)
      return try JSONDecoder().decode(FavoriteCheck.self, from: Data(raw.utf8))
    }
  }
}
