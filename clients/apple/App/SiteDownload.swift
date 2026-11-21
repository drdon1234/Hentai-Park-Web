import ParkCore
import SwiftUI

@MainActor struct SiteDownload: View {
  let session: ProgressSession
  let library: SourceLibrary
  let detail: SiteWork
  let work: LibraryWork
  let tr: (String) -> String
  @Environment(\.dismiss) private var dismiss
  @EnvironmentObject private var queue: DownloadQueue
  @State private var selected = Set<String>()
  @State private var pickingLine = false
  @State private var grant: VideoLines?
  @State private var options: [VideoLine] = []
  @State private var message: String?
  @State private var retry = 0
  @State private var saving = false

  private var chapters: [OfflineChapter] {
    work.chapters.count == 1 ? work.chapters : work.chapters.filter { selected.contains($0.id) }
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      Text(pickingLine ? tr("选择下载线路") :
        tr("选择要下载的章节（{count}）").replacingOccurrences(of: "{count}", with: String(chapters.count)))
        .font(.headline)
      if pickingLine {
        if grant == nil && message == nil { ProgressView() }
        List(Array(options.enumerated()), id: \.offset) { index, line in
          Button(line.label) { save(index: UInt32(index)) }
        }
        if options.isEmpty && message != nil {
          Button(tr("重试")) { retry += 1 }
        }
      } else if work.chapters.count == 1 {
        Text(work.chapters[0].title)
      } else {
        Button(tr(selected.count == work.chapters.count ? "取消全选" : "全选")) {
          selected = selected.count == work.chapters.count ? [] : Set(work.chapters.map(\.id))
        }
        List(work.chapters, id: \.id) { row in
          Toggle(row.title, isOn: Binding(get: { selected.contains(row.id) }, set: { checked in
            if checked { selected.insert(row.id) } else { selected.remove(row.id) }
          }))
        }
      }
      if let message { Text(tr(message)) }
      HStack {
        Button(tr("取消")) { dismiss() }
        Spacer()
        if !pickingLine {
          Button(tr(work.media == "video" ? "选择下载线路" : "开始下载")) {
            if work.media == "video" { pickingLine = true } else { save(index: nil) }
          }.disabled(chapters.isEmpty)
        }
      }
    }
    .padding().disabled(saving).interactiveDismissDisabled(saving)
    .task(id: pickingLine ? retry : -1) {
      guard pickingLine, let episode = chapters.first?.id else { return }
      grant = nil
      options = []
      message = nil
      do {
        let found = try await session.videoChoices(library,
          key: ChapterKey(source: work.source, workId: work.workId, chapterId: episode))
        try Task.checkCancellation()
        grant = found
        options = found.options()
        if options.isEmpty { message = "此影片暂无可用播放地址" }
      } catch { if !Task.isCancelled { message = siteMessage(error) } }
    }
  }

  private func save(index: UInt32?) {
    guard !saving else { return }
    saving = true
    message = nil
    Task {
      defer { saving = false }
      do {
        try await session.enqueueDownload(work: work, detail: detail, chapters: chapters, lines: grant, index: index)
        queue.didEnqueue()
        dismiss()
      } catch { message = "保存失败，请重试" }
    }
  }
}
