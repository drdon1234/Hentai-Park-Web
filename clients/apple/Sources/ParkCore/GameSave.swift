import Foundation

public struct GameSave: Sendable {
  public static let byteLimit = 8 * 1024 * 1024
  public let data: Data
  public let origin: String
  private let source: String
  private let game: String

  public static func decode(_ data: Data, source: String, target: OnlineGame) throws -> GameSave {
    guard data.count <= byteLimit else { throw SiteReadError.malformed }
    struct Fields: Decodable {
      let source: String
      let game: String
      let origin: String
      let localStorage: [String: String]
    }
    let fields = try JSONDecoder().decode(Fields.self, from: data)
    let save = GameSave(data: data, origin: fields.origin, source: fields.source, game: fields.game)
    try save.validate(source: source, target: target)
    // 保留原始 JSON；Web Storage 可以同时包含不同 Unicode 规范形式的键。
    return save
  }

  fileprivate func validate(source: String, target: OnlineGame) throws {
    guard source == self.source, game == target.storageId(), target.allowsDocument(address: origin),
      let host = URL(string: origin)?.host, origin == "https://" + host.lowercased()
    else { throw SiteReadError.malformed }
  }

  /// 写入前核对页面仍处于所选来源，保留备份后再核对页面内容有无变化。
  public static let snapshotScript = """
    if (location.origin !== origin) return null;
    const entries = Object.create(null);
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      entries[key] = localStorage.getItem(key);
    }
    const value = JSON.stringify({source, game, origin, localStorage: entries});
    return value.length <= limit ? value : null;
    """

  public static let restoreScript = """
    if (location.origin !== origin) return false;
    const old = JSON.parse(previous).localStorage, next = JSON.parse(incoming).localStorage;
    if (Object.values(next).some(value => typeof value !== "string")) return false;
    const keys = Object.keys(old);
    if (keys.length !== localStorage.length || keys.some(k => old[k] !== localStorage.getItem(k))) return false;
    function replace(entries) {
      localStorage.clear();
      for (const [key, value] of Object.entries(entries)) localStorage.setItem(key, value);
    }
    try { replace(next); return true; }
    catch { try { replace(old); } catch {} return false; }
    """
}

extension ProgressSession {
  public func preserveGameSave(_ save: GameSave, source: String, target: OnlineGame) async throws {
    let folder = directory.appendingPathComponent("park/online_game_saves", isDirectory: true)
    try await run { _, _ in
      try save.validate(source: source, target: target)
      try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
      let stem = target.profileName().dropFirst("park_game_".count)
      let file = folder.appendingPathComponent("\(stem)_\(UUID().uuidString).json")
      try save.data.write(to: file, options: .atomic)
      let handle = try FileHandle(forWritingTo: file)
      defer { try? handle.close() }
      try handle.synchronize()
    }
  }
}
