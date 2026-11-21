import ParkCore
import SwiftUI

@MainActor struct SiteGame: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: String
  let work: String
  let tr: (String) -> String
  let back: () -> Void
  @Environment(\.privacyObscured) private var obscured
  @Environment(\.scenePhase) private var phase
  @StateObject private var controls = GamePageControls()
  @State private var target: OnlineGame?
  @State private var problem: String?
  @State private var retry = 0
  @State private var ready = false
  @State private var failed = false
  @State private var keysVisible = false

  var body: some View {
    VStack {
      Button(tr("返回"), action: back)
      if let problem {
        Text(tr(problem))
        if problem == "游戏加载失败" { Button(tr("重试"), action: reload) }
      } else if let target {
        HStack {
          GameSaveMenu(controls: controls, session: session, source: site, target: target, tr: tr)
          Button(tr(keysVisible ? "隐藏虚拟按键" : "显示虚拟按键")) {
            if keysVisible { controls.releaseKeys() }
            keysVisible.toggle()
          }
        }.disabled(!ready || failed || obscured || phase != .active)
        IsolatedSitePage(
          policy: .game(target), controls: controls, suspended: obscured || phase != .active,
          ready: $ready, failed: $failed
        )
        .allowsHitTesting(!obscured && phase == .active)
        if failed {
          Button(tr("游戏加载失败") + " · " + tr("重试"), action: reload)
        } else if !ready {
          ProgressView()
        }
        if keysVisible && ready && !failed && !obscured && phase == .active {
          LazyVGrid(columns: [GridItem(.adaptive(minimum: 48))]) {
            ForEach(GameKey.allCases, id: \.self) { key in
              GameKeyButton(label: tr(key.label)) { down in controls.press(key, down: down) }
            }
          }
          .onDisappear { controls.releaseKeys() }
        }
      } else {
        ProgressView()
      }
    }.task(id: retry) {
      guard #available(iOS 17, macOS 14, *) else {
        problem = "当前设备暂不支持在线游戏"
        return
      }
      do {
        let game = try await session.htmlGame(library, source: site, work: work)
        try Task.checkCancellation()
        guard game.websiteDataIdentifier != nil, URL(string: game.url()) != nil else {
          throw SiteReadError.malformed
        }
        target = game
      } catch SourceFailure.Unsupported {
        if !Task.isCancelled { problem = "当前设备暂不支持在线游戏" }
      } catch {
        if !Task.isCancelled { problem = "游戏加载失败" }
      }
    }
  }

  private func reload() {
    target = nil
    problem = nil
    ready = false
    failed = false
    retry += 1
  }
}

private struct GameKeyButton: View {
  let label: String
  let press: (Bool) -> Void
  @GestureState private var held = false
  var body: some View {
    Text(label).frame(minWidth: 44, minHeight: 44)
      .background(held ? Color.accentColor.opacity(0.3) : Color.secondary.opacity(0.15))
      .cornerRadius(6).contentShape(Rectangle())
      .gesture(
        DragGesture(minimumDistance: 0).updating($held) { _, value, _ in value = true }
          .onChanged { _ in press(true) }.onEnded { _ in press(false) }
      )
      .onChange(of: held) { if !$0 { press(false) } }
      .accessibilityAddTraits(.isButton)
      .accessibilityAction {
        press(true)
        press(false)
      }
  }
}
