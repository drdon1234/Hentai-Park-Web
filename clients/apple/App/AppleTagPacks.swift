import ParkCore
import SwiftUI

@MainActor struct AppleTagPacks: View {
  let session: ProgressSession
  let library: SourceLibrary
  let initialLocale: String
  let tr: (String) -> String
  @Environment(\.dismiss) private var dismiss
  @State private var inventory: TagPackInventory?
  @State private var selected = ""
  @State private var locale = ""
  @State private var available = [OfficialTagPack]()
  @State private var localVersion: String?
  @State private var message: String?
  @State private var removing = false
  @State private var operation: Task<Void,Never>?
  @State private var revision = 0

  private var remote: Set<TagPackKey> { Set(available.map { TagPackKey(source: $0.source,locale: $0.locale) }) }
  private var all: Set<TagPackKey> { (inventory?.bundled ?? []).union(inventory?.installed ?? []).union(remote) }
  private var sites: [String] { Set(all.map(\.source)).sorted() }
  private var locales: [String] { all.filter { $0.source == selected }.map(\.locale).sorted() }
  private var key: TagPackKey { TagPackKey(source: selected,locale: locale) }
  private func language(_ id: String) -> String {
    Locale(identifier: id).localizedString(forIdentifier: id) ?? id
  }
  private func normalize() {
    if !sites.contains(selected) { selected = sites.first ?? "" }
    if !locales.contains(locale) { locale = locales.contains(initialLocale) ? initialLocale : locales.first ?? "" }
  }
  private func run(_ action: @escaping @MainActor () async throws -> Void) {
    guard operation == nil else { return }
    message = nil
    operation = Task {
      defer { operation = nil }
      do {
        try await action()
        let updated = try await session.tagPacks(library)
        try Task.checkCancellation()
        inventory = updated; normalize(); revision += 1
      } catch { if !Task.isCancelled { message = tr("翻译包安装失败，请重试") } }
    }
  }
  private func refresh() async throws -> DictionaryIndex {
    let index = try await session.dictionaryIndex(library)
    available = try JSONDecoder().decode([OfficialTagPack].self,from: Data(index.packages().utf8))
    return index
  }
  private func bulk(_ targetLocale: String) async throws {
    var failures = [String]()
    var index: DictionaryIndex?
    do { index = try await refresh() } catch {
      try Task.checkCancellation()
      failures.append(tr("官方词包网络请求失败"))
    }
    let published: Set<TagPackKey> = index == nil ? [] : remote
    let targets = (inventory?.bundled ?? []).union(published).filter { $0.locale == targetLocale }.sorted { $0.source < $1.source }
    var count = 0
    for (i,target) in targets.enumerated() {
      try Task.checkCancellation()
      message = "\(i + 1) / \(targets.count)"
      do {
        try await session.installTagPack(target,library: library,index: published.contains(target) ? index : nil)
        count += 1
      } catch { try Task.checkCancellation(); failures.append(target.source) }
    }
    message = ([tr("已安装 {count} 个词包").replacingOccurrences(of:"{count}",with:String(count))] +
      (failures.isEmpty ? [] : [tr("未完成：{items}").replacingOccurrences(of:"{items}",with:failures.joined(separator:", "))])).joined(separator:"\n")
  }
  var body: some View {
    VStack(alignment: .leading) {
      HStack { Text(tr("翻译包管理")).font(.headline); Spacer(); Button(tr("返回")) { dismiss() } }
      List {
        if !sites.isEmpty {
          Picker(tr("站点"),selection:$selected) {
            ForEach(sites,id:\.self) { site in Text(inventory?.names[site] ?? site).tag(site) }
          }
          Picker(tr("语言"),selection:$locale) {
            ForEach(locales,id:\.self) { id in Text(language(id)).tag(id) }
          }
          Text(tr("词包优先显示译文；未命中时按内容翻译开关处理，搜索与屏蔽仍使用原文")).font(.caption)
          if inventory?.bundled.contains(key) == true {
            let installed = inventory?.installed.contains(key) == true
            Button(tr(installed ? "翻译包已安装" : "安装内置词包")) {
              let target = key
              run { try await session.installTagPack(target,library:library,index:nil); message = tr("翻译包已安装") }
            }.disabled(installed)
          }
          if let entry = available.first(where: { $0.source == selected && $0.locale == locale }) {
            Button(tr("从官方仓库下载更新")) {
              let target = key
              run { let index = try await refresh(); try await session.installTagPack(target,library:library,index:index); message = tr("翻译包已安装") }
            }.disabled(localVersion.map { entry.version.compare($0,options:.numeric) != .orderedDescending } ?? false)
          }
          if inventory?.installed.contains(key) == true {
            Button(tr("移除词库"),role:.destructive) { removing = true }
          }
          Button(tr("下载全部站点的 {language} 词包").replacingOccurrences(of:"{language}",with:language(locale))) {
            let lang = locale; run { try await bulk(lang) }
          }
        } else if inventory != nil { Text(tr("暂无可用词包，可使用内容翻译")) }
        Button(tr("检查官方可用词包")) { run { _ = try await refresh(); message = tr("已更新可用词包列表") } }
      }.disabled(operation != nil)
      if operation != nil { ProgressView() }
      if let message { Text(message).font(.caption) }
    }.padding()
    #if os(macOS)
    .frame(minWidth: 400, minHeight: 320)
    #endif
    .task { run {} }
    .task(id: "\(selected)/\(locale)/\(revision)") {
      let target = key; guard !target.source.isEmpty && !target.locale.isEmpty else { return }
      let version = try? await session.dictionaryVersion(target)
      if !Task.isCancelled { localVersion = version }
    }
    .onChange(of:selected) { _ in normalize() }
    .onReceive(NotificationCenter.default.publisher(for: .tagPackChanged)) { _ in
      if operation == nil { run {} }
    }
    .onDisappear { operation?.cancel() }
    .alert(tr("移除词库"),isPresented:$removing) {
      Button(tr("取消"),role:.cancel) {}
      Button(tr("移除词库"),role:.destructive) {
        let target = key
        run { try await session.removeTagPack(target); message = tr("词库已移除") }
      }
    } message: {
      Text(tr("移除 {site} 的 {language} 词库？").replacingOccurrences(of:"{site}",with:inventory?.names[selected] ?? selected)
        .replacingOccurrences(of:"{language}",with:language(locale)))
    }
  }
}
