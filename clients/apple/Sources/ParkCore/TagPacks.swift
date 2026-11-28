import Foundation

extension Notification.Name {
  public static let tagPackChanged = Notification.Name("ParkCore.tagPackChanged")
  public static let tagRemindersChanged = Notification.Name("ParkCore.tagRemindersChanged")
}

public struct TagPackKey: Hashable, Decodable, Sendable {
  public let source: String
  public let locale: String
  public init(source: String, locale: String) { self.source = source; self.locale = locale }
}

public struct TagPackInventory: Sendable {
  public let bundled: Set<TagPackKey>
  public let installed: Set<TagPackKey>
  public let names: [String: String]
}

public struct OfficialTagPack: Decodable, Sendable {
  public let source: String
  public let locale: String
  public let version: String
}

extension ProgressSession {
  public struct TagReminderSettings: Decodable, Sendable {
    public let tagDownloadReminders: Bool?
    public let tagDownloadIgnoredSources: [String]?
    public func allows(_ key: TagPackKey) -> Bool {
      tagDownloadReminders != false && tagDownloadIgnoredSources?.contains(key.source) != true
    }
  }

  public func tagReminderSettings() async throws -> TagReminderSettings { try await preferences(TagReminderSettings.self) }

  public func setTagReminders(_ enabled: Bool) async throws {
    try await run { store, _ in try store.savePreference(name: "tagDownloadReminders", json: String(enabled)) }
    await MainActor.run { NotificationCenter.default.post(name: .tagRemindersChanged, object: nil) }
  }

  public func ignoreTagReminders(source: String) async throws {
    try await run { store, _ in try store.ignoreDictionaryReminder(source: source) }
    await MainActor.run { NotificationCenter.default.post(name: .tagRemindersChanged, object: nil) }
  }

  public func tagPacks(_ library: SourceLibrary) async throws -> TagPackInventory {
    try await run { store, _ in
      let names = Dictionary(uniqueKeysWithValues: try library.bundledSites())
      let assets = library.resources.appendingPathComponent("tag_translations", isDirectory: true)
      let fm = FileManager.default
      let dirs = try fm.contentsOfDirectory(at: assets, includingPropertiesForKeys: [.isDirectoryKey])
      var bundled = Set<TagPackKey>()
      for dir in dirs where try dir.resourceValues(forKeys: [.isDirectoryKey]).isDirectory == true {
        for path in try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil) where path.pathExtension == "json" {
          bundled.insert(TagPackKey(source: dir.lastPathComponent, locale: path.deletingPathExtension().lastPathComponent))
        }
      }
      let installed = try JSONDecoder().decode([TagPackKey].self, from: Data(store.dictionaryInventory().utf8))
      return TagPackInventory(bundled: bundled, installed: Set(installed), names: names)
    }
  }

  public func dictionaryIndex(_ library: SourceLibrary) async throws -> DictionaryIndex {
    let root = directory
    return try await run { _, request in
      try library.service(directory: root, request: request).officialDictionaries(request: request)
    }
  }

  public func installTagPack(_ key: TagPackKey, library: SourceLibrary, index: DictionaryIndex?) async throws {
    let root = directory
    try await run { store, request in
      let bytes: Data
      if let index {
        bytes = try library.service(directory: root, request: request)
          .officialDictionary(index: index, source: key.source, locale: key.locale, request: request)
      } else {
        let assets = library.resources.appendingPathComponent("tag_translations",isDirectory: true)
        let file = assets.appendingPathComponent(key.source).appendingPathComponent(key.locale + ".json").standardizedFileURL
        guard file.deletingLastPathComponent().deletingLastPathComponent().path == assets.standardizedFileURL.path,
              file.deletingLastPathComponent().lastPathComponent == key.source,
              file.deletingPathExtension().lastPathComponent == key.locale else { throw CocoaError(.fileReadInvalidFileName) }
        bytes = try Data(contentsOf: file)
      }
      try Task.checkCancellation()
      try store.installDictionary(source: key.source, locale: key.locale, bytes: bytes)
    }
    NotificationCenter.default.post(name: .tagPackChanged, object: key)
  }

  public func removeTagPack(_ key: TagPackKey) async throws {
    try await run { store, _ in try store.removeDictionary(source: key.source, locale: key.locale) }
    NotificationCenter.default.post(name: .tagPackChanged, object: key)
  }

  public func dictionaryVersion(_ key: TagPackKey) async throws -> String? {
    try await run { store, _ in
      struct Header: Decodable { let version: String }
      guard let bytes = try store.installedDictionary(source: key.source, locale: key.locale) else { return nil }
      return try JSONDecoder().decode(Header.self, from: bytes).version
    }
  }
}
