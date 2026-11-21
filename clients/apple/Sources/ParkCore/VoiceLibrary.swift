import Foundation

public struct VoiceNode: Decodable, Identifiable, Sendable {
  public let id: String
  public let name: String
  public let type: String
  let children: [VoiceNode]?
  let childrenComplete: Bool?
  let subtitleIds: [String]?
  public var playable: Bool { type == "audio" || type == "video" }

  init(id: String, name: String, type: String, subtitleIds: [String] = []) {
    self.id = id
    self.name = name
    self.type = type
    self.subtitleIds = subtitleIds
    children = nil
    childrenComplete = nil
  }
}

public struct VoiceFolder: Sendable {
  public let entries: [VoiceNode]
  public let nextPage: Int?

  public func subtitles(for track: VoiceNode) -> [VoiceNode] {
    func stem(_ name: String, extensions: Set<String>) -> String? {
      guard !name.contains("/"), !name.contains("\\"), let dot = name.lastIndex(of: "."),
        dot > name.startIndex, extensions.contains(name[name.index(after: dot)...].lowercased())
      else { return nil }
      return name[..<dot].lowercased()
    }
    let media = stem(track.name, extensions: ["mp3","flac","wav","m4a","aac","ogg","opus","mp4","m4v","mkv","webm","m3u8"])
    let captions = entries.filter { $0.type == "subtitle" }
    var seen = Set<String>()
    let byId = Dictionary(uniqueKeysWithValues: captions.map { ($0.id, $0) })
    let explicit = (track.subtitleIds ?? []).compactMap { byId[$0] }
    return (explicit + captions.filter { media != nil && stem($0.name, extensions: ["vtt","srt","lrc"]) == media })
      .filter { seen.insert($0.id).inserted }
  }
}

private extension SiteWork {
  func voiceEntries() -> [VoiceNode] {
    let single = singleChapterId.flatMap { $0.isEmpty ? nil : $0 } ?? "_"
    let rows = chapters.flatMap { $0.isEmpty ? nil : $0 }
      ?? [SiteWork.Chapter(id: single, title: title)]
    let tracks = rows.map { chapter in
      let fallback = voiceMediaForm == "videoOnly" ? "video" : "audio"
      let kind = voiceTrackKinds?[chapter.id] ?? fallback
      return VoiceNode(id: chapter.id, name: chapter.title ?? chapter.id,
        type: kind == "video" || kind == "audio" ? kind : fallback,
        subtitleIds: voiceSubtitles?[chapter.id] ?? [])
    }
    let items = tracks + tracks.flatMap { $0.subtitleIds ?? [] }.map { VoiceNode(id: $0, name: $0, type: "subtitle") }
    var seen = Set<String>()
    return items.filter { !$0.id.isEmpty && seen.insert($0.id).inserted }
  }
}

extension ProgressSession {
  public func storedVoiceFolder(_ key: ChapterKey, title: String) async throws -> VoiceFolder {
    let raw = try await run { store, _ in try store.storedWork(source: key.source, work: key.workId) }
    if let raw {
      let work = try JSONDecoder().decode(SiteWork.self, from: Data(raw.utf8))
      if work.id == key.workId, work.chapters?.contains(where: { $0.id == key.chapterId }) == true {
        return VoiceFolder(entries: work.voiceEntries(), nextPage: nil)
      }
    }
    return VoiceFolder(entries: [VoiceNode(id: key.chapterId, name: title, type: "audio")], nextPage: nil)
  }

  public func voiceFolder(
    _ library: SourceLibrary, site: String, work: SiteWork, folder: VoiceNode?,
    after previous: VoiceFolder? = nil, confirmation: ManualRead? = nil
  ) async throws -> VoiceFolder {
    let page = previous?.nextPage ?? 1
    guard previous == nil || previous?.nextPage != nil else { throw SiteReadError.malformed }
    let nodes: [VoiceNode]
    var more = false
    if let folder {
      if folder.type != "directory" {
        nodes = [folder]
      } else if let children = folder.children, folder.childrenComplete != false {
        nodes = children
      } else {
        let args: [Any] = [work.id, folder.id, ["page": page]]
        let raw = try await siteRead(library, site: site, method: .voiceDirectory,
          arguments: String(decoding: try JSONSerialization.data(withJSONObject: args), as: UTF8.self),
          confirmation: confirmation)
        struct Response: Decodable { let items: [VoiceNode]; let hasNext: Bool }
        let section = try JSONDecoder().decode(Response.self, from: Data(raw.utf8))
        guard section.items.count <= 1000 else { throw SiteReadError.malformed }
        nodes = section.items
        more = section.hasNext
      }
    } else {
      nodes = work.voiceEntries()
    }
    var entries = previous?.entries ?? []
    var seen = Set(entries.map(\.id))
    let count = entries.count
    for node in nodes {
      guard !node.id.isEmpty, !node.name.isEmpty else { throw SiteReadError.malformed }
      if seen.insert(node.id).inserted { entries.append(node) }
    }
    guard entries.count <= 10000, !more || (entries.count > count && page < 1000) else { throw SiteReadError.malformed }
    return VoiceFolder(entries: entries, nextPage: more ? page + 1 : nil)
  }

  public func selectVoiceTrack(work: SiteWork, site: String, folder: VoiceFolder, track: String, incognito: Bool) async throws -> ChapterKey {
    let tracks = folder.entries.filter(\.playable)
    guard tracks.contains(where: { $0.id == track }) else { throw SiteReadError.malformed }
    if !incognito {
      let summary: [String: Any] = [
        "id": work.id, "title": work.title, "mediaKind": "voice", "chaptersComplete": false,
        "chapters": tracks.map { ["id": $0.id, "title": $0.name] },
        "voiceTrackKinds": Dictionary(uniqueKeysWithValues: tracks.map { ($0.id, $0.type) }),
        "voiceSubtitles": Dictionary(uniqueKeysWithValues: tracks.map { ($0.id, folder.subtitles(for: $0).map(\.id)) })
      ]
      let json = String(decoding: try JSONSerialization.data(withJSONObject: summary), as: UTF8.self)
      _ = try await run { store, _ in try store.saveWorkSummary(source: site, summary: json, favorite: nil) }
    }
    return ChapterKey(source: site, workId: work.id, chapterId: track)
  }

  public func voiceCues(_ library: SourceLibrary?, key: ChapterKey, node: String) async throws -> [VoiceCue] {
    guard let library else {
      return try await run { store, _ in try store.offlineSubtitles(key: key, node: node) }
    }
    let root = directory
    return try await run { _, request in
      try library.service(directory: root, request: request).voiceSubtitles(
        source: key.source, work: key.workId, node: node, request: request)
    }
  }
}

/// 核心以整秒保存播放位置。流媒体尚无时长时仍可记录已播放的秒数。
public func playbackSeconds(_ value: Double, duration: Int64? = nil) -> Int64? {
  guard value >= 0, let seconds = Int64(exactly: value.rounded(.down)) else { return nil }
  if let duration, duration > 0 { return min(seconds, duration) }
  return seconds
}
