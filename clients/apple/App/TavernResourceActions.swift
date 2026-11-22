import ParkCore
import SwiftUI
import UniformTypeIdentifiers

private struct TavernExport: FileDocument {
  static let readableContentTypes: [UTType] = [.json, .png]
  let bytes: Data
  init(_ bytes: Data) { self.bytes = bytes }
  init(configuration: ReadConfiguration) throws {
    guard let bytes = configuration.file.regularFileContents else { throw CocoaError(.fileReadCorruptFile) }
    self.bytes = bytes
  }
  func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
    FileWrapper(regularFileWithContents: bytes)
  }
}

@MainActor struct TavernResourceActions: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: String
  let item: SiteWork
  let tr: (String) -> String
  @State private var resource: SourceTavernResource?
  @State private var saved = false
  @State private var retry = 0
  @State private var loadFailed = false
  @State private var action: Task<Void, Never>?
  @State private var document: TavernExport?
  @State private var exporting = false
  @State private var status: String?

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      if let resource {
        Text(tr(resource.kind() == "lorebook" ? "世界书" : "角色卡"))
        Button(tr(saved ? "已保存" : "保存到本地")) {
          perform(failure: "保存失败，请重试") {
            try await session.saveTavernResource(resource)
            saved = true
          }
        }.disabled(saved || action != nil)
        Button(tr("导出原文件")) {
          perform(failure: "导出失败，请重试") {
            document = TavernExport(try await session.originalTavernBytes(resource))
            exporting = true
          }
        }.disabled(action != nil)
        if let status { Text(tr(status)) }
      } else if loadFailed {
        Text(tr("站点加载失败，请重试"))
        Button(tr("重试")) { retry += 1 }
      }
    }.task(id: retry) {
      loadFailed = false
      do {
        let asset = try await session.siteTavernResource(library, site: site, item: item)
        try Task.checkCancellation()
        saved = asset?.saved() == true
        resource = asset
      } catch { if !Task.isCancelled { loadFailed = true } }
    }
    .fileExporter(isPresented: $exporting, document: document,
      contentType: resource?.format() == "png" ? .png : .json,
      defaultFilename: String(item.title.prefix(64)).components(separatedBy: .init(charactersIn: "/\\:")).joined(separator: "_")
    ) { result in
      document = nil
      switch result {
      case .success: status = "已导出"
      case .failure(let error):
        if (error as NSError).code != NSUserCancelledError { status = "导出失败，请重试" }
      }
    }.onDisappear { action?.cancel() }
  }

  private func perform(failure: String, operation: @escaping @MainActor () async throws -> Void) {
    guard action == nil else { return }
    status = nil
    action = Task {
      defer { action = nil }
      do { try await operation() }
      catch { if !Task.isCancelled { status = failure } }
    }
  }
}
