import Foundation

private actor ExportDirectories {
  static let shared = ExportDirectories()

  func roots(current: URL, library: URL, remember: Bool) throws -> Set<URL> {
    #if os(iOS)
      return [current]
    #else
      let registry = library.appendingPathComponent("export_directories.json")
      let saved = FileManager.default.fileExists(atPath: registry.path)
        ? try JSONDecoder().decode([String].self, from: Data(contentsOf: registry)) : []
      guard saved.allSatisfy({ $0.hasPrefix("/") }) else { throw SiteReadError.malformed }
      var roots = Set(saved.map { URL(fileURLWithPath: $0, isDirectory: true).standardizedFileURL })
      let inserted = roots.insert(current.standardizedFileURL).inserted
      if remember && inserted {
        try JSONEncoder().encode(roots.map(\.path).sorted()).write(to: registry, options: .atomic)
      }
      return roots
    #endif
  }
}

private struct DownloadPreferences: Decodable, Sendable {
  let adultConfirmed: Bool?
  let downloadWifiOnly: Bool?
  let downloadSiteConcurrency: Int?
  let downloadTaskConcurrency: Int?
  let downloadConcurrency: Int?
  let downloadDirectory: String?
  let pdfPagesPerFile: Int32?
  let language: String?

  func limit(_ value: Int?, default fallback: Int) -> UInt32 {
    UInt32(max(1, min(8, value ?? fallback)))
  }
}

public struct LibraryWorkID: Hashable, Sendable {
  let source: String
  let work: String
}

public extension LibraryWork {
  var libraryID: LibraryWorkID { LibraryWorkID(source: source, work: workId) }
}

public enum DownloadLimit: String, CaseIterable, Sendable {
  case sites = "downloadSiteConcurrency"
  case works = "downloadTaskConcurrency"
  case pages = "downloadConcurrency"
}

extension ProgressSession {
  public func downloadLimits() async throws -> [DownloadLimit: Int] {
    let settings = try await preferences(DownloadPreferences.self)
    return [
      .sites: Int(settings.limit(settings.downloadSiteConcurrency, default: 3)),
      .works: Int(settings.limit(settings.downloadTaskConcurrency, default: 1)),
      .pages: Int(settings.limit(settings.downloadConcurrency, default: 3)),
    ]
  }

  public func setDownloadLimit(_ limit: DownloadLimit, to value: Int) async throws {
    guard (1...8).contains(value) else { throw CocoaError(.coderInvalidValue) }
    try await run { store, _ in try store.savePreference(name: limit.rawValue, json: String(value)) }
  }

  public func comicPdfLimit() async throws -> Int32 {
    try await preferences(DownloadPreferences.self).pdfPagesPerFile ?? 300
  }

  public func importComic(from url: URL, chapterTitle: String) async throws {
    guard url.isFileURL, ["cbz", "zip"].contains(url.pathExtension.lowercased()) else {
      throw SiteReadError.malformed
    }
    let settings = try await preferences(DownloadPreferences.self)
    let destination = try downloadDirectory(configured: settings.downloadDirectory)
    try await run { store, request in
      _ = try store.importComic(archivePath: url.path, filename: url.lastPathComponent,
        directory: destination.path, chapterTitle: chapterTitle, request: request)
    }
  }

  public func exportComic(_ work: LibraryWork, pdfPages: Int32?) async throws -> [URL] {
    let settings = try await preferences(DownloadPreferences.self)
    guard work.downloaded, !work.invalidDownload, work.media == "comic", !work.chapters.isEmpty else {
      throw SiteReadError.malformed
    }
    let output = try downloadDirectory(configured: settings.downloadDirectory).appendingPathComponent("exports", isDirectory: true)
    _ = try await ExportDirectories.shared.roots(current: output, library: directory, remember: true)
    let cache = try FileManager.default.url(for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
    let locale = settings.language.flatMap { $0.isEmpty ? nil : $0 } ?? Locale.preferredLanguages.first ?? "en"
    return try await run { store, request in
      if let pages = pdfPages {
        guard (0...5000).contains(pages) else { throw SiteReadError.malformed }
        try store.savePreference(name: "pdfPagesPerFile", json: String(pages))
      }
      return try store.exportComic(source: work.source, workId: work.workId, format: pdfPages == nil ? .cbz : .pdf,
        maxPages: pdfPages ?? 0, directory: output.path, cacheDirectory: cache.path, locale: locale,
        request: request).map { output.appendingPathComponent($0) }
    }
  }

  public func exportedComics() async throws -> [URL] {
    let settings = try await preferences(DownloadPreferences.self)
    let output = try downloadDirectory(configured: settings.downloadDirectory).appendingPathComponent("exports", isDirectory: true)
    let files = FileManager.default
    let roots = try await ExportDirectories.shared.roots(current: output, library: directory, remember: false)
    let keys: [URLResourceKey] = [.isRegularFileKey, .isSymbolicLinkKey, .contentModificationDateKey]
    var entries: [URL] = []
    for root in roots where files.fileExists(atPath: root.path) {
      guard try root.resourceValues(forKeys: [.isSymbolicLinkKey]).isSymbolicLink != true else { continue }
      entries += try files.contentsOfDirectory(at: root, includingPropertiesForKeys: keys, options: .skipsHiddenFiles)
    }
    return try entries.compactMap { url -> (URL, Date)? in
      let values = try url.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey, .contentModificationDateKey])
      guard values.isRegularFile == true, values.isSymbolicLink != true else { return nil }
      return (url, values.contentModificationDate ?? .distantPast)
    }.sorted { $0.1 == $1.1 ? $0.0.path < $1.0.path : $0.1 > $1.1 }.map(\.0)
  }

  public func removeExportedComic(_ selected: URL, modified: Date, size: Int) async throws {
    guard ["pdf", "cbz"].contains(selected.pathExtension.lowercased()),
          try await exportedComics().contains(selected) else { throw SiteReadError.malformed }
    let current = URL(fileURLWithPath: selected.path)
    let values = try current.resourceValues(forKeys: [.contentModificationDateKey, .fileSizeKey, .isSymbolicLinkKey])
    guard values.isSymbolicLink != true, values.contentModificationDate == modified, values.fileSize == size else {
      throw SiteReadError.malformed
    }
    try FileManager.default.removeItem(at: current)
  }

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

  public func enqueueGame(_ links: GameLinks, title: String) async throws {
    let settings = try await preferences(DownloadPreferences.self)
    guard settings.adultConfirmed == true else { throw SiteReadError.malformed }
    let root = try downloadDirectory(configured: settings.downloadDirectory)
    try await run { store, _ in
      _ = try links.enqueueDownload(store: store, directory: root.path, title: title)
    }
  }

  public func videoDownloadInfo(_ work: LibraryWork) async throws -> DownloadRowInfo {
    guard work.media == "video", work.downloaded else { throw SiteReadError.malformed }
    return try await run { store, _ in try store.downloadRowInfo(source: work.source, work: work.workId) }
  }

  public func chooseVideoDownloadLine(key: ChapterKey, lines: VideoLines, index: UInt32) async throws {
    try await run { store, _ in try store.chooseVideoDownloadLine(key: key, lines: lines, index: index) }
  }

  public func exportedVideoEpisode(_ key: ChapterKey) async throws -> URL {
    let folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
    try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
    do {
      let file = try await run { store, request in
        try store.exportVideoEpisode(key: key, destination: folder.path, request: request)
      }
      try FileManager.default.setAttributes([.posixPermissions: 0o400], ofItemAtPath: file)
      return URL(fileURLWithPath: file)
    } catch {
      try? FileManager.default.removeItem(at: folder)
      throw error
    }
  }

  public func changeQueue(_ works: [LibraryWork], resume: Bool) async throws {
    let targets = works.map { DownloadTarget(source: $0.source, work: $0.workId) }
    let changed = try await run { store, _ in try store.changeDownloadQueue(targets: targets, resume: resume) }
    if !resume {
      for target in changed {
        downloadRequests[LibraryWorkID(source: target.source, work: target.work)]?.values.forEach { $0.cancel() }
      }
    }
  }

  public func setDownloadQueued(_ work: LibraryWork, resume: Bool) async throws {
    try await run { store, _ in
      try store.setDownloadQueued(source: work.source, work: work.workId, resume: resume)
    }
    if !resume {
      let id = work.libraryID
      downloadRequests[id]?.values.forEach { $0.cancel() }
    }
  }

  public func removeDownload(_ work: LibraryWork) async throws {
    downloadRequests[work.libraryID]?.values.forEach { $0.cancel() }
    try await run { store, _ in
      _ = try store.deleteDownload(source: work.source, workId: work.workId, chapters: [])
    }
  }

  public func removeDownloadedChapters(_ ids: Set<String>, from work: LibraryWork) async throws {
    guard !ids.isEmpty, work.downloaded, !work.invalidDownload,
          ids.isSubset(of: Set(work.chapters.map(\.id)).union(work.pendingDeletion ?? [])) else {
      throw CocoaError(.coderInvalidValue)
    }
    let ordered = ids.sorted()
    try await run { store, _ in
      _ = try store.deleteDownload(source: work.source, workId: work.workId, chapters: ordered)
    }
  }

  public func retryDownloadRecord(_ work: LibraryWork) async throws {
    try await run { store, _ in
      _ = try store.downloadChapters(source: work.source, work: work.workId)
    }
  }

  public func removeInvalidDownloadRecord(_ work: LibraryWork) async throws {
    try await run { store, _ in
      try store.removeInvalidDownload(source: work.source, work: work.workId)
    }
  }

  private func download(
    _ library: SourceLibrary, id: LibraryWorkID, sites: UInt32, works: UInt32, pages: UInt32
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
    _ library: SourceLibrary, wifi: Bool, gameGrant: @escaping @Sendable ([String]) async -> GameLinks? = { _ in nil },
    observeQueue: (@Sendable () async -> AsyncStream<Void>)? = nil,
    changed: @escaping @Sendable () async -> Void
  ) async throws {
    let settings = try await preferences(DownloadPreferences.self)
    guard settings.adultConfirmed == true, wifi || settings.downloadWifiOnly != true else { return }
    let sites = settings.limit(settings.downloadSiteConcurrency, default: 3)
    let works = settings.limit(settings.downloadTaskConcurrency, default: 1)
    let pages = settings.limit(settings.downloadConcurrency, default: 3)
    var failed = Set<LibraryWorkID>()
    var failedGames = Set<[String]>()
    func pendingWorks() async throws -> [LibraryWorkID] {
      try await libraryWorks().compactMap { work -> LibraryWorkID? in
        guard !work.invalidDownload, work.pendingDeletion == nil,
          work.downloadStatus == 0 || work.downloadStatus == 1 else { return nil }
        return work.libraryID
      }
    }
    while true {
      try Task.checkCancellation()
      let changes = await observeQueue?()
      let pending = try await pendingWorks()
      let ready = pending.filter { !failed.contains($0) }
      let games: [GameDownloadRecord]
      do { games = try await gameDownloads().filter { $0.status == "queued" && !failedGames.contains($0.key) } }
      catch {
        try Task.checkCancellation()
        if ready.isEmpty { throw error }
        games = []
      }
      if let game = games.first {
        do { try await transferGame(library, key: game.key, grant: await gameGrant(game.key)) }
        catch where Task.isCancelled { throw CancellationError() }
        catch { failedGames.insert(game.key) }
        await changed()
        if ready.isEmpty { continue }
      }
      guard !ready.isEmpty else {
        if !pending.isEmpty { throw SiteReadError.malformed }
        return
      }
      // 先轮询一次所有作品，让另一站点可以利用空闲的站点额度。
      var ran = false
      try await withThrowingTaskGroup(of: DownloadEvent.self) { group in
        var remaining = ready[...]
        var scheduled = Set(ready)
        var running = 0
        func submit(_ id: LibraryWorkID) {
          running += 1
          group.addTask {
            do {
              let done = try await self.download(library, id: id, sites: sites, works: works, pages: pages)
              return .finished(id, done)
            } catch where Task.isCancelled { throw CancellationError() }
            catch { return .finished(id, nil) }
          }
        }
        func listen() {
          guard let changes else { return }
          group.addTask {
            for await _ in changes { return .queueChanged }
            return .closed
          }
        }
        listen()
        while true {
          while running < Int(sites * works), let id = remaining.popFirst() { submit(id) }
          guard running > 0, let event = try await group.next() else { break }
          try Task.checkCancellation()
          switch event {
          case .finished(let id, let result):
            running -= 1
            if result == nil { failed.insert(id) }
            if result != false { ran = true; await changed() }
          case .queueChanged:
            for id in try await pendingWorks() where !failed.contains(id) && scheduled.insert(id).inserted {
              remaining.append(id)
            }
            listen()
          case .closed: break
          }
        }
        group.cancelAll()
      }
      if !ran { try await Task.sleep(nanoseconds: 700_000_000) }
    }
  }
}

private enum DownloadEvent: Sendable {
  case queueChanged, closed
  case finished(LibraryWorkID, Bool?)
}
