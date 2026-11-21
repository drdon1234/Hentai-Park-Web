import Network
import ParkCore
import SwiftUI
#if os(iOS)
  import UIKit
#endif

@MainActor final class DownloadQueue: ObservableObject {
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
