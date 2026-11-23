import ParkCore
import SwiftUI
import WebKit

private enum VisitResult: Error { case dismissed, reviewed }
func siteMessage(_ error: Error) -> String {
  switch error {
  case VisitResult.dismissed: return "已取消"
  case VisitResult.reviewed: return "请核对原站操作结果"
  case SourceFailure.LoginRequired: return "这个站点需要登录后才能浏览"
  case SourceFailure.Cancelled, is CancellationError: return "请求已取消"
  case SourceFailure.Storage: return "无法读取文件，请重试"
  case SiteReadError.malformed, is DecodingError: return "站点返回了无效结果"
  default: return "站点加载失败，请重试"
  }
}

@MainActor final class SitePrompt: ObservableObject {
  struct Visit: Identifiable {
    let id: UUID
    let read: ManualRead
  }
  @Published var visit: Visit?
  private var continuation: CheckedContinuation<ManualRead, Error>?
  func perform<T>(_ call: (ManualRead?) async throws -> T) async throws -> T {
    do { return try await call(nil) } catch SiteReadError.confirmation(let read) {
      let id = UUID()
      let choice: ManualRead = try await withTaskCancellationHandler {
        try Task.checkCancellation()
        return try await withCheckedThrowingContinuation { response in
          continuation = response
          visit = Visit(id: id, read: read)
        }
      } onCancel: {
        Task { @MainActor in self.finish(id, replay: false) }
      }
      try Task.checkCancellation()
      return try await call(choice)
    }
  }
  func finish(_ id: UUID, replay: Bool, reviewed: Bool = false) {
    guard let current = visit, current.id == id, let response = continuation else { return }
    continuation = nil
    visit = nil
    if replay && current.read.canReadAgain() {
      response.resume(returning: current.read)
    } else {
      response.resume(throwing: reviewed ? VisitResult.reviewed : VisitResult.dismissed)
    }
  }
}

extension View {
  @MainActor func siteConfirmation(_ prompt: SitePrompt, tr: @escaping (String) -> String)
    -> some View
  {
    modifier(SiteVisitSheet(prompt: prompt, tr: tr))
  }
}

@MainActor private struct SiteVisitSheet: ViewModifier {
  @ObservedObject var prompt: SitePrompt
  let tr: (String) -> String
  @Environment(\.privacyObscured) var obscured
  func body(content: Content) -> some View {
    content.sheet(
      item: Binding(
        get: { prompt.visit },
        set: { value in
          if value == nil, let visit = prompt.visit { prompt.finish(visit.id, replay: false) }
        })
    ) { visit in
      SiteVisit(
        visit: visit, tr: tr,
        finish: { replay, reviewed in prompt.finish(visit.id, replay: replay, reviewed: reviewed) }
      )
      .opacity(obscured ? 0 : 1).accessibilityHidden(obscured)
    }.onChange(of: obscured) { hidden in
      if hidden, let visit = prompt.visit { prompt.finish(visit.id, replay: false) }
    }
  }
}

@MainActor private struct SiteVisit: View {
  let visit: SitePrompt.Visit
  let tr: (String) -> String
  let finish: (Bool, Bool) -> Void
  @State private var opened = false
  @State private var ready = false
  @State private var failed = false
  var body: some View {
    VStack {
      if let url = URL(string: visit.read.url()), let host = url.host {
        Text(host).font(.headline)
        if opened {
          IsolatedSitePage(policy: .confirmation(url), ready: $ready, failed: $failed)
          if failed { Text(tr("站点加载失败，请重试")) }
          let replay = visit.read.canReadAgain()
          Button(tr(replay ? "重新读取原内容" : "完成")) { finish(replay, true) }.disabled(!ready || failed)
        } else {
          Button(tr("打开原站")) { opened = true }
        }
      }
      Button(tr("取消")) { finish(false, false) }
    }.padding()
  }
}
