import Network
import ParkCore
import SwiftUI
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

  func refresh() {
    requested = true
    startIfNeeded()
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
  let untypedSources: Set<String>
  let session: ProgressSession
  let tr: (String) -> String
  @State private var confirming = false
  @State private var targets: [LibraryWork] = []
  @State private var busy = false
  @State private var failed = false
  @State private var catalogFailed = false
  @State private var catalogRetry = 0
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
        Button(tr("删除下载")) {
          targets = works.filter { selection.selected.contains($0.libraryID) }
          confirming = !targets.isEmpty
        }.disabled(selection.selected.isEmpty)
        Button(tr("取消")) { selection.finish(); failed = false }
      } else {
        Button(tr("批量选择")) { selection.selecting = true }.disabled(works.isEmpty)
        Button(tr("导出文件"), action: showExports)
        Button(tr("管理分类"), action: manageCategories)
      }
      if failed { Text(tr("删除失败，请重试")) }
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

  private struct CatalogRequest: Equatable {
    let sources: Set<String>
    let retry: Int
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
  @ObservedObject var jobs: ComicExportJobs
  let session: ProgressSession
  let tr: (String) -> String
  @Environment(\.dismiss) private var dismiss
  @State private var files: [URL] = []
  @State private var retry = 0
  @State private var failed = false
  var body: some View {
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
            SavedFileExportButton(url: file, filename: file.lastPathComponent, label: file.lastPathComponent, tr: tr)
          }
        }
      }.buttonStyle(.borderless)
    }
    #if os(macOS)
    .frame(minWidth: 420, idealWidth: 500, minHeight: 320, idealHeight: 420)
    #endif
    .task(id: jobs.revision + retry) {
      do { files = try await session.exportedComics(); failed = false }
      catch { if !Task.isCancelled { failed = true } }
    }
  }
}
