import Foundation

private struct DownloadPreferences: Decodable, Sendable {
  let adultConfirmed: Bool?
  let downloadWifiOnly: Bool?
  let downloadSiteConcurrency: Int?
  let downloadTaskConcurrency: Int?
  let downloadConcurrency: Int?
  let downloadDirectory: String?

  func limit(_ value: Int?, default fallback: Int) -> UInt32 {
    UInt32(max(1, min(8, value ?? fallback)))
  }
}

struct DownloadID: Hashable, Sendable {
  let source: String
  let work: String
}

extension ProgressSession {
  func downloadDirectory(configured: String?) throws -> URL {
    let owned = directory.appendingPathComponent("downloads", isDirectory: true)
    #if os(iOS)
      var root = owned
      try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
      var values = URLResourceValues()
      values.isExcludedFromBackup = true
      try root.setResourceValues(values)
      return root
    #else
      guard let configured, !configured.isEmpty else { return owned }
      guard configured.hasPrefix("/") else { throw SiteReadError.malformed }
      return URL(fileURLWithPath: configured, isDirectory: true)
    #endif
  }

  public func enqueueDownload(
    work: LibraryWork, detail: SiteWork, chapters: [OfflineChapter],
    lines: VideoLines? = nil, index: UInt32? = nil
  ) async throws {
    guard work.workId == detail.id, !chapters.isEmpty,
      chapters.allSatisfy({ work.chapters.contains($0) }) else { throw SiteReadError.malformed }
    let settings = try await preferences(DownloadPreferences.self)
    guard settings.adultConfirmed == true else { throw SiteReadError.malformed }
    let root = try downloadDirectory(configured: settings.downloadDirectory)
    let book = DownloadBook(
      source: work.source, work: work.workId, title: work.title, media: work.media,
      domain: work.contentDomain ?? "", cover: detail.cover,
      singleChapterId: detail.chapters?.isEmpty != false ? work.chapters.first?.id : nil)
    try await run { store, _ in
      try store.enqueueDownload(book: book, chapters: chapters, directory: root.path, lines: lines, index: index)
    }
  }

  public func setDownloadQueued(_ work: LibraryWork, resume: Bool) async throws {
    try await run { store, _ in
      try store.setDownloadQueued(source: work.source, work: work.workId, resume: resume)
    }
    if !resume {
      let id = DownloadID(source: work.source, work: work.workId)
      downloadRequests[id]?.values.forEach { $0.cancel() }
    }
  }

  private func download(
    _ library: SourceLibrary, id: DownloadID, sites: UInt32, works: UInt32, pages: UInt32
  ) async throws -> Bool {
    let token = UUID(), request = Request(), root = directory
    downloadRequests[id, default: [:]][token] = request
    defer {
      downloadRequests[id]?.removeValue(forKey: token)
      if downloadRequests[id]?.isEmpty == true { downloadRequests.removeValue(forKey: id) }
    }
    return try await run(request: request) { store, request in
      try library.service(directory: root, request: request).runDownload(
        store: store, source: id.source, work: id.work, sites: sites, works: works, pages: pages,
        request: request)
    }
  }

  /// 一次唤醒持续到队列清空。下载文件、断点及跨窗口并发额度均由核心管理。
  public func processDownloads(
    _ library: SourceLibrary, wifi: Bool, changed: @escaping @Sendable () async -> Void
  ) async throws {
    let settings = try await preferences(DownloadPreferences.self)
    guard settings.adultConfirmed == true, wifi || settings.downloadWifiOnly != true else { return }
    let sites = settings.limit(settings.downloadSiteConcurrency, default: 3)
    let works = settings.limit(settings.downloadTaskConcurrency, default: 1)
    let pages = settings.limit(settings.downloadConcurrency, default: 3)
    var failed = Set<DownloadID>()
    while true {
      try Task.checkCancellation()
      let pending = try await libraryWorks().compactMap { work -> DownloadID? in
        guard !work.invalidDownload, work.pendingDeletion == nil,
          work.downloadStatus == 0 || work.downloadStatus == 1 else { return nil }
        return DownloadID(source: work.source, work: work.workId)
      }
      let ready = pending.filter { !failed.contains($0) }
      guard !ready.isEmpty else {
        if !pending.isEmpty { throw SiteReadError.malformed }
        return
      }
      // 先轮询一次所有作品，让另一站点可以利用空闲的站点额度。
      var ran = false
      try await withThrowingTaskGroup(of: (DownloadID, Bool?).self) { group in
        var remaining = ready.makeIterator()
        func submit(_ id: DownloadID) {
          group.addTask {
            do {
              let done = try await self.download(library, id: id, sites: sites, works: works, pages: pages)
              return (id, done)
            } catch where Task.isCancelled { throw CancellationError() }
            catch { return (id, nil) }
          }
        }
        for _ in 0..<Int(sites * works) {
          guard let id = remaining.next() else { break }
          submit(id)
        }
        while let (id, result) = try await group.next() {
          try Task.checkCancellation()
          if result == nil { failed.insert(id) }
          if result != false { ran = true; await changed() }
          if let next = remaining.next() { submit(next) }
        }
      }
      if !ran { try await Task.sleep(nanoseconds: 700_000_000) }
    }
  }
}
