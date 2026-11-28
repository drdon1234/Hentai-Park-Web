import Network
import ParkCore
import SwiftUI
import UniformTypeIdentifiers
#if os(iOS)
  import UIKit
#endif

@MainActor final class DownloadQueue: ObservableObject {
  let exports = ComicExportJobs()
  let recordings = BroadcastRecorder()
  @Published private(set) var revision = 0
  @Published private(set) var failed = false
  private var windows = Set<UUID>()
  private var monitor: NWPathMonitor?
  private var wifi: Bool?
  private var foreground = true
  private var requested = false
  private var worker: Task<Void, Never>?
  private var session: ProgressSession?
  private var gameGrants: [[String]: GameLinks] = [:]
  #if os(iOS)
    private var allowance = UIBackgroundTaskIdentifier.invalid
  #endif

  func attach(_ window: UUID) {
    guard windows.insert(window).inserted, monitor == nil else { return }
    let monitor = NWPathMonitor()
    self.monitor = monitor
    monitor.pathUpdateHandler = { [weak self, weak monitor] path in
      let network = path.status == .satisfied ? path.usesInterfaceType(.wifi) : nil
      Task { @MainActor in
        guard let self, let monitor, self.monitor === monitor, self.wifi != network else { return }
        self.wifi = network
        self.worker?.cancel()
        self.refresh()
      }
    }
    monitor.start(queue: DispatchQueue(label: "park.download.network"))
  }

  func detach(_ window: UUID) {
    windows.remove(window)
    guard windows.isEmpty else { return }
    recordings.stopAll()
    monitor?.cancel()
    monitor = nil
    wifi = nil
    requested = false
    worker?.cancel()
  }

  func transition(_ phase: ScenePhase) {
    if phase == .active {
      foreground = true
      refresh()
    } else if phase == .background {
      foreground = false
      #if os(iOS)
        if allowance == .invalid { worker?.cancel() }
      #endif
    }
  }

  func toggle(_ work: LibraryWork, resume: Bool, fallback: ProgressSession) async {
    do {
      try await (session ?? fallback).setDownloadQueued(work, resume: resume)
      revision += 1
      refresh()
    } catch { failed = true }
  }

  func change(_ works: [LibraryWork], resume: Bool, fallback: ProgressSession) async throws {
    try await (session ?? fallback).changeQueue(works, resume: resume)
    revision += 1
    refresh()
  }

  func refresh() {
    requested = true
    startIfNeeded()
  }

  func importComic(_ url: URL, chapterTitle: String, fallback: ProgressSession) async throws {
    let scoped = url.startAccessingSecurityScopedResource()
    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
    try await (session ?? fallback).importComic(from: url, chapterTitle: chapterTitle)
    revision += 1
  }

  func remove(_ work: LibraryWork, fallback: ProgressSession) async throws {
    try await (session ?? fallback).removeDownload(work)
    revision += 1
  }

  func recoverRecord(_ work: LibraryWork, remove: Bool, fallback: ProgressSession) async throws {
    let library = session ?? fallback
    defer { revision += 1 }
    if remove { try await library.removeInvalidDownloadRecord(work) }
    else {
      try await library.retryDownloadRecord(work)
      refresh()
    }
  }

  func removeRead(fallback: ProgressSession) async throws -> UInt64 {
    let count = try await (session ?? fallback).deleteReadDownloads()
    if count > 0 { revision += 1 }
    return count
  }

  func didEnqueue() {
    revision += 1
    refresh()
  }

  func enqueueGame(_ resolved: ResolvedGameResource, key: [String], title: String, fallback: ProgressSession) async throws {
    let grant = resolved.links
    gameGrants[key] = grant
    do { try await (session ?? fallback).enqueueGame(grant, title: title) }
    catch {
      if gameGrants[key] === grant { gameGrants.removeValue(forKey: key) }
      throw error
    }
    Task { [weak self, weak grant] in
      try? await Task.sleep(nanoseconds: 120_000_000_000)
      if let self, let grant, self.gameGrants[key] === grant { self.gameGrants.removeValue(forKey: key) }
    }
    didEnqueue()
  }

  func changeGame(_ item: GameDownloadRecord, action: String, fallback: ProgressSession) async throws {
    try await (session ?? fallback).changeGameDownload(item.key, action: action)
    if action != "resume" { gameGrants.removeValue(forKey: item.key) }
    didEnqueue()
  }

  private func startIfNeeded() {
    guard requested, !windows.isEmpty, foreground, let wifi, worker == nil,
      let library = InstalledSources.library else { return }
    requested = false
    failed = false
    #if os(iOS)
      allowance = UIApplication.shared.beginBackgroundTask(withName: "Download") { [weak self] in
        self?.worker?.cancel()
        self?.endAllowance()
      }
    #endif
    worker = Task {
      do {
        let opened = try await ProgressSession.openApplicationLibrary()
        session = opened
        try await opened.processDownloads(library, wifi: wifi, gameGrant: { key in
          await self.takeGameGrant(key)
        }) {
          await self.downloadChanged()
        }
      } catch { if !Task.isCancelled { failed = true } }
      await session?.close()
      session = nil
      worker = nil
      #if os(iOS)
        endAllowance()
      #endif
      startIfNeeded()
    }
  }

  private func downloadChanged() { revision += 1 }
  private func takeGameGrant(_ key: [String]) -> GameLinks? { gameGrants.removeValue(forKey: key) }

  #if os(iOS)
    private func endAllowance() {
      if allowance != .invalid { UIApplication.shared.endBackgroundTask(allowance) }
      allowance = .invalid
    }
  #endif
}

@MainActor final class DownloadSelection: ObservableObject {
  @Published var status: Int32?
  @Published var kind: String?
  @Published private var siteKinds: [String: String] = [:]
  @Published var selecting = false
  @Published var selected = Set<LibraryWorkID>()

  func rows(_ works: [LibraryWork]) -> [LibraryWork] {
    works.filter { work in
      work.downloaded && !work.invalidDownload && (status == nil || work.downloadStatus == status)
        && (kind == nil || kind == (work.itemKind ?? siteKinds[work.source]))
    }
  }

  func loadSiteKinds(_ session: ProgressSession, sources: Set<String>) async throws {
    guard let library = InstalledSources.library else { return }
    let kinds = try await session.librarySourceKinds(library, sources: sources)
    try Task.checkCancellation()
    siteKinds = sources.reduce(into: [:]) { $0[$1] = kinds[$1] ?? "comic" }
  }

  func finish() {
    selected.removeAll()
    selecting = false
  }
}

@MainActor struct InvalidDownloadRow: View {
  let work: LibraryWork
  let session: ProgressSession
  let tr: (String) -> String
  @EnvironmentObject private var queue: DownloadQueue
  @State private var confirming = false
  @State private var busy = false
  @State private var error: String?

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      Text(tr("下载记录恢复失败")).font(.headline)
      Text(work.source)
      Text(tr("原始记录和本地文件已保留，可以重试或仅移除记录。"))
      if let error { Text(tr(error)).foregroundStyle(.secondary) }
      HStack {
        Button(tr("重试")) { recover(remove: false) }
        Button(tr("移除记录"), role: .destructive) { confirming = true }
      }.buttonStyle(.borderless).disabled(busy)
    }
    .alert(tr("移除损坏的下载记录？"), isPresented: $confirming) {
      Button(tr("取消"), role: .cancel) {}
      Button(tr("移除记录"), role: .destructive) { recover(remove: true) }
    } message: { Text(tr("只移除无法恢复的下载记录，保留本地文件。")) }
  }

  private func recover(remove: Bool) {
    guard !busy else { return }
    busy = true
    error = nil
    Task {
      defer { busy = false }
      do {
        try await queue.recoverRecord(work, remove: remove, fallback: session)
      } catch { self.error = remove ? "删除失败，请重试" : "下载记录恢复失败，请前往下载页处理" }
    }
  }
}

@MainActor struct DownloadControls: View {
  @ObservedObject var selection: DownloadSelection
  @EnvironmentObject private var queue: DownloadQueue
  let works: [LibraryWork]
  let allWorks: [LibraryWork]
  let untypedSources: Set<String>
  let session: ProgressSession
  let tr: (String) -> String
  @State private var confirming = false
  @State private var targets: [LibraryWork] = []
  @State private var busy = false
  @State private var failed = false
  @State private var catalogFailed = false
  @State private var catalogRetry = 0
  @State private var showLimits = false
  @State private var showImport = false
  @State private var importMessage: String?
  let showExports: () -> Void
  let manageCategories: () -> Void

  var body: some View {
    Section {
      Picker(tr("媒体类型"), selection: $selection.kind) {
        Text(tr("全部")).tag(nil as String?)
        ForEach([("comic", "漫画"), ("video", "视频"), ("voice", "音声"), ("novel", "小说"), ("tavern", "酒馆"),
                 ("game", "游戏"), ("article", "吃瓜"), ("mixed", "混合"), ("unknown", "未知")], id: \.0) { kind, label in
          Text(tr(label)).tag(Optional(kind))
        }
      }.pickerStyle(.menu).onChange(of: selection.kind) { _ in selection.finish() }
      Picker(tr("下载状态"), selection: $selection.status) {
        Text(tr("全部")).tag(nil as Int32?)
        ForEach(Array(["等待下载", "下载中", "已暂停", "下载完成", "下载失败"].enumerated()), id: \.offset) { pair in
          Text(tr(pair.element)).tag(Optional(Int32(pair.offset)))
        }
      }.pickerStyle(.menu).onChange(of: selection.status) { _ in selection.finish() }
      if selection.selecting {
        queueButton(resume: false, selected: true)
        queueButton(resume: true, selected: true)
        Button(tr("删除下载")) {
          targets = works.filter { selection.selected.contains($0.libraryID) }
          confirming = !targets.isEmpty
        }.disabled(selection.selected.isEmpty)
        Button(tr("取消")) { selection.finish(); failed = false }
      } else {
        Button(tr("批量选择")) { selection.selecting = true }.disabled(works.isEmpty)
        queueButton(resume: false, selected: false)
        queueButton(resume: true, selected: false)
        Button(tr("下载并发设置")) { showLimits = true }
          .sheet(isPresented: $showLimits) {
            DownloadLimitSettings(session: session, tr: tr, changed: queue.refresh)
          }
        Button(tr("导出文件"), action: showExports)
        Button(tr("导入本地漫画")) { showImport = true }
          .fileImporter(isPresented: $showImport, allowedContentTypes: [.zip, UTType(filenameExtension: "cbz") ?? .archive]) { result in
            switch result {
            case .failure(let error):
              if (error as NSError).domain != NSCocoaErrorDomain || (error as NSError).code != NSUserCancelledError {
                importMessage = "无法读取文件，请重试"
              }
            case .success(let url):
              busy = true
              importMessage = nil
              Task {
                defer { busy = false }
                do {
                  try await queue.importComic(url, chapterTitle: tr("全本"), fallback: session)
                  importMessage = "已导入"
                } catch { importMessage = "导入失败，请检查文件格式" }
              }
            }
          }
        if let importMessage { Text(tr(importMessage)) }
        Button(tr("管理分类"), action: manageCategories)
      }
      if failed { Text(tr("操作失败，请重试")) }
      if catalogFailed {
        Text(tr("站点加载失败，请重试"))
        Button(tr("重试")) { catalogRetry += 1 }
      }
    }
    .task(id: CatalogRequest(sources: selection.kind == nil ? [] : untypedSources, retry: catalogRetry)) {
      catalogFailed = false
      guard selection.kind != nil, !untypedSources.isEmpty else { return }
      do { try await selection.loadSiteKinds(session, sources: untypedSources) }
      catch { if !Task.isCancelled { catalogFailed = true } }
    }
    .disabled(busy)
    .alert(tr("删除下载？"), isPresented: $confirming) {
      Button(tr("删除"), role: .destructive) {
        busy = true; failed = false
        Task {
          defer { busy = false }
          do {
            for work in targets {
              try await queue.remove(work, fallback: session)
              selection.selected.remove(work.libraryID)
            }
            selection.finish()
          } catch { failed = true }
        }
      }
      Button(tr("取消"), role: .cancel) {}
    } message: {
      Text(tr("从下载列表移除「{title}」并删除已下载内容？")
        .replacingOccurrences(of: "{title}", with: targets.map(\.title).joined(separator: "\n")))
    }
  }

  private func queueButton(resume: Bool, selected: Bool) -> some View {
    let scope = selected ? works.filter { selection.selected.contains($0.libraryID) } : allWorks
    let eligible = scope.filter { work in
      guard work.downloaded, !work.invalidDownload, work.pendingDeletion == nil, let state = work.downloadStatus else { return false }
      return resume ? state == 2 || state == 4 : state == 0 || state == 1
    }
    let label = selected ? (resume ? "继续下载" : "暂停下载") : (resume ? "全部继续" : "全部暂停")
    return Button(tr(label)) {
      guard !busy else { return }
      busy = true
      failed = false
      Task {
        defer { busy = false }
        do {
          try await queue.change(eligible, resume: resume, fallback: session)
          if selected { selection.finish() }
        } catch { failed = true }
      }
    }.disabled(busy || eligible.isEmpty)
  }

  private struct CatalogRequest: Equatable {
    let sources: Set<String>
    let retry: Int
  }
}

@MainActor struct DownloadCategoryButton: View {
  let work: LibraryWork
  let session: ProgressSession
  let tr: (String) -> String
  @State private var editing = false

  var body: some View {
    Button(tr("分类")) { editing = true }.buttonStyle(.borderless)
      .sheet(isPresented: $editing) { DownloadCategorySheet(work: work, session: session, tr: tr) }
  }
}

@MainActor private struct DownloadCategorySheet: View {
  let work: LibraryWork
  let session: ProgressSession
  let tr: (String) -> String
  @Environment(\.dismiss) private var dismiss
  @State private var groups: [LibraryCategory]?
  @State private var selected = Set<Int64>()
  @State private var saving = false
  @State private var failed = false
  @State private var reload = 0

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      Text(tr("分类")).font(.headline)
      Text(work.title)
      if let groups {
        if groups.isEmpty { Text(tr("还没有分类")) }
        ScrollView {
          VStack(alignment: .leading) {
            ForEach(groups, id: \.id) { row in
              Toggle(row.name, isOn: Binding(get: { selected.contains(row.id) }, set: { checked in
                if checked { selected.insert(row.id) } else { selected.remove(row.id) }
              }))
            }
          }
        }
      } else if !failed { ProgressView() }
      if failed {
        Text(tr("操作失败，请重试"))
        if groups == nil { Button(tr("重试")) { reload += 1 } }
      }
      HStack {
        Button(tr("取消")) { dismiss() }.keyboardShortcut(.cancelAction)
        Button(tr("确定")) {
          guard let groups, !saving else { return }
          let original = Set(groups.filter(\.selected).map(\.id))
          let added = Array(selected.subtracting(original)), removed = Array(original.subtracting(selected))
          if added.isEmpty && removed.isEmpty { dismiss(); return }
          saving = true
          Task {
            defer { saving = false }
            do { try await session.editDownloadCategories(work, added: added, removed: removed); dismiss() }
            catch { failed = true }
          }
        }.keyboardShortcut(.defaultAction).disabled(groups?.isEmpty != false)
      }
    }
    .padding().disabled(saving).interactiveDismissDisabled(saving)
    .task(id: reload) {
      do {
        let rows = try await session.downloadCategories(work)
        try Task.checkCancellation()
        selected = Set(rows.filter(\.selected).map(\.id)); groups = rows; failed = false
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}

@MainActor private struct DownloadLimitSettings: View {
  let session: ProgressSession
  let tr: (String) -> String
  let changed: () -> Void
  @Environment(\.dismiss) private var dismiss
  @State private var values: [DownloadLimit: Int] = [:]
  @State private var saving = false
  @State private var failed = false
  @State private var reload = 0

  private func label(_ limit: DownloadLimit) -> String {
    switch limit {
    case .sites: return tr("同时下载站点数")
    case .works: return tr("每站同时下载作品数")
    case .pages: return tr("每个下载任务的并发页数")
    }
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      Text(tr("下载并发设置")).font(.headline)
      if values.isEmpty && !failed { ProgressView() }
      ForEach(DownloadLimit.allCases, id: \.self) { limit in
        if let current = values[limit] {
          Picker(label(limit), selection: Binding(get: { current }, set: { value in
            guard value != current, !saving else { return }
            saving = true
            Task {
              defer { saving = false }
              do {
                try await session.setDownloadLimit(limit, to: value)
                values = try await session.downloadLimits()
                failed = false
                changed()
              } catch { failed = true }
            }
          })) {
            ForEach(1...8, id: \.self) { Text(String($0)).tag($0) }
          }.pickerStyle(.menu).disabled(saving)
        }
      }
      if failed {
        Text(tr("操作失败，请重试"))
        Button(tr("重试")) { reload += 1 }
      }
      Button(tr("完成")) { dismiss() }.disabled(saving)
    }
    .padding()
    .interactiveDismissDisabled(saving)
    .task(id: reload) {
      do { values = try await session.downloadLimits(); failed = false }
      catch { failed = true }
    }
  }
}

@MainActor final class ComicExportJobs: ObservableObject {
  struct Job: Identifiable {
    enum State { case running, done(Int), failed }
    let id = UUID()
    let work: LibraryWorkID
    let title: String
    let pdf: Bool
    var state: State = .running
  }
  @Published private(set) var jobs: [Job] = []
  @Published private(set) var revision = 0

  func running(_ work: LibraryWorkID, pdf: Bool) -> Bool {
    jobs.contains { job in
      guard job.work == work && job.pdf == pdf else { return false }
      if case .running = job.state { return true }
      return false
    }
  }

  func start(_ work: LibraryWork, pdfPages: Int32?) {
    let pdf = pdfPages != nil
    guard !running(work.libraryID, pdf: pdf) else { return }
    let job = Job(work: work.libraryID, title: work.title, pdf: pdf)
    jobs.insert(job, at: 0)
    let operation = Task {
      // 用户离开详情或关闭本窗口后，由独立会话完成已经接受的文件导出。
      let session = try await ProgressSession.openApplicationLibrary()
      do {
        let files = try await session.exportComic(work, pdfPages: pdfPages)
        await session.close()
        return files.count
      } catch {
        await session.close()
        throw error
      }
    }
    #if os(iOS)
    let allowance = UIApplication.shared.beginBackgroundTask(withName: "Comic export") { operation.cancel() }
    #endif
    Task {
      #if os(iOS)
      defer { if allowance != .invalid { UIApplication.shared.endBackgroundTask(allowance) } }
      #endif
      let result = await operation.result
      guard let index = jobs.firstIndex(where: { $0.id == job.id }) else { return }
      switch result {
      case .success(let count): jobs[index].state = .done(count)
      case .failure: jobs[index].state = .failed
      }
      revision += 1
    }
  }

  func dismiss(_ id: UUID) {
    jobs.removeAll { job in
      if case .running = job.state { return false }
      return job.id == id
    }
  }

  func fileRemoved() { revision += 1 }
}

@MainActor struct ComicExportActions: View {
  @ObservedObject var jobs: ComicExportJobs
  let work: LibraryWork
  let session: ProgressSession
  let tr: (String) -> String
  @State private var choosing = false
  @State private var limit: Int32 = 300
  @State private var failed = false
  private let choices: [Int32] = [100,300,1000,0]

  var body: some View {
    Menu(tr("导出")) {
      Button(tr("导出为 CBZ")) { jobs.start(work, pdfPages: nil) }.disabled(jobs.running(work.libraryID, pdf: false))
      Button(tr("导出为 PDF")) {
        Task {
          do {
            let saved = try await session.comicPdfLimit()
            limit = choices.contains(saved) ? saved : 300
            failed = false
            choosing = true
          } catch { failed = true }
        }
      }.disabled(jobs.running(work.libraryID, pdf: true))
    }
    .sheet(isPresented: $choosing) {
      VStack(alignment: .leading, spacing: 16) {
        Text(tr("导出 PDF")).font(.headline)
        Picker(tr("每个文件最多页数"), selection: $limit) {
          ForEach(choices, id: \.self) { value in
            Text(value == 0 ? tr("不限") : tr("{value0} 页").replacingOccurrences(of: "{value0}", with: String(value))).tag(value)
          }
        }
        Text(tr("超过页数或单个文件过大时自动分成多个文件；导出在后台进行。"))
        HStack {
          Button(tr("取消"), role: .cancel) { choosing = false }
          Button(tr("导出")) { choosing = false; jobs.start(work, pdfPages: limit) }
        }
      }.padding()
    }
    if jobs.running(work.libraryID, pdf: false) || jobs.running(work.libraryID, pdf: true) { ProgressView(tr("正在导出…")) }
    if failed { Text(tr("加载失败")) }
  }
}

@MainActor struct ComicExportShelf: View {
  private struct Removal: Identifiable {
    let id: URL
    let modified: Date
    let size: Int
  }
  @ObservedObject var jobs: ComicExportJobs
  let session: ProgressSession
  let tr: (String) -> String
  @Environment(\.dismiss) private var dismiss
  @State private var files: [URL] = []
  @State private var retry = 0
  @State private var failed = false
  @State private var removal: Removal?
  @State private var removeFailed = false
  var body: some View {
    let names = Dictionary(grouping: files, by: \.lastPathComponent)
    VStack(alignment: .leading) {
      HStack { Text(tr("导出文件")).font(.headline); Spacer(); Button(tr("完成")) { dismiss() } }.padding()
      List {
        if !jobs.jobs.isEmpty {
          Section(header: Text(tr("导出任务"))) {
            ForEach(jobs.jobs) { job in
              VStack(alignment: .leading) {
                Text(job.title + (job.pdf ? " · PDF" : " · CBZ"))
                switch job.state {
                case .running: ProgressView(tr("正在导出…"))
                case .failed:
                  Text(tr("导出失败，请重试"))
                  Button(tr("移除记录")) { jobs.dismiss(job.id) }
                case .done(let count):
                  Text(tr("已导出 {value0} 个文件").replacingOccurrences(of: "{value0}", with: String(count)))
                  Button(tr("移除记录")) { jobs.dismiss(job.id) }
                }
              }
            }
          }
        }
        Section(header: Text(tr("已导出"))) {
          if failed { Text(tr("加载失败")); Button(tr("重试")) { retry += 1 } }
          else if files.isEmpty && jobs.jobs.isEmpty { Text(tr("还没有导出文件")) }
          ForEach(files, id: \.self) { file in
            VStack(alignment: .leading) {
              SavedFileExportButton(url: file, filename: file.lastPathComponent, label: file.lastPathComponent, tr: tr)
              if (names[file.lastPathComponent]?.count ?? 0) > 1 { Text(verbatim: file.deletingLastPathComponent().path).font(.caption) }
              HStack {
                FileShareButton(url: file, label: tr("分享")).fixedSize()
                if ["pdf", "cbz"].contains(file.pathExtension.lowercased()) {
                  Button(tr("删除"), role: .destructive) {
                    do {
                      let current = URL(fileURLWithPath: file.path)
                      let stat = try current.resourceValues(forKeys: [.fileSizeKey, .contentModificationDateKey])
                      guard let size = stat.fileSize, let modified = stat.contentModificationDate else { throw CocoaError(.fileReadUnknown) }
                      removal = Removal(id: current, modified: modified, size: size)
                      removeFailed = false
                    } catch { removeFailed = true }
                  }
                }
              }
            }
          }
          if removeFailed { Text(tr("操作失败，请重试")) }
        }
      }.buttonStyle(.borderless)
    }
    #if os(macOS)
    .frame(minWidth: 420, idealWidth: 500, minHeight: 320, idealHeight: 420)
    #endif
    .alert(item: $removal) { item in
      Alert(title: Text(tr("删除导出文件？")),
            message: Text(verbatim: (names[item.id.lastPathComponent]?.count ?? 0) > 1 ? item.id.path : item.id.lastPathComponent),
            primaryButton: .destructive(Text(tr("删除"))) {
              Task {
                do {
                  try await session.removeExportedComic(item.id, modified: item.modified, size: item.size)
                  jobs.fileRemoved()
                } catch { removeFailed = true; retry += 1 }
              }
            }, secondaryButton: .cancel(Text(tr("取消"))))
    }
    .task(id: jobs.revision + retry) {
      do { files = try await session.exportedComics(); failed = false }
      catch { if !Task.isCancelled { failed = true } }
    }
  }
}
