import ParkCore
import SwiftUI

@MainActor struct SiteSettingsPage: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let locale: String
  let tr: (String) -> String
  var changed: (() -> Void)? = nil
  var signedIn: (() -> Void)? = nil
  @Environment(\.dismiss) private var dismiss
  @Environment(\.privacyObscured) private var obscured
  @State private var settings: SiteSettings?
  @State private var state: SiteOptionState?
  @State private var account: SiteAccount?
  @State private var signingIn = false
  @State private var confirmingSignOut = false
  @State private var values = [String: String]()
  @State private var editing: PluginOption?
  @State private var draft = ""
  @State private var error: String?
  @State private var attempt = 0
  @State private var operation: Task<Void, Never>?

  var body: some View {
    VStack(alignment: .leading) {
      HStack {
        Button(tr("返回")) {
          if signingIn { signingIn = false; values = [:] }
          else if editing != nil { editing = nil; draft = "" }
          else { dismiss() }
        }
        Text(settings?.name ?? site.name ?? site.key).font(.headline)
      }.padding(.horizontal)
      if let error {
        Text(tr(error)).padding(.horizontal)
        if settings == nil { Button(tr("重试")) { attempt += 1 }.padding(.horizontal) }
      }
      if let settings {
        if signingIn, let form = settings.login { login(form, settings) }
        else if let editing { editor(editing, settings) }
        else { overview(settings) }
      } else if error == nil { ProgressView().padding() }
      if operation != nil { ProgressView().padding() }
    }.padding(.vertical).disabled(operation != nil).interactiveDismissDisabled(operation != nil)
    #if os(macOS)
    .frame(minWidth: 420, idealWidth: 520, minHeight: 320, idealHeight: 420)
    #endif
    .opacity(obscured ? 0 : 1).accessibilityHidden(obscured)
    .task(id: attempt) {
      do {
        let result = try await session.siteSettings(library, source: site.key, locale: locale)
        try Task.checkCancellation()
        settings = result; state = result.state; account = result.account; error = nil
      } catch { if !Task.isCancelled { self.error = siteMessage(error) } }
    }.onDisappear {
      operation?.cancel(); operation = nil; values = [:]; draft = ""
    }.alert(tr("退出登录"), isPresented: $confirmingSignOut) {
      Button(tr("取消"), role: .cancel) {}
      Button(tr("确定"), role: .destructive) {
        perform {
          try await session.signOut(library, source: site.key)
          account = nil; changed?()
        }
      }
    }
  }

  private func overview(_ settings: SiteSettings) -> some View {
    List {
      if let form = settings.login {
        Section(header: Text(tr("站点账号"))) {
          if let account, account.loggedIn {
            Text(account.name.isEmpty ? tr("已登录") : account.name)
            Button(tr("退出登录")) { confirmingSignOut = true }
          } else if !(form.fields ?? []).isEmpty {
            Button(tr("登录")) { error = nil; values = [:]; signingIn = true }
          }
        }
      }
      if !settings.options.isEmpty {
        Section(header: Text(tr("站点设置"))) {
          ForEach(settings.options) { option in
            let label = settings.localized(option.label ?? option.key)
            let value = state?.values[option.key] ?? option.defaultValue
            if option.type == "switch" {
              Toggle(label, isOn: Binding(get: { value == .flag(true) }, set: { save(option, .flag($0)) }))
                .disabled(option.key == "managedAccount" && settings.login?.managedAccount == true)
            } else {
              Button {
                error = nil; draft = option.isPrivate ? "" : value?.fieldText ?? ""; editing = option
              } label: {
                VStack(alignment: .leading) {
                  Text(label)
                  if option.isPrivate {
                    Text(state?.configured.contains(option.key) == true ? "••••••" : tr("未设置")).font(.caption)
                  } else if let choice = option.values?.first(where: { $0.value == value }) {
                    Text(settings.localized(choice.label)).font(.caption)
                  } else if let text = value?.fieldText, !text.isEmpty { Text(text).font(.caption) }
                }
              }
            }
          }
        }
      }
    }
  }

  private func login(_ form: SiteLoginForm, _ settings: SiteSettings) -> some View {
    Form {
      if let hint = form.hint, !hint.isEmpty { Text(settings.localized(hint)) }
      ForEach(form.fields ?? []) { field in
        let binding = Binding<String>(get: { values[field.key] ?? "" }, set: { values[field.key] = $0 })
        let name = settings.localized(field.label ?? field.key)
        if field.secret == true || ["cookies","session"].contains(form.type ?? "") {
          SecureField(name, text: binding)
        } else { TextField(name, text: binding) }
      }
      Button(settings.localized(form.title ?? "登录")) {
        guard form.accepts(values) else { return }
        let submitted = values
        values = [:]
        perform {
          let result = try await session.signIn(library, source: site.key, values: submitted)
          account = result; signingIn = false
          if result.loggedIn { changed?(); signedIn?() }
        }
      }.disabled(!form.accepts(values))
    }
  }

  private func editor(_ option: PluginOption, _ settings: SiteSettings) -> some View {
    Form {
      let title = settings.localized(option.label ?? option.key)
      if option.type == "select" {
        Section(header: Text(title)) {
          ForEach(option.values ?? [], id: \.value) { choice in
            Button(settings.localized(choice.label)) { save(option, choice.value) }
          }
        }
      } else {
        if option.isPrivate { SecureField(title, text: $draft) }
        else { TextField(title, text: $draft) }
        Button(tr("保存")) {
          if option.type == "number" {
            guard let n = Decimal(string: draft, locale: Locale(identifier: "en_US_POSIX")) else { error = "保存失败，请重试"; return }
            save(option, .number(n))
          } else { save(option, .text(draft)) }
        }
      }
    }
  }

  private func save(_ option: PluginOption, _ value: PluginOptionValue) {
    perform {
      state = try await session.changeSiteOption(library, source: site.key, key: option.key, value: value)
      editing = nil; draft = ""; changed?()
    }
  }

  private func perform(_ action: @escaping @MainActor () async throws -> Void) {
    guard operation == nil else { return }
    error = nil
    operation = Task {
      defer { operation = nil }
      do { try await action() }
      catch { if !Task.isCancelled { self.error = siteMessage(error) } }
    }
  }
}

private extension PluginOption {
  var isPrivate: Bool { secret == true || type == "password" }
}
private extension PluginOptionValue {
  var fieldText: String? {
    switch self {
    case .text(let v): return v
    case .number(let v): return NSDecimalNumber(decimal: v).stringValue
    case .flag, .list, .none: return nil
    }
  }
}
