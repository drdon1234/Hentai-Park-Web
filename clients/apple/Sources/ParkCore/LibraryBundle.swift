import Foundation

public struct LibraryBundle: Sendable {
  public let key: ChapterKey
  public let contents: BundleContents
  public let selected: String?

  public func siblings(of entry: BundleEntry) -> VoiceFolder {
    var seen = Set<String>()
    let nodes = contents.entries.filter { $0.parent == entry.parent && seen.insert($0.id).inserted }
      .map { VoiceNode(id: $0.id, name: $0.name, type: $0.kind, subtitleIds: $0.subtitles) }
    return VoiceFolder(entries: nodes, nextPage: nil)
  }
}

extension ProgressSession {
  public func openBundle(work: LibraryWork, chapter: String) async throws -> LibraryBundle {
    if work.chapters.contains(where: { $0.id == chapter }) {
      let key = ChapterKey(source: work.source, workId: work.workId, chapterId: chapter)
      let contents = try await run { store, _ in try store.offlineBundle(key: key) }
      return LibraryBundle(key: key, contents: contents, selected: nil)
    }
    // 历史播放位置按声轨记录。下载任务按整部作品或所选目录记录。
    for section in work.chapters {
      try Task.checkCancellation()
      let key = ChapterKey(source: work.source, workId: work.workId, chapterId: section.id)
      let book: BundleContents
      do { book = try await run { store, _ in try store.offlineBundle(key: key) } }
      catch StorageError.UnavailableDownload { continue }
      if book.kind == "voice", book.entries.contains(where: {
        $0.id == chapter && ($0.kind == "audio" || $0.kind == "video")
      }) { return LibraryBundle(key: key, contents: book, selected: chapter) }
    }
    throw StorageError.UnavailableDownload
  }

  public func bundleMedia(_ key: ChapterKey, node: String) async throws -> PlaybackSession {
    try await run { store, request in try store.offlineBundleMedia(key: key, node: node, request: request) }
  }

  public func bundleAsset(_ key: ChapterKey, node: String) async throws -> URL {
    try await run { store, _ in URL(fileURLWithPath: try store.bundleFile(key: key, id: node).path) }
  }
}
