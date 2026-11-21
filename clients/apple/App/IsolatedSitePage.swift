import CryptoKit
import ParkCore
import SwiftUI
import WebKit

#if os(iOS)
  typealias PlatformWebView = UIViewRepresentable
#else
  typealias PlatformWebView = NSViewRepresentable
#endif

@MainActor struct IsolatedSitePage: PlatformWebView {
  let policy: SitePagePolicy
  var originalSite: String? = nil
  var controls: GamePageControls? = nil
  var suspended = false
  @Binding var ready: Bool
  @Binding var failed: Bool
  func makeCoordinator() -> Delegate {
    Delegate(policy: policy, originalSite: originalSite, controls: controls) { loaded, error in
      ready = loaded
      failed = error
    }
  }
  #if os(iOS)
    func makeUIView(context: Context) -> WKWebView { context.coordinator.open() }
    func updateUIView(_ view: WKWebView, context: Context) {
      context.coordinator.setSuspended(suspended, view)
    }
    static func dismantleUIView(_ view: WKWebView, coordinator: Delegate) {
      coordinator.close(view)
    }
  #else
    func makeNSView(context: Context) -> WKWebView { context.coordinator.open() }
    func updateNSView(_ view: WKWebView, context: Context) {
      context.coordinator.setSuspended(suspended, view)
    }
    static func dismantleNSView(_ view: WKWebView, coordinator: Delegate) {
      coordinator.close(view)
    }
  #endif

  @MainActor final class Delegate: NSObject, WKNavigationDelegate {
    let policy: SitePagePolicy
    let originalSite: String?
    let controls: GamePageControls?
    let state: (Bool, Bool) -> Void
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
      UUID().uuidString, isDirectory: true)
    private var active = true
    private var suspended = false
    func setSuspended(_ value: Bool, _ view: WKWebView) {
      guard active, value != suspended else { return }
      suspended = value
      if value { controls?.releaseKeys() }
      view.setAllMediaPlaybackSuspended(value, completionHandler: nil)
    }
    init(policy: SitePagePolicy, originalSite: String?, controls: GamePageControls?, state: @escaping (Bool, Bool) -> Void)
    {
      self.originalSite = originalSite
      self.policy = policy
      self.controls = controls
      self.state = state
    }
    func open() -> WKWebView {
      let config = WKWebViewConfiguration()
      if case .game(let game) = policy, #available(iOS 17, macOS 14, *),
        let id = game.websiteDataIdentifier
      {
        config.websiteDataStore = WKWebsiteDataStore(forIdentifier: id)
      } else if case .confirmation = policy, #available(iOS 17, macOS 14, *), let originalSite {
        // 关联到站点的原站窗口共用同一分区。临时验证和无痕窗口不传入站点标识。
        let hash = SHA256.hash(data: Data(("park_site_" + originalSite).utf8))
        let bytes = Array(hash.prefix(16))
        let id = UUID(uuid: (bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                             bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]))
        config.websiteDataStore = WKWebsiteDataStore(forIdentifier: id)
      } else {
        config.websiteDataStore = .nonPersistent()
      }
      #if os(iOS)
        config.allowsInlineMediaPlayback = true
      #endif
      config.preferences.javaScriptCanOpenWindowsAutomatically = false
      config.mediaTypesRequiringUserActionForPlayback = .all
      let view = WKWebView(frame: .zero, configuration: config)
      view.navigationDelegate = self
      Task { await prepare(view) }
      return view
    }
    private func prepare(_ view: WKWebView) async {
      guard active else { return }
      guard let url = policy.url, policy.permits(url) else {
        state(false, true)
        return
      }
      if case .game(let game) = policy {
        guard #available(iOS 17, macOS 14, *), let id = game.websiteDataIdentifier,
          view.configuration.websiteDataStore.identifier == id
        else {
          state(false, true)
          return
        }
      }
      do {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        guard let store = WKContentRuleListStore(url: directory) else {
          throw SiteReadError.malformed
        }
        let json = try policy.resourceRules()
        let compiled: WKContentRuleList = try await withCheckedThrowingContinuation { result in
          store.compileContentRuleList(forIdentifier: "resources", encodedContentRuleList: json) {
            list, error in
            if let list {
              result.resume(returning: list)
            } else {
              result.resume(throwing: error ?? SiteReadError.malformed)
            }
          }
        }
        guard active else { return }
        view.configuration.userContentController.add(compiled)
        view.load(URLRequest(url: url))
      } catch { if active { state(false, true) } }
    }
    func close(_ view: WKWebView) {
      active = false
      controls?.detach(view)
      view.navigationDelegate = nil
      view.stopLoading()
      view.pauseAllMediaPlayback(completionHandler: nil)
      view.configuration.userContentController.removeAllContentRuleLists()
    }
    deinit { try? FileManager.default.removeItem(at: directory) }
    func webView(
      _ view: WKWebView, decidePolicyFor action: WKNavigationAction,
      decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
    ) {
      guard let frame = action.targetFrame, !action.shouldPerformDownload else {
        decisionHandler(.cancel)
        return
      }
      decisionHandler(
        policy.permits(action.request.url, subframe: !frame.isMainFrame) ? .allow : .cancel)
    }
    func webView(
      _ view: WKWebView, decidePolicyFor response: WKNavigationResponse,
      decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void
    ) {
      if !response.isForMainFrame, response.response.url?.absoluteString == "about:blank",
        policy.permits(response.response.url, subframe: true)
      {
        decisionHandler(.allow)
        return
      }
      let bad =
        (response.response as? HTTPURLResponse).map { !(200..<400).contains($0.statusCode) } ?? true
      let permit =
        policy.permits(response.response.url, subframe: !response.isForMainFrame)
        && !bad && response.canShowMIMEType
      if !permit && response.isForMainFrame { state(false, true) }
      decisionHandler(permit ? .allow : .cancel)
    }
    func webView(_ view: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
      controls?.detach(view)
      state(false, false)
    }
    func webView(_ view: WKWebView, didFinish navigation: WKNavigation!) {
      if active && policy.permits(view.url) { controls?.attach(view) }
      state(policy.permits(view.url), !policy.permits(view.url))
    }
    func webView(_ view: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
      state(false, true)
    }
    func webView(
      _ view: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
      withError error: Error
    ) { state(false, true) }
    func webViewWebContentProcessDidTerminate(_ view: WKWebView) {
      controls?.detach(view)
      state(false, true)
    }
  }
}
