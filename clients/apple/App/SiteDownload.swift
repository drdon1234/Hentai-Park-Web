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
  @State private var selected: Set<String>
  @State private var pickingLine = false
  @State private var grant: VideoLines?
  @State private var options: [VideoLine] = []
  @State private var message: String?
  @State private var retry = 0
  @State private var saving = false

  init(session: ProgressSession, library: SourceLibrary, detail: SiteWork, work: LibraryWork,
       tr: @escaping (String) -> String, initialSelection: Set<String> = []) {
    self.session = session; self.library = library; self.detail = detail; self.work = work; self.tr = tr
    _selected = State(initialValue: initialSelection)
  }

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

@MainActor struct SiteChapterDirectory: View {
  let session: ProgressSession
  let library: SourceLibrary
  let detail: SiteWork
  let work: LibraryWork
  let tr: (String) -> String
  let canDownload: Bool
  let open: (OfflineChapter) -> Void
  @State private var descending = false
  @State private var editing = false
  @State private var selected = Set<String>()
  @State private var readIDs = Set<String>()
  @State private var downloading = false
  @State private var saving = false
  @State private var saveFailed = false
  @State private var loadFailed = false
  @State private var revision = 0

  private var rows: [OfflineChapter] { descending ? Array(work.chapters.reversed()) : work.chapters }


  var body: some View {
    if !work.chapters.isEmpty {
      LazyVStack(alignment: .leading, spacing: 8) {
        HStack {
          Button(tr(descending ? "倒序" : "正序")) { descending.toggle() }
          Button(tr("管理章节")) { choose([]) }
        }
        if loadFailed { Button(tr("重试")) { revision += 1 } }
        ForEach(rows, id: \.id) { row in
          Button { open(row) } label: {
            HStack {
              Text(row.title)
              if readIDs.contains(row.id) { Image(systemName: "checkmark").accessibilityLabel(tr("已读")) }
            }
          }.contextMenu {
            Button(tr("管理章节")) { choose([row.id]) }
          }
        }
      }
      .task(id: revision) {
        guard work.media != "video" else { return }
        do {
          let saved = try await session.readChapterIDs(source: work.source, work: work.workId)
          try Task.checkCancellation()
          readIDs = saved; loadFailed = false
        } catch { if !Task.isCancelled { loadFailed = true } }
      }
      .sheet(isPresented: $editing) {
        VStack(alignment: .leading, spacing: 12) {
          HStack {
            Text(tr("已选 {count}").replacingOccurrences(of: "{count}", with: String(selected.count))).font(.headline)
            Spacer()
            Button(tr("取消")) { editing = false }
          }
          Button(tr(selected.count == rows.count ? "取消全选" : "全选")) {
            selected = selected.count == rows.count ? [] : Set(rows.map(\.id))
          }
          List(rows, id: \.id) { row in
            Toggle(row.title, isOn: Binding(get: { selected.contains(row.id) }, set: { checked in
              if checked { selected.insert(row.id) } else { selected.remove(row.id) }
            }))
          }
          if saveFailed { Text(tr("保存失败，请重试")) }
          HStack {
            if work.media != "video" {
              Menu(tr("管理")) {
                Button(tr("标为已读")) { mark(work.chapters.filter { selected.contains($0.id) }, read: true) }
                Button(tr("标为未读")) { mark(work.chapters.filter { selected.contains($0.id) }, read: false) }
                if selected.count == 1, let id = selected.first {
                  let earlier = Array(work.chapters.prefix { $0.id != id })
                  Button(tr("之前的章节标为已读")) { mark(earlier, read: true) }.disabled(earlier.isEmpty)
                }
              }.disabled(selected.isEmpty)
            }
            if canDownload {
              Button(tr("下载")) { downloading = true }.disabled(selected.isEmpty)
            }
          }
        }
        .padding()
        #if os(macOS)
        .frame(minWidth: 320, minHeight: 300)
        #endif
        .disabled(saving).interactiveDismissDisabled(saving)
        .sheet(isPresented: $downloading) {
          SiteDownload(session: session, library: library, detail: detail, work: work, tr: tr, initialSelection: selected)
        }
      }
    }
  }

  private func choose(_ ids: Set<String>) {
    selected = ids; saveFailed = false; editing = true
  }

  private func mark(_ chapters: [OfflineChapter], read: Bool) {
    guard !saving, !chapters.isEmpty else { return }
    saving = true; saveFailed = false
    Task {
      defer { saving = false }
      do {
        try await session.markChapters(chapters, source: work.source, work: work.workId, read: read)
        revision += 1; editing = false
      } catch { saveFailed = true }
    }
  }
}
