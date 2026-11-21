import AVFoundation
import CoreText
import ParkCore
import SwiftUI

/// 断行和绘制使用相同的字体、宽度及 Core Text 行；字号或窗口改变时从原文锚点重新排版。
private struct NovelPage {
  let lines: [CTLine]
  let next: Int
  let content: String
  let lineHeight: CGFloat
  let ascent: CGFloat

  init(text: NSString, cursor: Int, box: CGSize, fontSize: CGFloat, backwards: Bool = false) {
    let font = CTFontCreateWithName("TimesNewRomanPSMT" as CFString, fontSize, nil)
    ascent = CTFontGetAscent(font)
    lineHeight = max(fontSize * 1.5, ascent + CTFontGetDescent(font) + CTFontGetLeading(font))
    let rows = max(1, Int(box.height / lineHeight))
    let width = max(1, box.width)
    var start = backwards ? max(0, cursor - 16000) : cursor
    var end = backwards ? cursor : min(text.length, cursor + 16000)
    func isLow(_ index: Int) -> Bool {
      index < text.length && (0xdc00...0xdfff).contains(text.character(at: index))
    }
    if isLow(start) { start = max(0, start - 1) }
    if isLow(end) { end -= 1 }
    let section = text.substring(with: NSRange(location: start, length: end - start))
    let attributes: [NSAttributedString.Key: Any] = [
      NSAttributedString.Key(kCTFontAttributeName as String): font,
      NSAttributedString.Key(kCTForegroundColorFromContextAttributeName as String): true,
    ]
    let setter = CTTypesetterCreateWithAttributedString(
      NSAttributedString(string: section, attributes: attributes) as CFAttributedString)
    var drawn: [CTLine] = []
    var starts: [Int] = []
    var index = 0
    while index < end - start && (backwards || drawn.count < rows) {
      let count = CTTypesetterSuggestLineBreak(setter, index, Double(width))
      let length = count > 0 ? count : (isLow(start + index + 1) ? 2 : 1)
      starts.append(start + index)
      if !backwards {
        drawn.append(CTTypesetterCreateLine(setter, CFRange(location: index, length: length)))
      }
      index += length
    }
    next = backwards ? (starts.suffix(rows).first ?? start) : start + index
    content = backwards ? "" : text.substring(with: NSRange(location: start, length: index))
    lines = drawn
  }
}

@MainActor
private final class NovelSpeech: NSObject, ObservableObject, PlaybackOwner,
  AVSpeechSynthesizerDelegate
{
  private let synthesizer = AVSpeechSynthesizer()
  private var current: AVSpeechUtterance?

  override init() {
    super.init()
    synthesizer.delegate = self
    #if os(iOS)
      synthesizer.usesApplicationAudioSession = true
    #endif
  }
  func speak(_ part: String) throws {
    stop()
    guard !part.isEmpty else { return }
    try PlaybackFocus.shared.claim(self, spoken: true)
    let utterance = AVSpeechUtterance(string: part)
    current = utterance
    synthesizer.speak(utterance)
  }
  func stop() { interrupt() }
  func interrupt() {
    current = nil
    synthesizer.stopSpeaking(at: .immediate)
    PlaybackFocus.shared.release(self)
  }
  nonisolated func speechSynthesizer(
    _ synthesizer: AVSpeechSynthesizer, didFinish utterance: AVSpeechUtterance
  ) { finished(utterance) }
  nonisolated func speechSynthesizer(
    _ synthesizer: AVSpeechSynthesizer, didCancel utterance: AVSpeechUtterance
  ) { finished(utterance) }
  private nonisolated func finished(_ utterance: AVSpeechUtterance) {
    Task { @MainActor [weak self] in
      guard let self, self.current === utterance else { return }
      self.current = nil
      PlaybackFocus.shared.release(self)
    }
  }
}

private enum NovelFailure {
  case load
  case mode(NovelTextMode)
  case font(Double)
  case page(Int32, Int32?)
  case speech

  var message: String {
    switch self {
    case .load, .mode: return "加载失败"
    case .font, .page: return "保存失败，请重试"
    case .speech: return "媒体播放失败"
    }
  }
}

@MainActor struct NovelReader: View {
  let session: ProgressSession
  let key: ChapterKey
  let work: LibraryWork
  let incognito: Bool
  let locale: String
  let showBack: Bool
  let tr: (String) -> String
  let back: () -> Void
  var library: SourceLibrary? = nil
  @State private var document: NovelDocument?
  @State private var view: NovelPresentation?
  @State private var text: NSString = ""
  @State private var sourcePosition: Int32 = 0
  @State private var cursor: Int32 = 0
  @State private var updated: Int64 = 0
  @State private var mode: NovelTextMode = .original
  @State private var fontSize = 22.0
  @ScaledMetric(relativeTo: .body) private var fontScale: CGFloat = 1
  @State private var notes = false
  @State private var error: NovelFailure?
  @State private var retry = 0
  @State private var pending: Task<Void, Never>?
  @StateObject private var speech = NovelSpeech()
  @Environment(\.colorScheme) private var colors
  @Environment(\.scenePhase) private var phase
  @Environment(\.privacyObscured) private var obscured

  private var title: String {
    work.chapters.first(where: { $0.id == key.chapterId })?.title ?? work.title
  }
  private var modes: [(NovelTextMode, String)] {
    [(.original, "显示原文"), (.translated, "显示译文"), (.bilingual, "双语对照")]
  }

  var body: some View {
    VStack(alignment: .leading) {
      HStack {
        if showBack { Button(tr("返回"), action: back) }
        Text(document.flatMap { $0.title.isEmpty ? nil : $0.title } ?? title).lineLimit(2)
        Spacer()
        if let document, view != nil {
          Menu(tr("阅读设置")) {
            Button(tr("书签与标注")) { notes = true }
            if !document.translation.isEmpty {
              ForEach(modes.indices, id: \.self) { i in
                Button(tr(modes[i].1)) { switchMode(modes[i].0) }
              }
            }
            Button(tr("字号") + " +") { resize(to: min(42, fontSize + 2)) }.disabled(fontSize >= 42)
            Button(tr("字号") + " −") { resize(to: max(12, fontSize - 2)) }.disabled(fontSize <= 12)
            Button(tr("朗读"), action: speak)
            Button(tr("停止朗读")) { speech.stop() }
          }.disabled(pending != nil)
        }
      }
      if let error {
        HStack {
          Text(tr(error.message))
          Button(tr("重试")) { repeatAction(error) }.disabled(pending != nil)
        }
      }
      if let view {
        GeometryReader { geometry in
          let size = CGSize(
            width: geometry.size.width, height: max(0, geometry.size.height - footerHeight))
          let font = CGFloat(fontSize) * fontScale
          let page = NovelPage(text: text, cursor: Int(cursor), box: size, fontSize: font)
          VStack(spacing: 0) {
            Canvas { context, bounds in
              context.withCGContext { cg in
                cg.translateBy(x: 0, y: bounds.height)
                cg.scaleBy(x: 1, y: -1)
                cg.textMatrix = .identity
                cg.setFillColor(CGColor(gray: colors == .dark ? 1 : 0, alpha: 1))
                for (i, line) in page.lines.enumerated() {
                  let runs = CTLineGetGlyphRuns(line) as? [CTRun]
                  let first = runs?.min {
                    CTRunGetStringRange($0).location < CTRunGetStringRange($1).location
                  }
                  let rtl = first.map { CTRunGetStatus($0).contains(.rightToLeft) } ?? false
                  let offset = CTLineGetPenOffsetForFlush(line, rtl ? 1 : 0, Double(bounds.width))
                  cg.textPosition = CGPoint(
                    x: CGFloat(offset),
                    y: bounds.height - page.ascent - CGFloat(i) * page.lineHeight)
                  CTLineDraw(line, cg)
                }
              }
            }.frame(height: size.height).accessibilityElement(children: .ignore).accessibilityLabel(
              Text(page.content)
            )
            .accessibilityAction(named: Text(tr("下一页"))) {
              if page.next < text.length {
                jump(view.source(offset: Int32(page.next)), display: Int32(page.next))
              }
            }
            HStack {
              Button(tr("上一页")) {
                let previous = NovelPage(
                  text: text, cursor: Int(cursor), box: size, fontSize: font, backwards: true)
                jump(view.source(offset: Int32(previous.next)), display: Int32(previous.next))
              }.disabled(cursor <= 0 || pending != nil)
              Spacer()
              Button(tr("下一页")) {
                jump(view.source(offset: Int32(page.next)), display: Int32(page.next))
              }
              .disabled(page.next >= text.length || pending != nil || footerHeight == 0)
            }
            .fixedSize(horizontal: false, vertical: true)
            .background(
              GeometryReader { bar in
                Color.clear.preference(key: NovelFooterHeight.self, value: bar.size.height)
              })
          }
        }
        .onPreferenceChange(NovelFooterHeight.self) { footerHeight = $0 }
      } else if error == nil {
        ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
      }
    }
    .padding()
    .task(id: retry) { await load() }
    .onDisappear {
      pending?.cancel()
      speech.stop()
    }
    .onChange(of: phase) { if $0 != .active { speech.stop() } }
    .onChange(of: obscured) {
      if $0 {
        notes = false
        speech.stop()
      }
    }
    #if os(iOS)
      .onReceive(NotificationCenter.default.publisher(for: AVAudioSession.interruptionNotification))
      { event in
        if (event.userInfo?[AVAudioSessionInterruptionTypeKey] as? NSNumber)?.uintValue
          == AVAudioSession.InterruptionType.began.rawValue
        {
          speech.stop()
        }
      }
    #endif
    .sheet(isPresented: $notes) {
      VStack {
        Text(tr("书签与标注")).font(.headline)
        if document?.notes.isEmpty == true { Text(tr("暂无书签与标注")) }
        List(document?.notes ?? [], id: \.id) { note in
          Button {
            notes = false
            jump(Int32(clamping: note.offset))
          } label: {
            VStack(alignment: .leading) {
              Text(note.quote)
              if !note.note.isEmpty { Text(note.note).font(.caption) }
            }
          }
        }
        Button(tr("返回")) { notes = false }
      }.padding()
    }
  }

  @State private var footerHeight: CGFloat = 0

  private func speak() {
    let end = min(Int(cursor) + 3500, text.length)
    let safeEnd =
      end < text.length && (0xdc00...0xdfff).contains(text.character(at: end)) ? end - 1 : end
    do {
      try speech.speak(
        text.substring(with: NSRange(location: Int(cursor), length: safeEnd - Int(cursor))))
      error = nil
    } catch { self.error = .speech }
  }

  private func repeatAction(_ failed: NovelFailure) {
    error = nil
    switch failed {
    case .load: retry += 1
    case .mode(let mode): switchMode(mode)
    case .font(let value): resize(to: value)
    case .page(let position, let display): jump(position, display: display)
    case .speech: speak()
    }
  }

  private func load() async {
    do {
      let settings = try await session.novelSettings()
      let engine = settings.translateEngine ?? "google"
      let body: NovelDocument
      if let library {
        body = try await session.sourceNovel(library, key: key, locale: locale, engine: engine)
      } else {
        body = try await session.offlineNovel(key, locale: locale, engine: engine)
      }
      let saved = try await session.read(key)
      try Task.checkCancellation()
      fontSize = min(42, max(12, settings.novelFontSize ?? 22))
      sourcePosition = Int32(
        clamping: saved?.pageIndex ?? (work.historyChapter == key.chapterId ? work.historyPage : 0))
      updated = saved?.pageUpdatedMs ?? 0
      document = body
      try render(body, mode)
    } catch { if !Task.isCancelled { self.error = .load } }
  }

  private func render(_ body: NovelDocument, _ mode: NovelTextMode) throws {
    let next = try NovelPresentation(original: body.text, translation: body.translation, mode: mode)
    let at = next.displayed(offset: sourcePosition)
    text = next.text() as NSString
    view = next
    self.mode = mode
    cursor = at
  }

  private func switchMode(_ next: NovelTextMode) {
    guard let document else { return }
    speech.stop()
    do {
      try render(document, next)
      error = nil
    } catch { error = .mode(next) }
  }

  private func resize(to value: Double) {
    pending = Task {
      defer { pending = nil }
      do {
        try await session.setNovelFontSize(value)
        fontSize = value
        error = nil
      } catch { if !Task.isCancelled { self.error = .font(value) } }
    }
  }

  private func jump(_ position: Int32, display: Int32? = nil) {
    guard pending == nil, let view, let document else { return }
    let original = document.text as NSString
    var anchor = min(original.length, max(0, Int(position)))
    if anchor < original.length && (0xdc00...0xdfff).contains(original.character(at: anchor)) {
      anchor -= 1
    }
    let next = display ?? view.displayed(offset: Int32(anchor))
    speech.stop()
    if Int32(anchor) == sourcePosition {
      cursor = next
      error = nil
      return
    }
    pending = Task {
      defer { pending = nil }
      do {
        if !incognito {
          let stamp = max(updated + 1, Int64(Date().timeIntervalSince1970 * 1000))
          let turn = PageTurn(
            key: key, historyChapterId: key.chapterId, chapterTitle: title,
            pageIndex: Int64(anchor),
            pageCount: Int64(original.length) + 1, updatedMs: stamp)
          updated = try await session.turnPage(turn).pageUpdatedMs ?? stamp
        }
        try Task.checkCancellation()
        sourcePosition = Int32(anchor)
        cursor = next
        error = nil
      } catch { if !Task.isCancelled { self.error = .page(position, display) } }
    }
  }
}

private struct NovelFooterHeight: PreferenceKey {
  static var defaultValue: CGFloat { 0 }
  static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}
