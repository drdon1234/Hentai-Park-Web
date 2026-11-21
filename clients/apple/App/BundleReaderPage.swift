import SwiftUI
import ImageIO
import UniformTypeIdentifiers
import ParkCore

@MainActor struct BundleReaderPage: View {
  let session: ProgressSession
  let work: LibraryWork
  let chapter: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  @State private var bundle: LibraryBundle?
  @State private var parents: [String] = []
  @State private var playOnOpen = true
  @State private var selected: BundleEntry?
  @State private var attempt = 0
  @State private var failed = false

  var body: some View {
    Group {
      if let bundle, let selected {
        asset(selected, bundle: bundle)
      } else {
        VStack(alignment: .leading) {
          Button(tr("返回")) { if parents.isEmpty { back() } else { parents.removeLast() } }.padding(.horizontal)
          if let bundle {
            List {
              Text(parents.last.flatMap { id in bundle.contents.entries.first { $0.id == id }?.name } ?? work.title).font(.headline)
              if !bundle.contents.complete { Text(tr("未完成")) }
              if bundle.contents.previewOnly { Text(tr("受限内容，仅展示原站公开预览")) }
              if parents.isEmpty {
                ForEach(bundle.contents.blocks.indices, id: \.self) { index in
                  let block = bundle.contents.blocks[index]
                  if block.kind == "text", let text = block.text { Text(text) }
                  else if let id = block.asset, let node = bundle.contents.entries.first(where: { $0.id == id }) {
                    if node.kind == "image" {
                      BundlePicture(session: session, key: bundle.key, node: node, maximum: 1200, tr: tr,
                        open: { selected = node })
                    } else { button(node) }
                  }
                }
              }
              let inline = Set(bundle.contents.blocks.compactMap(\.asset))
              ForEach(bundle.contents.entries.filter { $0.parent == parents.last && !inline.contains($0.id) }, id: \.id) { node in button(node) }
            }
          } else if failed {
            Text(tr("离线文件不可用，请检查存储位置后重试"))
            Button(tr("重试")) { attempt += 1 }
          } else { ProgressView() }
        }
      }
    }.task(id: attempt) {
      guard bundle == nil else { return }
      do {
        failed = false
        let value = try await session.openBundle(work: work, chapter: chapter)
        if let id = value.selected, let node = value.contents.entries.first(where: { $0.id == id }) {
          var chain: [String] = []
          var parent = node.parent
          var seen = Set<String>()
          while let id = parent, seen.insert(id).inserted, let row = value.contents.entries.first(where: { $0.id == id }) {
            chain.append(id)
            parent = row.parent
          }
          parents = Array(chain.reversed())
          selected = node
        }
        bundle = value
      } catch { if !Task.isCancelled { failed = true } }
    }
  }

  private func button(_ node: BundleEntry) -> some View {
    Button(node.name) {
      if node.kind == "directory" { parents.append(node.id) }
      else { selected = node }
    }
  }

  @ViewBuilder private func asset(_ node: BundleEntry, bundle: LibraryBundle) -> some View {
    let key = ChapterKey(source: work.source, workId: work.workId, chapterId: node.id)
    if node.kind == "audio" || node.kind == "video" {
      let siblings = bundle.siblings(of: node)
      let captions = siblings.entries.first(where: { $0.id == node.id }).map { siblings.subtitles(for: $0) } ?? []
      MovieReader(key: key, title: node.name, incognito: incognito || bundle.contents.kind != "voice",
        showBack: true, tr: tr, back: { selected = nil; playOnOpen = true }, bundle: bundle.key, captions: captions,
        tracks: bundle.contents.kind == "voice" ? siblings.entries.filter(\.playable) : [],
        playOnOpen: playOnOpen, selectTrack: { track, resume in
          playOnOpen = resume
          selected = bundle.contents.entries.first(where: { $0.id == track.id })
        }).id(key)
    } else if node.kind == "subtitle", let caption = bundle.siblings(of: node).entries.first(where: { $0.id == node.id }) {
      VoiceCaptionPage(session: session, library: nil, key: bundle.key, node: caption, tr: tr, dismiss: { selected = nil }, seek: nil)
    } else {
      VStack(alignment: .leading) {
        Button(tr("返回")) { selected = nil }
        Text(node.name).font(.headline)
        if node.kind == "image" {
          BundlePicture(session: session, key: bundle.key, node: node, maximum: 4096, tr: tr)
        } else {
          BundleExport(session: session, key: bundle.key, node: node, tr: tr)
        }
      }.padding()
    }
  }
}

@MainActor private struct BundlePicture: View {
  let session: ProgressSession
  let key: ChapterKey
  let node: BundleEntry
  let maximum: Int
  let tr: (String) -> String
  var open: (() -> Void)? = nil
  @State private var pixels: CGImage?
  @State private var retry = 0
  @State private var failed = false
  var body: some View {
    Group {
      if let pixels {
        let image = Image(decorative: pixels, scale: 1).resizable().scaledToFit().accessibilityLabel(node.name)
        if let open { Button(action: open) { image } } else { image }
      }
      else if failed { Button(tr("重试")) { retry += 1 } }
      else { ProgressView() }
    }.task(id: retry) {
      do {
        failed = false
        let url = try await session.bundleAsset(key, node: node.id)
        let size = maximum
        let image = try await Task.detached { try thumbnail(at: url, maxSize: size) }.value
        try Task.checkCancellation()
        pixels = image
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}

private struct ExportedAsset: FileDocument {
  static var readableContentTypes: [UTType] { [.data] }
  let file: FileWrapper
  init(url: URL) throws {
    file = FileWrapper(regularFileWithContents: try Data(contentsOf: url, options: .alwaysMapped))
  }
  init(configuration: ReadConfiguration) throws { file = configuration.file }
  func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper { file }
}

@MainActor private struct BundleExport: View {
  let session: ProgressSession
  let key: ChapterKey
  let node: BundleEntry
  let tr: (String) -> String
  @State private var document: ExportedAsset?
  @State private var presenting = false
  @State private var busy = false
  @State private var failed = false

  var body: some View {
    VStack(alignment: .leading) {
      Button(tr("导出")) {
        guard !busy else { return }
        busy = true
        failed = false
        Task {
          defer { busy = false }
          do {
            let url = try await session.bundleAsset(key, node: node.id)
            document = try await Task.detached { try ExportedAsset(url: url) }.value
            presenting = true
          } catch { if !Task.isCancelled { failed = true } }
        }
      }.disabled(busy)
      if failed { Text(tr("导出失败，请重试")) }
    }.fileExporter(isPresented: $presenting, document: document, contentType: .data,
      defaultFilename: URL(fileURLWithPath: node.name).lastPathComponent) { result in
        document = nil
        if case .failure(let error) = result, (error as NSError).code != NSUserCancelledError { failed = true }
      }
  }
}
