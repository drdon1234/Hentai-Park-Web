import Network
import ParkCore
import SwiftUI
#if os(iOS)
  import UIKit
#endif

@MainActor final class DownloadQueue: ObservableObject {
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

  func didEnqueue() {
    revision += 1
    refresh()
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
        try await opened.processDownloads(library, wifi: wifi) {
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

  #if os(iOS)
    private func endAllowance() {
      if allowance != .invalid { UIApplication.shared.endBackgroundTask(allowance) }
      allowance = .invalid
    }
  #endif
}

@MainActor final class DownloadSelection: ObservableObject {
  @Published var status: Int32?
  @Published var selecting = false
  @Published var selected = Set<LibraryWorkID>()

  func rows(_ works: [LibraryWork]) -> [LibraryWork] {
    works.filter { $0.downloaded && (status == nil || $0.downloadStatus == status) }
  }

  func finish() {
    selected.removeAll()
    selecting = false
  }
}

@MainActor struct DownloadControls: View {
  @ObservedObject var selection: DownloadSelection
  @EnvironmentObject private var queue: DownloadQueue
  let works: [LibraryWork]
  let session: ProgressSession
  let tr: (String) -> String
  @State private var confirming = false
  @State private var targets: [LibraryWork] = []
  @State private var busy = false
  @State private var failed = false

  var body: some View {
    Section {
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
      }
      if failed { Text(tr("删除失败，请重试")) }
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
}
