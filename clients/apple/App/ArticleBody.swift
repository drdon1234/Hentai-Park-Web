import SwiftUI
import ParkCore

@MainActor struct ArticleBody: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: String
  let title: String
  let blocks: [SiteArticleBlock]
  let tr: (String) -> String
  let select: (Int) -> Void

  var body: some View {
    ForEach(blocks.indices, id: \.self) { index in
      switch blocks[index] {
      case .text(let text): Text(text)
      case .image(let address):
        ArticlePicture(session: session, library: library, site: site, address: address,
          title: title, maximum: 1200, tr: tr, open: { select(index) })
      case .video(_, let preview):
        if let preview, !preview.isEmpty {
          ArticlePicture(session: session, library: library, site: site, address: preview,
            title: title, maximum: 1200, tr: tr, open: { select(index) })
        }
        Button { select(index) } label: { Label(tr("播放"), systemImage: "play.fill") }
      }
    }
  }
}

@MainActor struct ArticleGallery: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: String
  let work: SiteWork
  let blocks: [SiteArticleBlock]
  let selection: Int
  let tr: (String) -> String
  let select: (Int?) -> Void
  private var media: [Int] { blocks.indices.filter { blocks[$0].isMedia } }

  var body: some View {
    VStack {
      HStack {
        Button(tr("返回")) { select(nil) }
        if let position = media.firstIndex(of: selection) {
          Text("\(position + 1)/\(media.count)").monospacedDigit()
          Spacer()
          Button { select(media[position - 1]) } label: { Image(systemName: "chevron.backward") }
            .accessibilityLabel(tr("上一页")).disabled(position == 0)
          Button { select(media[position + 1]) } label: { Image(systemName: "chevron.forward") }
            .accessibilityLabel(tr("下一页")).disabled(position + 1 == media.count)
        }
      }.padding(.horizontal)
      #if os(iOS)
        TabView(selection: Binding(get: { selection }, set: { select($0) })) {
          ForEach(media, id: \.self) { index in
            Group {
              if selection == index { content(index) } else { Color.clear }
            }.tag(index)
          }
        }.tabViewStyle(.page(indexDisplayMode: .never))
      #else
        content(selection)
      #endif
    }
  }

  @ViewBuilder private func content(_ index: Int) -> some View {
    switch blocks[index] {
    case .image(let address):
      ArticlePicture(session: session, library: library, site: site, address: address,
        title: work.title, maximum: 4096, tr: tr).id(index)
    case .video(let id, _):
      MovieReader(key: ChapterKey(source: site, workId: work.id, chapterId: id),
        title: work.title, incognito: true, showBack: false, tr: tr, back: { select(nil) },
        library: library).id(index)
    case .text: EmptyView()
    }
  }
}

@MainActor struct ArticlePicture: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: String
  let address: String
  let title: String
  let maximum: Int
  let tr: (String) -> String
  var open: (() -> Void)? = nil
  var openFailedImage: (() -> Void)? = nil
  @State private var pixels: CGImage?
  @State private var failed = false
  @State private var retry = 0

  var body: some View {
    Group {
      if let pixels {
        let image = Image(decorative: pixels, scale: 1).resizable().scaledToFit().accessibilityLabel(title)
        if let open { Button(action: open) { image } } else { image }
      } else if failed {
        Button(tr("重试")) { retry += 1 }
        if let openFailedImage { Button(tr("查看图片"), action: openFailedImage) }
      } else { ProgressView() }
    }.task(id: retry) {
      do {
        failed = false
        let resource = try await session.siteImage(library, site: site, address: address)
        let image = try await decodedPage(resource, maxSize: maximum)
        try Task.checkCancellation()
        pixels = image
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}
