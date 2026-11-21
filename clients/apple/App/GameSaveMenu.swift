import ParkCore
import SwiftUI
import UniformTypeIdentifiers
import WebKit

@MainActor final class GamePageControls: ObservableObject {
  private weak var view: WKWebView?
  private var generation = UUID()
  private var keyboard = GameKeyboard()
  func press(_ key: GameKey, down: Bool) {
    guard let view, let script = keyboard.press(key, down: down) else { return }
    view.evaluateJavaScript(script, in: nil, in: .defaultClient, completionHandler: nil)
  }
  func releaseKeys() {
    for script in keyboard.releaseAll() {
      view?.evaluateJavaScript(script, in: nil, in: .defaultClient, completionHandler: nil)
    }
  }
  func attach(_ page: WKWebView) {
    view = page
    generation = UUID()
  }
  func detach(_ page: WKWebView) {
    if view === page {
      releaseKeys()
      view = nil
      generation = UUID()
    }
  }

  func snapshot(source: String, target: OnlineGame) async throws -> GameSave {
    guard let view, let url = view.url, let host = url.host,
      target.allowsDocument(address: url.absoluteString)
    else { throw SiteReadError.malformed }
    let id = generation
    let origin = "https://" + host.lowercased()
    let raw = try await view.callAsyncJavaScript(
      GameSave.snapshotScript,
      arguments: [
        "origin": origin, "source": source, "game": target.storageId(), "limit": GameSave.byteLimit,
      ],
      in: nil, contentWorld: .defaultClient)
    try Task.checkCancellation()
    guard id == generation, let text = raw as? String, text.utf8.count <= GameSave.byteLimit
    else { throw SiteReadError.malformed }
    return try GameSave.decode(Data(text.utf8), source: source, target: target)
  }

  func restore(_ save: GameSave, session: ProgressSession, source: String, target: OnlineGame)
    async throws
  {
    let id = generation
    let current = try await snapshot(source: source, target: target)
    guard current.origin == save.origin else { throw SiteReadError.malformed }
    try await session.preserveGameSave(current, source: source, target: target)
    try Task.checkCancellation()
    guard id == generation, let view else { throw CancellationError() }
    let applied = try await view.callAsyncJavaScript(
      GameSave.restoreScript,
      arguments: [
        "origin": save.origin, "previous": String(decoding: current.data, as: UTF8.self),
        "incoming": String(decoding: save.data, as: UTF8.self),
      ], in: nil, contentWorld: .defaultClient)
    guard id == generation, applied as? Bool == true else { throw SiteReadError.malformed }
    view.reload()
  }
}

private struct SaveDocument: FileDocument {
  static let readableContentTypes: [UTType] = [.json]
  let data: Data
  init(_ data: Data) { self.data = data }
  init(configuration: ReadConfiguration) throws {
    guard let data = configuration.file.regularFileContents, data.count <= GameSave.byteLimit
    else { throw SiteReadError.malformed }
    self.data = data
  }
  func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
    FileWrapper(regularFileWithContents: data)
  }
}

@MainActor struct GameSaveMenu: View {
  let controls: GamePageControls
  let session: ProgressSession
  let source: String
  let target: OnlineGame
  let tr: (String) -> String
  @State private var task: Task<Void, Never>?
  @State private var importing = false
  @State private var exporting = false
  @State private var confirming = false
  @State private var selected: GameSave?
  @State private var output: SaveDocument?
  @State private var status: String?

  var body: some View {
    VStack {
      Menu(tr("游戏存档")) {
        Button(tr("导出游戏存档（仅 localStorage）")) {
          perform {
            let snapshot = try await controls.snapshot(source: source, target: target)
            output = SaveDocument(snapshot.data)
            exporting = true
          }
        }
        Button(tr("导入游戏存档（仅 localStorage）")) { importing = true }
      }.disabled(task != nil)
      if let status { Text(tr(status)) }
    }
    .fileExporter(
      isPresented: $exporting, document: output, contentType: .json,
      defaultFilename: "hentaipark-game-save.json"
    ) { result in
      output = nil
      switch result {
      case .success: status = "已导出游戏存档"
      case .failure(let error): show(error)
      }
    }
    .fileImporter(isPresented: $importing, allowedContentTypes: [.json, .plainText]) { result in
      perform {
        let url = try result.get()
        let bytes = try await Task.detached {
          let scoped = url.startAccessingSecurityScopedResource()
          defer { if scoped { url.stopAccessingSecurityScopedResource() } }
          let reader = try FileHandle(forReadingFrom: url)
          defer { try? reader.close() }
          var data = Data()
          while let part = try reader.read(
            upToCount: min(65536, GameSave.byteLimit + 1 - data.count)),
            !part.isEmpty
          {
            data.append(part)
            guard data.count <= GameSave.byteLimit else { throw SiteReadError.malformed }
          }
          return data
        }.value
        try Task.checkCancellation()
        selected = try GameSave.decode(bytes, source: source, target: target)
        confirming = true
      }
    }
    .alert(tr("导入游戏存档"), isPresented: $confirming) {
      Button(tr("取消"), role: .cancel) { selected = nil }
      Button(tr("导入")) {
        guard let save = selected else { return }
        selected = nil
        perform {
          try await controls.restore(save, session: session, source: source, target: target)
          status = "已导入游戏存档"
        }
      }
    } message: {
      Text(tr("将替换此游戏的 localStorage；IndexedDB 存档不包含在内。导入前会保留当前数据。"))
    }
    .onDisappear {
      task?.cancel()
      task = nil
      selected = nil
      output = nil
    }
  }

  private func perform(_ action: @escaping @MainActor () async throws -> Void) {
    guard task == nil else { return }
    status = nil
    task = Task {
      defer { task = nil }
      do {
        try Task.checkCancellation()
        try await action()
      } catch { if !Task.isCancelled { show(error) } }
    }
  }

  private func show(_ error: Error) {
    if (error as? CocoaError)?.code != .userCancelled { status = "游戏存档操作失败" }
  }
}
