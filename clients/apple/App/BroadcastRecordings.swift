import ParkCore
import SwiftUI
#if os(iOS)
import UIKit
#endif

@MainActor final class BroadcastRecorder: ObservableObject {
  private struct Job {
    let task: Task<Void, Never>
    var stopping = false
  }

  @Published private var jobs: [LibraryWorkID: Job] = [:]
  @Published private(set) var failures: Set<LibraryWorkID> = []
  @Published private(set) var revision = 0
  var isBusy: Bool { !jobs.isEmpty }
  func contains(_ id: LibraryWorkID) -> Bool { jobs[id] != nil }
  func isStopping(_ id: LibraryWorkID) -> Bool { jobs[id]?.stopping == true }

  func start(_ library: SourceLibrary, work: LibraryWork) {
    let id = work.libraryID
    guard jobs[id] == nil else { return }
    failures.remove(id)
    let task = Task {
      #if os(iOS)
      let allowance = UIApplication.shared.beginBackgroundTask(withName: "ParkRecording") {
        Task { @MainActor in self.stop(id) }
      }
      #endif
      var session: ProgressSession?
      do {
        try Task.checkCancellation()
        let opened = try await ProgressSession.openApplicationLibrary()
        session = opened
        try await opened.captureBroadcast(library, work: work)
      } catch {
        if !Task.isCancelled { failures.insert(id) }
      }
      await session?.close()
      #if os(iOS)
      if allowance != .invalid { UIApplication.shared.endBackgroundTask(allowance) }
      #endif
      jobs.removeValue(forKey: id)
      revision += 1
    }
    jobs[id] = Job(task: task)
    revision += 1
  }

  func stop(_ id: LibraryWorkID) {
    guard let job = jobs[id], !job.stopping else { return }
    jobs[id]?.stopping = true
    job.task.cancel()
  }

  func stopAll() {
    for id in Array(jobs.keys) { stop(id) }
  }
}

@MainActor struct BroadcastRecordings: View {
  let session: ProgressSession
  let tr: (String) -> String
  let back: () -> Void
  @EnvironmentObject private var recorder: BroadcastRecorder
  @State private var rows: [RecordedBroadcast]?
  @State private var selection: RecordingVolume?
  @State private var failed = false
  @State private var retry = 0

  var body: some View {
    if let selection {
      let row = selection.recording
      MovieReader(key: ChapterKey(source: row.source, workId: row.work, chapterId: "_"),
        title: row.title, incognito: true, showBack: true, tr: tr, back: { self.selection = nil },
        recording: selection)
    } else {
      VStack(alignment: .leading) {
        Button(tr("返回"), action: back).padding(.horizontal)
        if failed {
          Button(tr("无法读取录制文件") + " · " + tr("重试")) { retry += 1 }
        } else if let rows {
          if rows.isEmpty { Text(tr("暂无内容")).padding() }
          List(rows) { row in
            VStack(alignment: .leading) {
              Text(row.title)
              Text(tr(status(row))).foregroundColor(.secondary)
              if recorder.contains(row.libraryID) {
                Button(tr("停止录制")) { recorder.stop(row.libraryID) }
                  .disabled(recorder.isStopping(row.libraryID))
              } else {
                ForEach(row.volumes, id: \.self) { number in
                  Button(tr("录制分卷") + " " + String(UInt64(number) + 1)) {
                    selection = RecordingVolume(row, number: number)
                  }
                }
              }
            }.buttonStyle(.borderless)
          }
        } else { ProgressView() }
      }
      .task(id: retry) {
        repeat {
          do {
            let current = try await session.recordings()
            try Task.checkCancellation()
            rows = current
            failed = false
            if recorder.isBusy { try await Task.sleep(nanoseconds: 1_000_000_000) }
          } catch {
            if !Task.isCancelled { failed = true }
            return
          }
        } while recorder.isBusy && !Task.isCancelled
      }
      .onChange(of: recorder.revision) { _ in retry += 1 }
    }
  }

  private func status(_ row: RecordedBroadcast) -> String {
    if row.unavailable == true { return "无法读取录制文件" }
    if recorder.isStopping(row.libraryID) { return "正在结束录制" }
    switch row.status {
    case "recording": return "正在录制"
    case "stopping": return "正在结束录制"
    case "finished": return "录制已完成"
    case "interrupted": return "录制已中断，已保存已有内容"
    default: return "录制失败，已保存已有内容"
    }
  }
}
