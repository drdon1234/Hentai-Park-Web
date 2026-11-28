import SwiftUI
import ParkCore

private struct AutomaticContentTranslation: EnvironmentKey { static let defaultValue = false }
extension EnvironmentValues {
    var autoTranslateContent: Bool {
        get { self[AutomaticContentTranslation.self] }
        set { self[AutomaticContentTranslation.self] = newValue }
    }
}

@MainActor final class TranslationPage: ObservableObject {
    @Published var override: Bool?
    @Published private(set) var revision = 0
    @Published private(set) var failed = false
    func retry() { failed = false; revision += 1 }
    func reportFailure() { failed = true }
    func toggle(automatic: Bool) {
        failed = false
        override = !(override ?? automatic)
    }
}

@MainActor struct TranslationControls: View {
    @ObservedObject var page: TranslationPage
    let tr: (String) -> String
    @Environment(\.autoTranslateContent) private var automatic
    var body: some View {
        Menu {
            Button(tr((page.override ?? automatic) ? "显示原文（仅当前页面）" : "翻译当前页面")) { page.toggle(automatic: automatic) }
            if page.failed { Button(tr("翻译失败，请重试")) { page.retry() } }
        } label: { Label(tr("内容翻译"), systemImage: "character.bubble") }
    }
}

@MainActor struct RemoteText: View {
    let original: String
    let dictionary: String?
    let session: ProgressSession
    let library: SourceLibrary
    let locale: String
    @ObservedObject var page: TranslationPage
    @Environment(\.autoTranslateContent) private var automatic
    @Environment(\.privacyObscured) private var obscured
    @State private var result: String?
    @State private var received: TranslationKey?
    private struct TranslationKey: Hashable {
        let source, locale: String
    }
    private struct TranslationRequest: Hashable {
        let key: TranslationKey
        let active: Bool
        let revision: Int
    }
    private var request: TranslationRequest {
        TranslationRequest(key: TranslationKey(source: original, locale: locale),
                           active: (page.override ?? automatic) && !obscured && dictionary == nil, revision: page.revision)
    }
    var body: some View {
        let showing = page.override == false ? original : dictionary ?? (request.active && received == request.key ? result : nil) ?? original
        Text(showing).task(id: request) {
            let target = request
            guard target.active, received != target.key else { return }
            do {
                let translated = try await session.translateVisibleText(target.key.source, locale: target.key.locale, library: library)
                try Task.checkCancellation()
                result = translated; received = target.key
            } catch { if !Task.isCancelled { page.reportFailure() } }
        }
    }
}
